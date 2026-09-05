//! Flap tolerance (0.13.0).
//!
//! A source's node can vanish and reappear inside a grace window. The
//! 2026-09-05 16:07–16:19 UTC run made the case: VRChat ran for twelve
//! minutes and the tap logged 37 "capturing … key=VRChat.exe" lines, each
//! `pw_target` a new registry id (78332 → 78341 → 78348 → …), each one
//! followed milliseconds later by "source went away; closing session"
//! (spike/FINDINGS.md §47). VRChat recreates its playback stream repeatedly —
//! device init, menu transitions, a world load — and every one of those used
//! to be a hard stop: `on_node_removed` closed the session, discarded
//! whatever turn was in progress, and the very next `on_node` opened a brand
//! new one. Nineteen sessions, one conversation. From the reader's side an
//! unbroken twelve minutes came back as nineteen separate, unrelated
//! fragments.
//!
//! This module is the pure decision half of the fix. It knows nothing about
//! PipeWire, streams, or the database — it is handed match keys, a PID-based
//! identity and [`Instant`]s, and it says whether a reappearing node is the
//! same source continuing or a genuinely new one. `capture.rs` is the only
//! caller, and the only place a [`Resume::Same`] turns into "keep the session
//! id, reattach the stream, tell the pipeline about the gap".

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A source that left the graph, waiting inside the grace window to see
/// whether it comes back.
#[derive(Debug, Clone)]
struct Pending {
    session_id: i64,
    /// A PID-based identity, when known — see [`same_instance`] for why this
    /// must NOT be `NodeInfo::instance_key()`'s serial-first answer: a flap is
    /// by definition a NEW PipeWire node, so it always carries a NEW
    /// `object.serial`, and matching on that would refuse every flap it is
    /// meant to absorb. The process id is what actually stays put across a
    /// reconnect — VRChat's wine64-preloader keeps its pid through every one
    /// of the 37 stream recreations the 2026-09-05 run measured.
    pid_key: Option<String>,
    removed_at: Instant,
}

/// A burst of flaps in progress for one `match_key`, tracked independently of
/// [`Pending`] so the count survives across several flap/resume cycles and is
/// only read out once the source has actually settled (see [`FlapTracker::settle`]).
#[derive(Debug, Clone)]
struct Burst {
    flaps: u32,
    started: Instant,
    last_flap: Instant,
}

/// What a reappearing node should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    /// Continue this session. `gap` is how long the source was off the graph.
    Same { session_id: i64, gap: Duration },
    /// No pending flap matched (none was in flight, the grace window had
    /// already elapsed, or the reappearing node is a different instance):
    /// treat this as a brand-new source.
    Fresh,
}

/// One flap burst that has been resolved, for the summary line. Log **one**
/// of these per burst, never one per flap — that discipline is the point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettledBurst {
    pub match_key: String,
    pub flaps: u32,
    pub burst_duration: Duration,
}

/// A flap whose grace window ran out with nothing reappearing: the source
/// really did leave, and the caller must close the session for real.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiredFlap {
    pub match_key: String,
    pub session_id: i64,
    /// How many times this burst flapped before finally giving up. At least 1.
    pub flaps: u32,
    pub burst_duration: Duration,
}

/// Tracks in-flight flaps and running burst counts per source, whole.
///
/// Every method takes `now` rather than reading a clock, so a test can replay
/// exact timestamps — including the millisecond-apart pattern the journal
/// recorded for VRChat.exe on 2026-09-05.
#[derive(Debug)]
pub struct FlapTracker {
    grace: Duration,
    pending: HashMap<String, Pending>,
    bursts: HashMap<String, Burst>,
}

/// Two nodes are the same instance when either side does not know a pid (a
/// node with no `application.process.id` at all, which is a real and common
/// case — some Flatpak and remote-desktop clients never set one) or when both
/// sides know one and it matches. Only a KNOWN mismatch refuses the flap.
fn same_instance(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x == y,
        _ => true,
    }
}

impl FlapTracker {
    pub fn new(grace: Duration) -> Self {
        Self {
            grace,
            pending: HashMap::new(),
            bursts: HashMap::new(),
        }
    }

    pub fn grace(&self) -> Duration {
        self.grace
    }

    /// A captured source's node just left the graph. Starts (or extends) a
    /// pending flap for `match_key`; the caller must NOT close the session
    /// yet — that decision waits for [`Self::expire`] (timed out) or a
    /// matching [`Self::on_reappear`] (resumed).
    pub fn start_flap(
        &mut self,
        match_key: &str,
        session_id: i64,
        pid_key: Option<String>,
        now: Instant,
    ) {
        self.pending.insert(
            match_key.to_string(),
            Pending {
                session_id,
                pid_key,
                removed_at: now,
            },
        );
        let burst = self.bursts.entry(match_key.to_string()).or_insert(Burst {
            flaps: 0,
            started: now,
            last_flap: now,
        });
        burst.flaps += 1;
        burst.last_flap = now;
    }

    /// A node for `match_key` just appeared. Consults the pending flap, if
    /// any, and consumes it either way — a stale pending entry (grace already
    /// elapsed; [`Self::expire`] simply has not run yet) must not linger and
    /// match a THIRD, unrelated reappearance later.
    pub fn on_reappear(
        &mut self,
        match_key: &str,
        pid_key: Option<String>,
        now: Instant,
    ) -> Resume {
        let Some(pending) = self.pending.get(match_key) else {
            return Resume::Fresh;
        };
        if !same_instance(&pending.pid_key, &pid_key) {
            // A different copy of the app started while the old one's flap
            // was still pending. Leave the old entry for `expire` to close
            // out on its own schedule; this reappearance is unrelated to it.
            return Resume::Fresh;
        }
        let gap = now.saturating_duration_since(pending.removed_at);
        if gap > self.grace {
            self.pending.remove(match_key);
            return Resume::Fresh;
        }
        let pending = self.pending.remove(match_key).expect("checked above");
        Resume::Same {
            session_id: pending.session_id,
            gap,
        }
    }

    /// Sweep for flaps whose grace window has run out with nothing
    /// reappearing. Call this on the same clock that drives everything else
    /// polled (the daemon's 250 ms rule-change timer) — a flap is not itself
    /// an event, so nothing else will ever notice it aged out.
    pub fn expire(&mut self, now: Instant) -> Vec<ExpiredFlap> {
        let expired: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, p)| now.saturating_duration_since(p.removed_at) > self.grace)
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = Vec::with_capacity(expired.len());
        for key in expired {
            let pending = self.pending.remove(&key).expect("just found by key");
            let burst = self.bursts.remove(&key);
            let (flaps, burst_duration) = match burst {
                Some(b) => (b.flaps, b.last_flap.saturating_duration_since(b.started)),
                None => (1, Duration::ZERO),
            };
            out.push(ExpiredFlap {
                match_key: key,
                session_id: pending.session_id,
                flaps,
                burst_duration,
            });
        }
        out
    }

    /// Sweep for bursts that have gone quiet — no flap in the last `grace`
    /// interval — while the source is NOT currently pending (i.e. it is
    /// present on the graph and capturing normally). This is what turns a
    /// string of absorbed flaps into the one summary line the burst earns,
    /// separately from [`Self::expire`]'s "gave up entirely" case.
    pub fn settle(&mut self, now: Instant) -> Vec<SettledBurst> {
        let settled: Vec<String> = self
            .bursts
            .iter()
            .filter(|(k, b)| {
                !self.pending.contains_key(*k)
                    && now.saturating_duration_since(b.last_flap) >= self.grace
            })
            .map(|(k, _)| k.clone())
            .collect();
        let mut out = Vec::with_capacity(settled.len());
        for key in settled {
            let burst = self.bursts.remove(&key).expect("just found by key");
            out.push(SettledBurst {
                match_key: key,
                flaps: burst.flaps,
                burst_duration: burst.last_flap.saturating_duration_since(burst.started),
            });
        }
        out
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GRACE: Duration = Duration::from_secs(5);

    #[test]
    fn a_reappearance_inside_grace_continues_the_same_session() {
        // Every flap is, by definition, a brand new PipeWire node — VRChat's
        // pw_target went 78332 -> 78341 -> 78348 on 2026-09-05, a new
        // object.serial every time. What stays put across the reconnect is
        // the process id, so that is what flap identity is keyed on here,
        // never the serial.
        let mut t = FlapTracker::new(GRACE);
        let t0 = Instant::now();
        t.start_flap("VRChat.exe", 42, Some("pid:9001".into()), t0);
        let t1 = t0 + Duration::from_millis(120);
        assert_eq!(
            t.on_reappear("VRChat.exe", Some("pid:9001".into()), t1),
            Resume::Same {
                session_id: 42,
                gap: Duration::from_millis(120)
            },
            "the same process reconnecting under a new node must resume the old session"
        );
        assert_eq!(t.pending_len(), 0, "the resumed flap must not linger");
    }

    #[test]
    fn a_reappearance_past_the_grace_window_is_fresh() {
        let mut t = FlapTracker::new(GRACE);
        let t0 = Instant::now();
        t.start_flap("VRChat.exe", 1, None, t0);
        let late = t0 + GRACE + Duration::from_millis(1);
        assert_eq!(t.on_reappear("VRChat.exe", None, late), Resume::Fresh);
    }

    #[test]
    fn a_different_known_instance_does_not_resume_the_old_session() {
        let mut t = FlapTracker::new(GRACE);
        let t0 = Instant::now();
        t.start_flap("Discord", 7, Some("pid:100".into()), t0);
        let t1 = t0 + Duration::from_millis(50);
        // A second, unrelated copy of the same program launched in the gap.
        assert_eq!(
            t.on_reappear("Discord", Some("pid:200".into()), t1),
            Resume::Fresh
        );
    }

    #[test]
    fn an_unknown_instance_on_either_side_still_matches() {
        // Neither the departing nor the arriving node carried object.serial or
        // a pid — a real case (some Flatpak and remote-desktop clients) that
        // must stay eligible for flap tolerance rather than being refused by
        // an instance check neither side could ever pass.
        let mut t = FlapTracker::new(GRACE);
        let t0 = Instant::now();
        t.start_flap("obs", 9, None, t0);
        let t1 = t0 + Duration::from_millis(30);
        assert_eq!(
            t.on_reappear("obs", None, t1),
            Resume::Same {
                session_id: 9,
                gap: Duration::from_millis(30)
            }
        );
    }

    #[test]
    fn no_pending_flap_is_fresh() {
        let mut t = FlapTracker::new(GRACE);
        assert_eq!(
            t.on_reappear("anything", None, Instant::now()),
            Resume::Fresh
        );
    }

    #[test]
    fn expire_closes_out_a_flap_nobody_answered() {
        let mut t = FlapTracker::new(GRACE);
        let t0 = Instant::now();
        t.start_flap("VRChat.exe", 1, None, t0);
        assert!(
            t.expire(t0 + Duration::from_secs(1)).is_empty(),
            "still inside grace"
        );
        let expired = t.expire(t0 + GRACE + Duration::from_millis(1));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].match_key, "VRChat.exe");
        assert_eq!(expired[0].session_id, 1);
        assert_eq!(expired[0].flaps, 1);
    }

    /// The journal, replayed: 37 "capturing" lines in twelve minutes, gaps of
    /// milliseconds, for one source. Before this feature that was 19 sessions
    /// and one segment; the tracker must fold every one of those flaps back
    /// into the session the first `start_flap` opened, and log a single burst
    /// at the end rather than nineteen.
    #[test]
    fn the_2026_09_05_vrchat_run_collapses_to_one_session_and_one_burst_log() {
        let mut t = FlapTracker::new(GRACE);
        let start = Instant::now();
        let session_id = 100;
        let mut now = start;
        let mut resumes = 0;
        // 18 flap/reappear round trips (19 "capturing" lines total, one per
        // reappearance including the very first), each gap under a second —
        // well inside a 5 s grace window, and matching the journal's
        // "milliseconds apart" description.
        for i in 0..18u64 {
            t.start_flap("VRChat.exe", session_id, None, now);
            now += Duration::from_millis(150 + i * 10);
            match t.on_reappear("VRChat.exe", None, now) {
                Resume::Same {
                    session_id: sid, ..
                } => {
                    assert_eq!(sid, session_id, "every flap must resume the SAME session");
                    resumes += 1;
                }
                Resume::Fresh => panic!("flap #{i} should have resumed, not started fresh"),
            }
            now += Duration::from_millis(200);
        }
        assert_eq!(resumes, 18);
        assert!(t.pending_len() == 0);

        // The source now stays up for a full grace window: the burst has
        // settled and is owed exactly one summary line.
        let settled = t.settle(now + GRACE);
        assert_eq!(settled.len(), 1);
        assert_eq!(settled[0].match_key, "VRChat.exe");
        assert_eq!(settled[0].flaps, 18, "one summary line, not eighteen");

        // And it is reported exactly once — a second sweep with nothing new
        // finds nothing left to settle.
        assert!(t.settle(now + GRACE + Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn a_burst_that_finally_times_out_reports_its_whole_count_once() {
        let mut t = FlapTracker::new(GRACE);
        let t0 = Instant::now();
        let mut now = t0;
        for _ in 0..3 {
            t.start_flap("Steam", 5, None, now);
            now += Duration::from_millis(100);
            assert!(matches!(
                t.on_reappear("Steam", None, now),
                Resume::Same { .. }
            ));
            now += Duration::from_millis(50);
        }
        // The fourth time it does not come back.
        t.start_flap("Steam", 5, None, now);
        let expired = t.expire(now + GRACE + Duration::from_millis(1));
        assert_eq!(expired.len(), 1);
        assert_eq!(
            expired[0].flaps, 4,
            "the three resumed flaps plus the final one that gave up"
        );
        // The burst bookkeeping is gone with it — nothing left to settle later.
        assert!(t.settle(now + GRACE * 4).is_empty());
    }

    #[test]
    fn independent_sources_do_not_interfere() {
        let mut t = FlapTracker::new(GRACE);
        let t0 = Instant::now();
        t.start_flap("VRChat.exe", 1, None, t0);
        t.start_flap("Discord", 2, None, t0);
        let t1 = t0 + Duration::from_millis(80);
        assert_eq!(
            t.on_reappear("Discord", None, t1),
            Resume::Same {
                session_id: 2,
                gap: Duration::from_millis(80)
            }
        );
        // VRChat's own flap is untouched by Discord resuming.
        let expired = t.expire(t0 + GRACE + Duration::from_millis(1));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].match_key, "VRChat.exe");
    }
}
