//! The truth ingest: the one TCP listener in the program (0.9.0).
//!
//! ## Why this exists at all
//!
//! `crate::server`'s first line is "No TCP, ever (DESIGN §8) — the transport is
//! the access control". That rule is not being relaxed; it is being paid for.
//! The client on the other end of this socket is Discord's renderer process
//! running a Vencord plugin, and a renderer cannot open a unix socket. HTTP to
//! loopback is the only door it has.
//!
//! So the listener is built to give back what the unix socket gave for free:
//!
//! * **Bound to `127.0.0.1` and nothing else.** Not `0.0.0.0`, not a
//!   configurable address — there is no config key for the interface, because
//!   the only correct value is the one it is hard-coded to. Every accepted
//!   connection's peer address is checked again anyway, and a non-loopback peer
//!   is hung up on without being read.
//! * **A bearer token in a 0600 file**, generated once, in place of the
//!   socket's 0600 mode. No token, wrong token, or no `Authorization` header at
//!   all is `401` and the body is never read.
//! * **Off by default.** `[truth].enabled = false` until somebody turns it on.
//! * **Write-only, and only into two tables.** There is no `GET` that returns
//!   any recording, any transcript or any name. The whole API is three routes.
//!
//! ## The wire
//!
//! ```text
//! POST /v1/discord/speaking   NDJSON  {t_ms, user_id, speaking, name, channel_id}
//! POST /v1/discord/voice      NDJSON  {t_ms, ev, user_id, name, channel_id, self_mute?, self_deaf?}
//! GET  /v1/health                     {"ok": true, ...}
//! ```
//!
//! `t_ms` is the plugin's `Date.now()` — wall-clock UNIX milliseconds. That is
//! the same clock `segments.t_start_ns` is on: the pipeline derives a segment's
//! stamp from a UTC anchor (`crate::clock::Anchor`), so both sides of the
//! comparison are UTC epoch time on one machine and no conversion is needed.
//! Comparing a wall clock against a monotonic one would have been silently
//! wrong, which is why it is said out loud here.
//!
//! Anything not those three routes is `204`, deliberately: the plugin is
//! fire-and-forget and a 404 it cannot act on is noise. A body over
//! [`MAX_BODY`] is `413` and is dropped rather than truncated — half a batch of
//! NDJSON is not a smaller batch, it is a corrupted one.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::clock::utc_now_ns;
use crate::store::Store;
use crate::truth::TruthStats;

/// The largest request body the ingest will read. A 500 ms batch of speaking
/// edges is a few hundred bytes; a megabyte is a corruption guard, not a
/// working limit.
pub const MAX_BODY: usize = 1024 * 1024;

/// Read and write timeouts on an accepted connection. The client is on the
/// same machine; anything slower than this is not a client.
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// The origin the Discord renderer posts from. Sent back on `Access-Control-
/// Allow-Origin` so the browser's preflight passes, and **only** this one:
/// `*` would let any page the user has open post into their recordings.
pub const ALLOWED_ORIGIN: &str = "https://discord.com";

// ---------------------------------------------------------------------------
// the token
// ---------------------------------------------------------------------------

/// Read the token at `path`, creating one if there is none.
///
/// 32 bytes of `/dev/urandom` as lower-case hex. Not a UUID and not a hash of
/// anything guessable: the file is the only thing standing between a local
/// process and a write into the recordings, so it is the strongest thing that
/// can be had without a dependency.
pub fn token(path: &Path) -> Result<String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let trimmed = existing.trim().to_string();
        if !trimmed.is_empty() {
            return Ok(trimmed);
        }
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut raw = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom for the truth token")?
        .read_exact(&mut raw)
        .context("reading the truth token's randomness")?;
    let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();

    // Written 0600 *before* the secret goes in: creating the file world
    // readable and tightening it afterwards leaves a window, and a window is
    // all it takes.
    let tmp = path.with_extension("token.tmp");
    {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting {} to 0600", tmp.display()))?;
        f.write_all(hex.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.write_all(b"\n")?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("installing {}", path.display()))?;
    info!(path = %path.display(), "generated a truth ingest token");
    Ok(hex)
}

/// Constant-time-ish comparison. The token is not guessable in the first
/// place and this is a loopback socket, but a length-and-bytes compare that
/// short-circuits is free to avoid and free to explain.
fn token_matches(want: &str, got: &str) -> bool {
    if want.len() != got.len() {
        return false;
    }
    want.bytes()
        .zip(got.bytes())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

// ---------------------------------------------------------------------------
// the listener
// ---------------------------------------------------------------------------

/// A running ingest. Dropping the handle stops it.
pub struct Ingest {
    addr: SocketAddr,
    stopping: Arc<AtomicBool>,
    accept: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Ingest {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn shutdown(&self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        // Wake the blocking accept with a connection that goes nowhere.
        let _ = TcpStream::connect(self.addr);
        let handle = self.accept.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(h) = handle {
            let _ = h.join();
        }
        info!(addr = %self.addr, "truth ingest closed");
    }
}

impl Drop for Ingest {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Bind `127.0.0.1:port` and start serving. `port` 0 asks the OS for an
/// ephemeral one, which is what a test wants and what
/// [`Ingest::addr`] then reports.
pub fn serve(
    store: Arc<std::sync::Mutex<Store>>,
    stats: Arc<TruthStats>,
    token: String,
    port: u16,
) -> Result<Ingest> {
    if token.trim().is_empty() {
        bail!("refusing to start the truth ingest with an empty token");
    }
    let want = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener =
        TcpListener::bind(want).with_context(|| format!("binding the truth ingest on {want}"))?;
    let addr = listener.local_addr().context("reading the bound address")?;

    let stopping = Arc::new(AtomicBool::new(false));
    let accept_stopping = Arc::clone(&stopping);
    let token = Arc::new(token);
    let accept = std::thread::Builder::new()
        .name("recalld-truth-net".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if accept_stopping.load(Ordering::SeqCst) {
                    break;
                }
                let stream = match stream {
                    Ok(s) => s,
                    Err(e) => {
                        if accept_stopping.load(Ordering::SeqCst) {
                            break;
                        }
                        warn!("truth ingest accept failed: {e}");
                        continue;
                    }
                };
                // Belt and braces over the loopback bind: a peer that is not
                // loopback is hung up on before a single byte is read.
                match stream.peer_addr() {
                    Ok(peer) if peer.ip().is_loopback() => {}
                    Ok(peer) => {
                        warn!(%peer, "refusing a non-loopback connection to the truth ingest");
                        continue;
                    }
                    Err(e) => {
                        debug!("could not read a truth ingest peer address: {e}");
                        continue;
                    }
                }
                let store = Arc::clone(&store);
                let stats = Arc::clone(&stats);
                let token = Arc::clone(&token);
                if let Err(e) = std::thread::Builder::new()
                    .name("recalld-truth-conn".into())
                    .spawn(move || {
                        if let Err(e) = serve_one(&store, &stats, &token, stream) {
                            debug!("a truth ingest connection ended: {e:#}");
                        }
                    })
                {
                    warn!("could not spawn a truth ingest thread: {e}");
                }
            }
        })
        .context("spawning the truth ingest accept thread")?;

    info!(%addr, "truth ingest listening (loopback only, bearer token)");
    Ok(Ingest {
        addr,
        stopping,
        accept: std::sync::Mutex::new(Some(accept)),
    })
}

/// What one request turned into.
#[derive(Debug, PartialEq, Eq)]
enum Reply {
    NoContent,
    Health,
    Unauthorized,
    TooLarge,
    BadRequest,
}

impl Reply {
    fn status(&self) -> &'static str {
        match self {
            Reply::NoContent => "204 No Content",
            Reply::Health => "200 OK",
            Reply::Unauthorized => "401 Unauthorized",
            Reply::TooLarge => "413 Payload Too Large",
            Reply::BadRequest => "400 Bad Request",
        }
    }
}

fn serve_one(
    store: &Arc<std::sync::Mutex<Store>>,
    stats: &TruthStats,
    token: &str,
    stream: TcpStream,
) -> Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok();
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok();
    let mut out = stream.try_clone().context("cloning the ingest socket")?;
    let mut reader = BufReader::new(stream);

    // ---- request line ----
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();

    // ---- headers ----
    let mut length = 0usize;
    let mut authorization: Option<String> = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        let Some((name, value)) = h.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse().unwrap_or(0),
            "authorization" => authorization = Some(value.to_string()),
            _ => {}
        }
    }

    // A preflight is answered before anything else: the browser sends no
    // Authorization header on it, so checking the token first would make every
    // real request unreachable.
    if method == "OPTIONS" {
        return respond(&mut out, Reply::NoContent, None);
    }

    if method == "GET" && path == "/v1/health" {
        // Health is behind the token too. "Is recalld up" is not a secret, but
        // an unauthenticated endpoint is one more thing to reason about and
        // the plugin has the token anyway.
        if !authorized(authorization.as_deref(), token) {
            return respond(&mut out, Reply::Unauthorized, None);
        }
        let (spans, open) = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            guard.truth_span_counts().unwrap_or((0, 0))
        };
        let body = serde_json::json!({
            "ok": true,
            "service": "nx-recall",
            "proto": crate::bus::PROTO,
            "spans": spans,
            "open": open,
        });
        return respond(&mut out, Reply::Health, Some(&body.to_string()));
    }

    if method != "POST" {
        return respond(&mut out, Reply::NoContent, None);
    }
    if !authorized(authorization.as_deref(), token) {
        stats.rejected.fetch_add(1, Ordering::Relaxed);
        return respond(&mut out, Reply::Unauthorized, None);
    }
    if length > MAX_BODY {
        stats.rejected.fetch_add(1, Ordering::Relaxed);
        respond(&mut out, Reply::TooLarge, None)?;
        // Closing with the body still arriving makes the kernel answer the
        // rest of it with RST, and a reset discards the 413 sitting in the
        // client's receive buffer — so the client saw a dropped connection,
        // not a refusal (the test for this failed one run in three). Say
        // "done writing" first, then discard what is in flight, bounded: a
        // few times the limit is what a client that stops on EOF can still
        // have in the pipe, and past that it is not a client and gets the
        // reset after all. Reading it all "to be polite" would be the denial
        // of service.
        let _ = out.shutdown(std::net::Shutdown::Write);
        let mut sink = [0u8; 64 * 1024];
        let mut drained = 0usize;
        while drained < 4 * MAX_BODY {
            match reader.read(&mut sink) {
                Ok(0) | Err(_) => break,
                Ok(n) => drained += n,
            }
        }
        return Ok(());
    }

    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body)?;
    }
    let body = match String::from_utf8(body) {
        Ok(s) => s,
        Err(_) => {
            stats.rejected.fetch_add(1, Ordering::Relaxed);
            return respond(&mut out, Reply::BadRequest, None);
        }
    };

    let kind = match path.split('?').next().unwrap_or("") {
        "/v1/discord/speaking" => Some(Line::Speaking),
        "/v1/discord/voice" => Some(Line::Voice),
        // Everything else is 204: the plugin cannot act on a 404 and a route
        // that does not exist yet is a newer plugin talking to an older daemon.
        _ => None,
    };
    let Some(kind) = kind else {
        return respond(&mut out, Reply::NoContent, None);
    };

    let guard = store.lock().unwrap_or_else(|p| p.into_inner());
    let mut taken = 0u64;
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            // One malformed line does not poison a batch. It is counted and
            // skipped: the plugin retries the whole batch on a non-2xx, and
            // failing the batch would loop forever on one bad line.
            stats.rejected.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        match ingest_line(&guard, kind, &v) {
            Ok(true) => taken += 1,
            Ok(false) => {
                stats.rejected.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => {
                warn!("could not store a truth line: {e:#}");
                stats.rejected.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    drop(guard);
    match kind {
        Line::Speaking => stats.lines_speaking.fetch_add(taken, Ordering::Relaxed),
        Line::Voice => stats.lines_voice.fetch_add(taken, Ordering::Relaxed),
    };

    respond(&mut out, Reply::NoContent, None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Line {
    Speaking,
    Voice,
}

fn authorized(header: Option<&str>, token: &str) -> bool {
    let Some(h) = header else { return false };
    let Some(rest) = h
        .strip_prefix("Bearer ")
        .or_else(|| h.strip_prefix("bearer "))
    else {
        return false;
    };
    token_matches(token, rest.trim())
}

/// Store one NDJSON line. `Ok(false)` means the line was well-formed JSON but
/// not a line this endpoint understands — a shape mismatch, not a failure.
fn ingest_line(store: &Store, kind: Line, v: &Value) -> Result<bool> {
    let Some(t_ms) = v.get("t_ms").and_then(Value::as_i64) else {
        return Ok(false);
    };
    let Some(user_id) = v.get("user_id").and_then(Value::as_str) else {
        return Ok(false);
    };
    if user_id.is_empty() {
        return Ok(false);
    }
    let t_ns = t_ms.saturating_mul(1_000_000);
    let name = v
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(user_id);
    let channel_id = v.get("channel_id").and_then(Value::as_str);

    // Every line teaches us the account exists and what it is calling itself,
    // whichever endpoint it arrived on. A `leave` is still a sighting.
    store.upsert_discord_user(user_id, name, utc_now_ns())?;

    match kind {
        Line::Speaking => {
            let Some(speaking) = v.get("speaking").and_then(Value::as_bool) else {
                return Ok(false);
            };
            if speaking {
                store.truth_speaking_start(user_id, name, channel_id, t_ns)?;
            } else {
                store.truth_speaking_stop(user_id, t_ns)?;
            }
            Ok(true)
        }
        Line::Voice => {
            let ev = v.get("ev").and_then(Value::as_str).unwrap_or("");
            match ev {
                // Leaving the channel ends any ring that was still lit: a
                // client that disappears mid-word sends no stop, and without
                // this the span would run until the 30 s timeout and claim
                // speech that did not happen.
                "leave" => {
                    store.truth_speaking_stop(user_id, t_ns)?;
                    Ok(true)
                }
                "join" | "self" => Ok(true),
                _ => Ok(false),
            }
        }
    }
}

fn respond(out: &mut TcpStream, reply: Reply, body: Option<&str>) -> Result<()> {
    let body = body.unwrap_or("");
    let head = format!(
        "HTTP/1.1 {}\r\n\
         Content-Length: {}\r\n\
         Content-Type: application/json\r\n\
         Access-Control-Allow-Origin: {}\r\n\
         Access-Control-Allow-Headers: authorization, content-type\r\n\
         Access-Control-Allow-Methods: POST, GET, OPTIONS\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n",
        reply.status(),
        body.len(),
        ALLOWED_ORIGIN,
    );
    out.write_all(head.as_bytes())?;
    if !body.is_empty() {
        out.write_all(body.as_bytes())?;
    }
    out.flush()?;
    Ok(())
}

/// Where the token lives, for the CLI and for `truth.status`.
pub fn token_path(config_path: &Path) -> PathBuf {
    crate::config::truth_token_path(config_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bearer_header_is_read_case_insensitively_and_nothing_else_is_accepted() {
        assert!(authorized(Some("Bearer abc"), "abc"));
        assert!(authorized(Some("bearer abc"), "abc"));
        assert!(authorized(Some("Bearer  abc "), "abc"));
        assert!(!authorized(Some("Bearer abd"), "abc"));
        assert!(!authorized(Some("Basic abc"), "abc"));
        assert!(!authorized(Some("abc"), "abc"));
        assert!(!authorized(None, "abc"));
        // A prefix must not pass for the whole.
        assert!(!authorized(Some("Bearer ab"), "abc"));
        assert!(!authorized(Some("Bearer abcd"), "abc"));
    }

    #[test]
    fn the_token_file_is_generated_once_and_reused() {
        let dir = std::env::temp_dir().join(format!("nx-recall-truthtoken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("truth.token");

        let first = token(&path).expect("generating a token");
        assert_eq!(first.len(), 64, "32 bytes as hex");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the token file must not be readable by others");

        assert_eq!(token(&path).unwrap(), first, "a second call reuses it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_token_file_is_replaced_rather_than_honoured() {
        let dir = std::env::temp_dir().join(format!("nx-recall-truthempty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("truth.token");
        std::fs::write(&path, "   \n").unwrap();
        assert_eq!(token(&path).unwrap().len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_token_is_refused_rather_than_started_open() {
        let store = Arc::new(std::sync::Mutex::new(Store::open_in_memory().unwrap()));
        let started = serve(store, Arc::new(TruthStats::default()), "  ".into(), 0);
        let Err(err) = started else {
            panic!("an empty token must not start a listener");
        };
        assert!(err.to_string().contains("empty token"));
    }
}
