//! The VRChat instance roster (DESIGN §7).
//!
//! The roster does not come over OSC — it comes from VRChat's output log, which
//! records every join and leave with a display name and rotates on a world
//! change. This is a port of `spike/roster_watch.py`, which validated that
//! source live, and it keeps the two behaviours that prototype had to learn:
//!
//! - **Names are stripped but never otherwise normalised.** They carry trailing
//!   whitespace before the `(usr_…)` id and are frequently non-ASCII; anything
//!   more aggressive than a trim would rename people.
//! - **`OnPlayerLeftRoom` is not a leave.** It is a room-lifecycle line with no
//!   name attached, and treating it as one empties the roster at random.
//!
//! Nothing here can take the daemon down. No VRChat, no Proton prefix, no log:
//! the tailer sleeps and looks again.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use regex::Regex;
use serde_json::json;
use tracing::{debug, info, warn};

use crate::bus::{Bus, Topic};
use crate::config::RosterConfig;
use crate::control::Control;
use crate::store::Store;

/// Where Steam's Proton prefix keeps VRChat's logs. App id 438100.
const LOG_DIR: &str = "Steam/steamapps/compatdata/438100/pfx/drive_c/users/steamuser\
                       /AppData/LocalLow/VRChat/VRChat";
const LOG_PREFIX: &str = "output_log_";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Join {
        who: String,
    },
    Leave {
        who: String,
    },
    World {
        world_id: String,
        instance: String,
    },
    /// `Entering Room: <name>` — the human-readable world name, which arrives
    /// separately from the id.
    Room {
        name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// UTC nanoseconds, converted from the log's local-time stamp.
    pub t_utc_ns: i64,
    pub event: Event,
}

/// The one regex, transcribed from the prototype.
///
/// `2026.08.31 18:45:57 Debug      -  [Behaviour] OnPlayerJoined Name (usr_xxx)`
/// The trailing ` (usr_…)` is present in newer builds and absent in older ones.
/// The mandatory space after `OnPlayerJoined`/`OnPlayerLeft` is what makes
/// `OnPlayerLeftRoom` fail to match, which is exactly right.
fn line_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"^(\d{4}\.\d{2}\.\d{2} \d{2}:\d{2}:\d{2}) .*?\[Behaviour\] ",
            r"(?:OnPlayer(Joined|Left) (.+?)(?: \(usr_[0-9a-fA-F-]+\))?",
            r"|Joining (wrld_[0-9a-fA-F-]+):(\S+)",
            r"|Entering Room: (.+))\s*$",
        ))
        .expect("the roster regex is a literal and must compile")
    })
}

pub fn parse_line(line: &str) -> Option<LogLine> {
    let caps = line_re().captures(line)?;
    let t_utc_ns = local_stamp_to_utc_ns(caps.get(1)?.as_str())?;

    if let Some(kind) = caps.get(2) {
        let who = caps.get(3)?.as_str().trim();
        if who.is_empty() {
            return None;
        }
        let who = who.to_string();
        return Some(LogLine {
            t_utc_ns,
            event: if kind.as_str() == "Joined" {
                Event::Join { who }
            } else {
                Event::Leave { who }
            },
        });
    }
    if let Some(world) = caps.get(4) {
        return Some(LogLine {
            t_utc_ns,
            event: Event::World {
                world_id: world.as_str().to_string(),
                // `12345~region(eu)` — the instance id is the part before the
                // first tilde; the rest is access-type decoration.
                instance: caps
                    .get(5)?
                    .as_str()
                    .split('~')
                    .next()
                    .unwrap_or_default()
                    .to_string(),
            },
        });
    }
    Some(LogLine {
        t_utc_ns,
        event: Event::Room {
            name: caps.get(6)?.as_str().trim().to_string(),
        },
    })
}

/// `YYYY.MM.DD HH:MM:SS` in the machine's local time, as UTC nanoseconds.
///
/// The log has no zone, and the prototype read it as local time
/// (`datetime.strptime(...).astimezone()`). `mktime` is the equivalent, and
/// unlike hand arithmetic it knows about the local DST rules — including the
/// ambiguous hour, where `tm_isdst = -1` asks libc to pick.
pub fn local_stamp_to_utc_ns(stamp: &str) -> Option<i64> {
    let bytes = stamp.as_bytes();
    if bytes.len() != 19 {
        return None;
    }
    let num = |a: usize, b: usize| stamp.get(a..b)?.parse::<i32>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    // mktime happily normalises month 13 into next January. A log line that
    // says that is corrupt, not a date, so reject it rather than invent a time.
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 60 {
        return None;
    }

    // SAFETY: the struct is zeroed before use and mktime only reads it.
    let secs = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        tm.tm_year = y - 1900;
        tm.tm_mon = mo - 1;
        tm.tm_mday = d;
        tm.tm_hour = h;
        tm.tm_min = mi;
        tm.tm_sec = s;
        tm.tm_isdst = -1;
        libc::mktime(&mut tm)
    };
    if secs == -1 {
        return None;
    }
    Some(secs as i64 * 1_000_000_000)
}

/// The newest `output_log_*.txt` under `dir`, by modification time.
pub fn newest_log(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        let name = path.file_name()?.to_string_lossy().to_string();
        if !name.starts_with(LOG_PREFIX) || !name.ends_with(".txt") {
            continue;
        }
        let Ok(mtime) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, p)| p)
}

/// Where to look when the config does not say. Every Steam library root we can
/// name, since a Proton prefix is not always under `~/.local/share`.
pub fn default_log_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(data) = dirs::data_dir() {
        dirs.push(data.join(LOG_DIR));
    }
    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(".steam").join("steam").join(LOG_DIR));
        dirs.push(home.join(".local/share").join(LOG_DIR));
    }
    dirs.retain(|d| d.exists());
    dirs.dedup();
    dirs
}

/// The in-memory view the prototype's `replay` built: which world we are in,
/// and who is in it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RosterState {
    pub world_id: Option<String>,
    pub instance: Option<String>,
    pub world_name: Option<String>,
    /// `(name, joined_at_utc_ns)`, in the order they arrived.
    pub present: Vec<(String, i64)>,
}

impl RosterState {
    /// Fold one line in. Returns whether the state changed, which is what
    /// decides if the daemon writes a row and publishes an event.
    pub fn apply(&mut self, line: &LogLine) -> bool {
        match &line.event {
            Event::World { world_id, instance } => {
                // A world change ends everyone's presence: the log rotates and
                // the old instance is simply gone.
                self.world_id = Some(world_id.clone());
                self.instance = Some(instance.clone());
                self.world_name = None;
                self.present.clear();
                true
            }
            Event::Room { name } => {
                self.world_name = Some(name.clone());
                true
            }
            Event::Join { who } => {
                if self.present.iter().any(|(n, _)| n == who) {
                    return false;
                }
                self.present.push((who.clone(), line.t_utc_ns));
                true
            }
            Event::Leave { who } => {
                let before = self.present.len();
                self.present.retain(|(n, _)| n != who);
                before != self.present.len()
            }
        }
    }

    pub fn names(&self) -> Vec<&str> {
        self.present.iter().map(|(n, _)| n.as_str()).collect()
    }
}

/// The tailer thread's stop switch.
#[derive(Default)]
pub struct RosterStop(AtomicBool);

impl RosterStop {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    fn stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Tail the newest log for as long as `stop` says to, writing to the store and
/// publishing `roster` events. Never returns an error: there is no failure here
/// that should cost the daemon its capture.
pub fn run(
    cfg: &RosterConfig,
    store: Arc<std::sync::Mutex<Store>>,
    bus: Arc<Bus>,
    control: Arc<Control>,
    stop: Arc<RosterStop>,
) {
    let poll = Duration::from_millis(cfg.poll_ms.max(50));
    let retry = Duration::from_secs(cfg.retry_s.max(1));
    let dirs: Vec<PathBuf> = match &cfg.log_dir {
        Some(d) => vec![d.clone()],
        None => Vec::new(),
    };
    let mut announced_missing = false;

    while !stop.stopped() {
        let candidates = if dirs.is_empty() {
            default_log_dirs()
        } else {
            dirs.clone()
        };
        let log = candidates.iter().find_map(|d| newest_log(d));
        let Some(log) = log else {
            if !announced_missing {
                info!("no VRChat log found yet; the roster tailer will keep looking");
                announced_missing = true;
            }
            sleep_until(&stop, retry);
            continue;
        };
        announced_missing = false;
        info!(log = %log.display(), "tailing the VRChat log");

        if let Err(e) = tail_one(
            &log,
            &candidates,
            poll,
            &store,
            &bus,
            &control,
            &stop,
        ) {
            warn!("roster tailer restarting after: {e:#}");
            sleep_until(&stop, retry);
        }
    }
    debug!("roster tailer stopped");
}

fn sleep_until(stop: &RosterStop, total: Duration) {
    let step = Duration::from_millis(100);
    let mut slept = Duration::ZERO;
    while slept < total && !stop.stopped() {
        std::thread::sleep(step);
        slept += step;
    }
}

/// Follow one log file until it is superseded. Returns `Ok(())` when the log
/// rotated (the caller picks the new one up) and an error only for a read
/// failure that warrants a pause before retrying.
fn tail_one(
    log: &Path,
    dirs: &[PathBuf],
    poll: Duration,
    store: &Arc<std::sync::Mutex<Store>>,
    bus: &Arc<Bus>,
    control: &Arc<Control>,
    stop: &RosterStop,
) -> Result<()> {
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    let mut file = std::fs::File::open(log)?;
    let mut state = RosterState::default();

    // Replay the history first, exactly as the prototype does: a daemon that
    // starts mid-session has to know who is already in the instance. The rows
    // are written afterwards, deduplicated on (name, joined_at), so a restart
    // does not double-count anyone.
    {
        let mut reader = BufReader::new(&mut file);
        let mut line = String::new();
        loop {
            line.clear();
            let mut raw = Vec::new();
            if reader.read_until(b'\n', &mut raw)? == 0 {
                break;
            }
            let text = String::from_utf8_lossy(&raw);
            if let Some(parsed) = parse_line(text.trim_end()) {
                state.apply(&parsed);
            }
        }
    }
    let mut pos = file.stream_position()?;
    record_snapshot(&state, store, bus, control);

    loop {
        if stop.stopped() {
            return Ok(());
        }
        // A world change rotates the log; so does a VRChat restart.
        if let Some(newest) = dirs.iter().find_map(|d| newest_log(d))
            && newest != log
        {
            info!(from = %log.display(), to = %newest.display(), "the VRChat log rotated");
            return Ok(());
        }
        let len = std::fs::metadata(log)?.len();
        if len < pos {
            // Truncated in place: start over rather than reading garbage.
            debug!("the VRChat log was truncated; re-reading from the top");
            pos = 0;
        }
        if len == pos {
            sleep_until(stop, poll);
            continue;
        }

        let mut file = std::fs::File::open(log)?;
        file.seek(SeekFrom::Start(pos))?;
        let mut reader = BufReader::new(file);
        loop {
            let mut raw = Vec::new();
            let n = reader.read_until(b'\n', &mut raw)?;
            if n == 0 {
                break;
            }
            if !raw.ends_with(b"\n") {
                // A partial line: leave it for the next pass so a name is
                // never split in half.
                break;
            }
            pos += n as u64;
            let text = String::from_utf8_lossy(&raw);
            let Some(parsed) = parse_line(text.trim_end()) else {
                continue;
            };
            if state.apply(&parsed) {
                record(&parsed, &state, store, bus, control);
            }
        }
    }
}

/// Write one accepted line down and tell every subscriber.
///
/// Paused means paused: the roster is a list of people, so while writes are
/// off it is neither stored nor broadcast. The file position still advances,
/// so resuming does not replay a backlog of who came and went.
fn record(
    line: &LogLine,
    state: &RosterState,
    store: &Arc<std::sync::Mutex<Store>>,
    bus: &Arc<Bus>,
    control: &Arc<Control>,
) {
    if control.is_paused() {
        return;
    }
    let guard = store.lock().unwrap_or_else(|p| p.into_inner());
    let world = state.world_id.as_deref();
    let instance = state.instance.as_deref();
    let outcome = match &line.event {
        Event::Join { who } => guard
            .roster_join(world, instance, who, line.t_utc_ns)
            .map(|_| ()),
        Event::Leave { who } => guard.roster_leave(who, line.t_utc_ns).map(|_| ()),
        Event::World { .. } => guard.roster_close_all(line.t_utc_ns).map(|_| ()),
        Event::Room { .. } => Ok(()),
    };
    drop(guard);
    if let Err(e) = outcome {
        warn!("could not record a roster event: {e:#}");
        return;
    }

    let data = match &line.event {
        Event::Join { who } => json!({"ev": "join", "who": who, "t": line.t_utc_ns}),
        Event::Leave { who } => json!({"ev": "leave", "who": who, "t": line.t_utc_ns}),
        Event::World { world_id, instance } => json!({
            "ev": "world", "world_id": world_id, "instance": instance, "t": line.t_utc_ns,
        }),
        Event::Room { name } => json!({"ev": "room", "name": name, "t": line.t_utc_ns}),
    };
    bus.publish(Topic::Roster, "roster", data);
}

/// After replaying a log's history: persist who is present and announce the
/// snapshot, so a client that connects mid-session sees the instance.
fn record_snapshot(
    state: &RosterState,
    store: &Arc<std::sync::Mutex<Store>>,
    bus: &Arc<Bus>,
    control: &Arc<Control>,
) {
    if control.is_paused() {
        return;
    }
    {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        for (who, joined) in &state.present {
            if let Err(e) = guard.roster_join(
                state.world_id.as_deref(),
                state.instance.as_deref(),
                who,
                *joined,
            ) {
                warn!("could not record {who} as present: {e:#}");
            }
        }
    }
    bus.publish(
        Topic::Roster,
        "roster",
        json!({
            "ev": "sync",
            "world_id": state.world_id,
            "instance": state.instance,
            "world_name": state.world_name,
            "who": state.names(),
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines in the shapes the prototype's regex cases cover.
    const JOIN: &str =
        "2026.08.31 18:45:57 Debug      -  [Behaviour] OnPlayerJoined Ines (usr_0a1b2c3d-4e5f-6789-abcd-ef0123456789)";
    const JOIN_OLD: &str = "2026.08.31 18:46:02 Log        -  [Behaviour] OnPlayerJoined Kestrel";
    const LEAVE: &str =
        "2026.08.31 19:01:00 Debug      -  [Behaviour] OnPlayerLeft Ines (usr_0a1b2c3d-4e5f-6789-abcd-ef0123456789)";
    const WORLD: &str =
        "2026.08.31 18:45:50 Debug      -  [Behaviour] Joining wrld_4432ea9b-729c-46e3-8eaf-846aa0a37fdd:12345~region(eu)";
    const ROOM: &str = "2026.08.31 18:45:51 Debug      -  [Behaviour] Entering Room: The Great Pug";

    fn ev(line: &str) -> Event {
        parse_line(line)
            .unwrap_or_else(|| panic!("this line must parse: {line}"))
            .event
    }

    #[test]
    fn joins_parse_with_and_without_the_user_id() {
        assert_eq!(ev(JOIN), Event::Join { who: "Ines".into() });
        assert_eq!(
            ev(JOIN_OLD),
            Event::Join {
                who: "Kestrel".into()
            }
        );
        assert_eq!(ev(LEAVE), Event::Leave { who: "Ines".into() });
    }

    #[test]
    fn names_are_stripped_but_never_otherwise_normalised() {
        // Trailing whitespace before the id is common and must not become part
        // of the name — otherwise the leave never matches the join.
        let padded = "2026.08.31 18:45:57 Debug      -  [Behaviour] OnPlayerJoined  Ines   (usr_00000000-0000-0000-0000-000000000000)";
        assert_eq!(ev(padded), Event::Join { who: "Ines".into() });

        // Non-ASCII names are the common case, not an edge case, and their
        // interior spacing and case are theirs.
        for name in ["きつね", "Ünter Strich", "ᴠᴏɪᴅ", "Ines  Two", "Ｍｉｒａ"] {
            let line = format!(
                "2026.08.31 18:45:57 Debug      -  [Behaviour] OnPlayerJoined {name} (usr_00000000-0000-0000-0000-000000000000)"
            );
            assert_eq!(
                ev(&line),
                Event::Join {
                    who: name.to_string()
                },
                "{name} must survive parsing unchanged"
            );
        }
    }

    #[test]
    fn on_player_left_room_is_noise_not_a_leave() {
        // The bug this guards: `OnPlayerLeftRoom` has no name, and reading it
        // as a leave empties the roster at random.
        for line in [
            "2026.08.31 19:30:00 Debug      -  [Behaviour] OnPlayerLeftRoom",
            "2026.08.31 19:30:00 Debug      -  [Behaviour] OnPlayerLeftRoom ",
            "2026.08.31 19:30:00 Debug      -  [Behaviour] OnPlayerJoinedRoom",
        ] {
            assert_eq!(parse_line(line), None, "{line} must not parse as an event");
        }
    }

    #[test]
    fn world_lines_carry_the_id_and_the_bare_instance() {
        assert_eq!(
            ev(WORLD),
            Event::World {
                world_id: "wrld_4432ea9b-729c-46e3-8eaf-846aa0a37fdd".into(),
                // Everything from the first `~` is access-type decoration.
                instance: "12345".into(),
            }
        );
        assert_eq!(
            ev(ROOM),
            Event::Room {
                name: "The Great Pug".into()
            }
        );
    }

    #[test]
    fn unrelated_lines_are_ignored() {
        for line in [
            "",
            "2026.08.31 18:45:57 Debug      -  [Video Playback] something else",
            "not a log line at all",
            "2026.13.99 99:99:99 Debug      -  [Behaviour] OnPlayerJoined Ines",
        ] {
            assert_eq!(parse_line(line), None, "{line:?} must not parse");
        }
    }

    #[test]
    fn timestamps_are_local_time_converted_to_utc() {
        let a = local_stamp_to_utc_ns("2026.08.31 18:45:57").unwrap();
        let b = local_stamp_to_utc_ns("2026.08.31 18:46:57").unwrap();
        assert_eq!(b - a, 60_000_000_000, "a minute is a minute in any zone");

        // Whatever the zone is, the value must read back as the same civil
        // time through libc — which is the property that makes it right.
        let secs = a / 1_000_000_000;
        // SAFETY: localtime_r writes into a zeroed tm we own.
        let tm = unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            libc::localtime_r(&(secs as libc::time_t), &mut tm);
            tm
        };
        assert_eq!((tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday), (2026, 8, 31));
        assert_eq!((tm.tm_hour, tm.tm_min, tm.tm_sec), (18, 45, 57));

        assert_eq!(local_stamp_to_utc_ns("nonsense"), None);
        assert_eq!(local_stamp_to_utc_ns("2026.08.31 18:45"), None);
    }

    #[test]
    fn the_state_folds_a_session_the_way_the_prototype_does() {
        let mut state = RosterState::default();
        assert!(state.apply(&parse_line(WORLD).unwrap()));
        assert!(state.apply(&parse_line(ROOM).unwrap()));
        assert!(state.apply(&parse_line(JOIN).unwrap()));
        assert!(state.apply(&parse_line(JOIN_OLD).unwrap()));
        assert_eq!(state.names(), vec!["Ines", "Kestrel"]);
        assert_eq!(state.instance.as_deref(), Some("12345"));
        assert_eq!(state.world_name.as_deref(), Some("The Great Pug"));

        // A duplicate join is not a change.
        assert!(!state.apply(&parse_line(JOIN).unwrap()));
        assert!(state.apply(&parse_line(LEAVE).unwrap()));
        assert_eq!(state.names(), vec!["Kestrel"]);
        // A leave for somebody who is not here changes nothing.
        assert!(!state.apply(&parse_line(LEAVE).unwrap()));

        // A world change empties the instance.
        let next = "2026.08.31 20:00:00 Debug      -  [Behaviour] Joining wrld_0000ea9b-729c-46e3-8eaf-846aa0a37fdd:99";
        assert!(state.apply(&parse_line(next).unwrap()));
        assert!(state.names().is_empty());
        assert_eq!(state.instance.as_deref(), Some("99"));
    }

    #[test]
    fn the_newest_log_is_the_one_that_is_tailed() {
        let dir = std::env::temp_dir().join(format!("nx-recall-roster-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(newest_log(&dir), None, "an empty dir is not an error");

        std::fs::write(dir.join("output_log_2026-08-31_18-00-00.txt"), b"old\n").unwrap();
        std::fs::write(dir.join("not-a-log.txt"), b"x\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let newer = dir.join("output_log_2026-08-31_20-00-00.txt");
        std::fs::write(&newer, b"new\n").unwrap();

        assert_eq!(newest_log(&dir), Some(newer));
        assert_eq!(newest_log(&dir.join("nowhere")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
