//! The control socket: a Unix domain socket, mode 0600, NDJSON both ways.
//!
//! No TCP, ever (DESIGN §8) — the transport is the access control, together
//! with the 0700 data directory. Each connection owns two threads: a reader
//! that parses requests and a writer that drains that client's bounded outbox.
//! Splitting them is what makes the "a slow client must never block the
//! pipeline" rule mechanical rather than aspirational: the pipeline's publish
//! only ever does a `try_send` into a channel, and a client that lets it fill
//! is disconnected.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use tracing::{debug, error, info, warn};

use crate::bus::{Client, PROTO};
use crate::proto::{self, Incoming};
use crate::service::Service;

/// A running socket server. Dropping the handle stops it.
pub struct Server {
    path: PathBuf,
    stopping: Arc<AtomicBool>,
    accept: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
    service: Arc<Service>,
}

impl Server {
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stop accepting, hang up on every client, and remove the socket file.
    pub fn shutdown(&self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        // Wake the blocking `accept` with a connection that goes nowhere.
        let _ = UnixStream::connect(&self.path);
        let handle = self.accept.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(h) = handle {
            let _ = h.join();
        }
        self.service.bus.hangup_all();
        let _ = std::fs::remove_file(&self.path);
        info!(path = %self.path.display(), "control socket closed");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Bind `path` and start serving. Fails if another daemon is already listening
/// there; a socket left behind by a crash is cleaned up instead.
pub fn serve(service: Arc<Service>, path: &Path) -> Result<Server> {
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        // The socket's own mode is not the only thing protecting it: on a
        // system without an XDG runtime dir the parent is ours to lock down.
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    // sockaddr_un caps unix socket paths at ~107 bytes; the raw bind error
    // ("path must be shorter than SUN_LEN") explains nothing, so say it here.
    if path.as_os_str().len() > 107 {
        anyhow::bail!(
            "socket path is {} bytes, over the unix limit of 107: {} — set a shorter \
             path via NXR_SOCKET or [socket].path",
            path.as_os_str().len(),
            path.display()
        );
    }
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            anyhow::bail!("another recalld is already listening on {}", path.display());
        }
        debug!(path = %path.display(), "removing a socket left by a previous run");
        std::fs::remove_file(path)
            .with_context(|| format!("removing the stale socket {}", path.display()))?;
    }

    let listener = UnixListener::bind(path)
        .with_context(|| format!("binding the control socket {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {} to 0600", path.display()))?;

    let stopping = Arc::new(AtomicBool::new(false));
    let accept_stopping = Arc::clone(&stopping);
    let accept_service = Arc::clone(&service);
    let accept = std::thread::Builder::new()
        .name("recalld-socket".into())
        .spawn(move || {
            for stream in listener.incoming() {
                if accept_stopping.load(Ordering::SeqCst) {
                    break;
                }
                match stream {
                    Ok(stream) => {
                        let service = Arc::clone(&accept_service);
                        if let Err(e) = std::thread::Builder::new()
                            .name("recalld-client".into())
                            .spawn(move || {
                                if let Err(e) = serve_client(service, stream) {
                                    debug!("client connection ended: {e:#}");
                                }
                            })
                        {
                            error!("could not spawn a client thread: {e}");
                        }
                    }
                    Err(e) => {
                        if accept_stopping.load(Ordering::SeqCst) {
                            break;
                        }
                        warn!("accept failed: {e}");
                    }
                }
            }
        })
        .context("spawning the socket accept thread")?;

    info!(path = %path.display(), proto = PROTO, "control socket listening");
    Ok(Server {
        path: path.to_path_buf(),
        stopping,
        accept: std::sync::Mutex::new(Some(accept)),
        service,
    })
}

fn serve_client(service: Arc<Service>, stream: UnixStream) -> Result<()> {
    let reader_stream = stream.try_clone().context("cloning the client socket")?;
    let writer_stream = stream.try_clone().context("cloning the client socket")?;
    let (client, rx) = service.bus.attach(Some(stream));

    let writer_id = client.id;
    let writer_client = Arc::clone(&client);
    let writer = std::thread::Builder::new()
        .name("recalld-client-w".into())
        .spawn(move || {
            let mut out = writer_stream;
            while let Ok(line) = rx.recv() {
                if out.write_all(&line).is_err() || out.flush().is_err() {
                    debug!(client = writer_id, "client went away mid-write");
                    writer_client.kill();
                    break;
                }
            }
        })
        .context("spawning the client writer thread")?;

    let result = read_loop(&service, &client, reader_stream);
    service.bus.detach(&client);
    let _ = writer.join();
    result
}

fn read_loop(service: &Arc<Service>, client: &Arc<Client>, stream: UnixStream) -> Result<()> {
    let mut greeted = false;
    let mut direct = stream.try_clone().ok();
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            // A hung-up or shut-down socket is the normal end of a connection.
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        match proto::parse(&line) {
            Incoming::Hello {
                proto: p,
                client: name,
            } => {
                if greeted {
                    // A second hello is a confused client, not a fatal fault.
                    client.send(Arc::new(proto::fatal("handshake", "already greeted")));
                    continue;
                }
                if p != PROTO as u64 {
                    // PROTOCOL.md: unsupported version is answered and closed.
                    say_and_close(
                        &mut direct,
                        client,
                        proto::fatal(
                            "proto",
                            &format!("this daemon speaks proto {PROTO}, not {p}"),
                        ),
                    );
                    return Ok(());
                }
                greeted = true;
                info!(client = client.id, peer = %name, "client connected");
                client.send(Arc::new(proto::welcome(
                    service.bus.current_seq(),
                    crate::store::SCHEMA_VERSION,
                )));
            }
            Incoming::Request(req) => {
                if !greeted {
                    say_and_close(
                        &mut direct,
                        client,
                        proto::fatal("handshake", "send a hello before any request"),
                    );
                    return Ok(());
                }
                let reply = match service.handle(client, &req) {
                    Ok(value) => proto::ok(&req.id, value),
                    Err(e) => {
                        debug!(client = client.id, method = %req.method, "request failed: {}", e.msg);
                        proto::err(&req.id, &e)
                    }
                };
                if !client.send(Arc::new(reply)) {
                    break;
                }
            }
            Incoming::Malformed(why) => {
                // One bad line does not end the connection: a client that can
                // still be told what it got wrong is a client that can recover.
                client.send(Arc::new(proto::fatal("malformed", &why)));
            }
        }
    }
    Ok(())
}

/// Write one last line straight to the socket, then hang up. The outbox is not
/// used here: `kill` shuts the socket down immediately, and this line has to
/// have landed before it does.
fn say_and_close(direct: &mut Option<UnixStream>, client: &Arc<Client>, line: Vec<u8>) {
    if let Some(out) = direct.as_mut() {
        let _ = out.write_all(&line);
        let _ = out.flush();
    }
    client.kill();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::Allowlist;
    use crate::bus::Bus;
    use crate::control::Control;
    use crate::store::Store;
    use serde_json::Value;
    use std::sync::Mutex;

    struct Rig {
        server: Server,
        dir: PathBuf,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            self.server.shutdown();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn rig(name: &str) -> Rig {
        let dir =
            std::env::temp_dir().join(format!("nx-recall-server-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir).unwrap();
        let control = Control::new(dir.clone(), None, &Allowlist::default());
        let bus = Bus::new(64, 32);
        let service = Service::new(Arc::new(Mutex::new(store)), control, bus);
        let server = serve(service, &dir.join("nx-recall.sock")).unwrap();
        Rig { server, dir }
    }

    struct Conn {
        reader: BufReader<UnixStream>,
        writer: UnixStream,
    }

    impl Conn {
        fn open(rig: &Rig) -> Self {
            Self::at(rig.server.path())
        }
        fn at(path: &Path) -> Self {
            let s = UnixStream::connect(path).unwrap();
            s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            Self {
                reader: BufReader::new(s.try_clone().unwrap()),
                writer: s,
            }
        }
        fn send(&mut self, line: &str) {
            self.writer.write_all(line.as_bytes()).unwrap();
            self.writer.write_all(b"\n").unwrap();
            self.writer.flush().unwrap();
        }
        fn recv(&mut self) -> Value {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).unwrap();
            assert!(n > 0, "the daemon closed the connection unexpectedly");
            serde_json::from_str(&line).unwrap()
        }
        fn eof(&mut self) -> bool {
            let mut line = String::new();
            self.reader.read_line(&mut line).unwrap_or(0) == 0
        }
        fn hello(rig: &Rig) -> Self {
            let mut c = Self::open(rig);
            c.send(r#"{"hello":{"proto":1,"client":"test/1"}}"#);
            let w = c.recv();
            assert_eq!(w["welcome"]["proto"], 1);
            c
        }
    }

    #[test]
    fn the_socket_is_owner_only() {
        let r = rig("mode");
        let mode = std::fs::metadata(r.server.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the socket must not be world-reachable"
        );
    }

    #[test]
    fn the_handshake_reports_the_protocol_the_seq_and_the_schema() {
        let r = rig("handshake");
        let mut c = Conn::open(&r);
        c.send(r#"{"hello":{"proto":1,"client":"nx-recall-gui/0.1"}}"#);
        let w = c.recv();
        assert_eq!(w["welcome"]["proto"], 1);
        assert_eq!(w["welcome"]["schema"], crate::store::SCHEMA_VERSION);
        assert_eq!(w["welcome"]["seq"], 0);
        assert!(
            w["welcome"]["daemon"]
                .as_str()
                .unwrap()
                .starts_with("recalld/")
        );
    }

    #[test]
    fn an_unsupported_protocol_is_answered_and_closed() {
        let r = rig("proto");
        let mut c = Conn::open(&r);
        c.send(r#"{"hello":{"proto":99,"client":"from-the-future/9"}}"#);
        assert_eq!(c.recv()["error"]["code"], "proto");
        assert!(c.eof(), "the connection must be closed after a proto error");
    }

    #[test]
    fn a_request_before_the_handshake_is_refused() {
        let r = rig("no-hello");
        let mut c = Conn::open(&r);
        c.send(r#"{"id":1,"method":"status"}"#);
        assert_eq!(c.recv()["error"]["code"], "handshake");
        assert!(c.eof());
    }

    #[test]
    fn one_malformed_line_does_not_end_the_connection() {
        let r = rig("malformed");
        let mut c = Conn::hello(&r);
        c.send("this is not json");
        assert_eq!(c.recv()["error"]["code"], "malformed");
        c.send(r#"{"id":4,"method":"status"}"#);
        assert_eq!(c.recv()["id"], 4);
    }

    #[test]
    fn requests_are_answered_with_their_own_id() {
        let r = rig("ids");
        let mut c = Conn::hello(&r);
        c.send(r#"{"id":11,"method":"speakers.list"}"#);
        c.send(r#"{"id":"twelve","method":"nonsense"}"#);
        let a = c.recv();
        assert_eq!(a["id"], 11);
        assert!(a["ok"]["speakers"].is_array());
        let b = c.recv();
        assert_eq!(b["id"], "twelve");
        assert_eq!(b["err"]["code"], "unknown_method");
    }

    #[test]
    fn a_second_daemon_refuses_to_take_the_socket() {
        let r = rig("busy");
        let store = Store::open(&r.dir).unwrap();
        let control = Control::new(r.dir.clone(), None, &Allowlist::default());
        let service = Service::new(Arc::new(Mutex::new(store)), control, Bus::new(8, 8));
        let err = match serve(service, r.server.path()) {
            Ok(_) => panic!("a second bind on a live socket must fail"),
            Err(e) => e,
        };
        assert!(format!("{err:#}").contains("already listening"));
    }

    #[test]
    fn a_socket_left_by_a_crash_is_reclaimed() {
        let r = rig("stale");
        // Take the socket away and leave a plain file where it was, exactly as
        // a kill -9 does.
        r.server.shutdown();
        std::fs::write(r.server.path(), b"stale").unwrap();

        let store = Store::open(&r.dir).unwrap();
        let control = Control::new(r.dir.clone(), None, &Allowlist::default());
        let service = Service::new(Arc::new(Mutex::new(store)), control, Bus::new(8, 8));
        let server = serve(service, r.server.path()).unwrap();

        let mut c = Conn::at(server.path());
        c.send(r#"{"hello":{"proto":1,"client":"t/1"}}"#);
        assert_eq!(c.recv()["welcome"]["proto"], 1);
        server.shutdown();
    }

    #[test]
    fn shutdown_closes_the_socket_file_and_the_clients() {
        let r = rig("stop");
        let mut c = Conn::hello(&r);
        let path = r.server.path().to_path_buf();

        r.server.shutdown();
        assert!(!path.exists(), "the socket file must not be left behind");
        assert!(c.eof(), "connected clients are hung up on");
    }
}
