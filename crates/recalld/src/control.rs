//! Runtime state the socket can read and change while the daemon runs.
//!
//! Two things live here because two different threads own the code that reacts
//! to them:
//!
//! - **Pause** is read by the pipeline. It is the panic path, so it is an
//!   atomic flag checked before every write — not a message that has to reach
//!   the front of a queue. Capture keeps running (dropping the stream would
//!   cost a re-negotiation and lose the session), and nothing is written down:
//!   no segment rows, no WAVs, no transcripts, no roster.
//! - **The allowlist** is read by the PipeWire thread, which cannot be called
//!   into from outside its loop. It polls a generation counter instead, so
//!   `sources.set` attaches or detaches a capture without a restart.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::allowlist::Allowlist;
use crate::analysis::AnalysisStats;
use crate::clock::utc_now_ns;
use crate::pipeline::Stats;
use crate::queue::EventQueue;

pub struct Control {
    paused: AtomicBool,
    paused_since_ns: AtomicU64,
    rules: Mutex<BTreeMap<String, bool>>,
    /// Bumped on every rule change. The capture loop compares it against what
    /// it last applied; equal means there is nothing to do.
    rules_gen: AtomicU64,
    started_at_ns: i64,
    /// Where `sources.set` persists a rule, so a toggle survives a restart.
    pub config_path: Option<PathBuf>,
    pub data_dir: PathBuf,
    pub queue: Option<Arc<EventQueue>>,
    pub stats: Arc<Stats>,
    pub analysis: Arc<AnalysisStats>,
    models: Mutex<Vec<String>>,
}

impl Control {
    pub fn new(data_dir: PathBuf, config_path: Option<PathBuf>, rules: &Allowlist) -> Arc<Self> {
        Arc::new(Self {
            paused: AtomicBool::new(false),
            paused_since_ns: AtomicU64::new(0),
            rules: Mutex::new(rules.as_map()),
            rules_gen: AtomicU64::new(0),
            started_at_ns: utc_now_ns(),
            config_path,
            data_dir,
            queue: None,
            stats: Arc::new(Stats::default()),
            analysis: Arc::new(AnalysisStats::default()),
            models: Mutex::new(Vec::new()),
        })
    }

    /// The daemon's own wiring: the counters and the queue it reports on.
    pub fn with_pipeline(
        mut self: Arc<Self>,
        queue: Arc<EventQueue>,
        stats: Arc<Stats>,
        analysis: Arc<AnalysisStats>,
    ) -> Arc<Self> {
        let this = Arc::get_mut(&mut self).expect("wiring happens before sharing");
        this.queue = Some(queue);
        this.stats = stats;
        this.analysis = analysis;
        self
    }

    pub fn set_models(&self, ids: Vec<String>) {
        *self.lock_models() = ids;
    }

    pub fn models(&self) -> Vec<String> {
        self.lock_models().clone()
    }

    fn lock_models(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.models.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---- pause -----------------------------------------------------------

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Returns whether this call was the one that paused.
    pub fn pause(&self) -> bool {
        let was = self.paused.swap(true, Ordering::SeqCst);
        if !was {
            self.paused_since_ns
                .store(utc_now_ns() as u64, Ordering::Relaxed);
        }
        !was
    }

    pub fn resume(&self) -> bool {
        self.paused.swap(false, Ordering::SeqCst)
    }

    pub fn paused_since_ns(&self) -> Option<i64> {
        if self.is_paused() {
            Some(self.paused_since_ns.load(Ordering::Relaxed) as i64)
        } else {
            None
        }
    }

    // ---- allowlist -------------------------------------------------------

    pub fn allowlist(&self) -> Allowlist {
        Allowlist::from_rules(self.lock_rules().clone())
    }

    pub fn set_rule(&self, match_key: &str, allowed: bool) {
        self.lock_rules().insert(match_key.to_string(), allowed);
        self.rules_gen.fetch_add(1, Ordering::SeqCst);
    }

    pub fn rules_generation(&self) -> u64 {
        self.rules_gen.load(Ordering::SeqCst)
    }

    fn lock_rules(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, bool>> {
        self.rules.lock().unwrap_or_else(|p| p.into_inner())
    }

    // ---- status ----------------------------------------------------------

    pub fn uptime_s(&self) -> i64 {
        (utc_now_ns() - self.started_at_ns).max(0) / 1_000_000_000
    }

    pub fn started_at_ns(&self) -> i64 {
        self.started_at_ns
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control() -> Arc<Control> {
        Control::new(
            PathBuf::from("/nonexistent"),
            None,
            &Allowlist::from_rules([("VRChat.exe", true)]),
        )
    }

    #[test]
    fn pause_is_idempotent_and_reports_who_flipped_it() {
        let c = control();
        assert!(!c.is_paused());
        assert!(c.pause(), "the first pause is the one that pauses");
        assert!(!c.pause(), "a second pause changes nothing");
        assert!(c.is_paused());
        assert!(c.paused_since_ns().is_some());

        assert!(c.resume());
        assert!(!c.resume());
        assert!(!c.is_paused());
        assert_eq!(c.paused_since_ns(), None);
    }

    #[test]
    fn a_rule_change_bumps_the_generation_the_capture_loop_polls() {
        let c = control();
        let gen0 = c.rules_generation();
        assert!(c.allowlist().decide("VRChat.exe").captures());

        c.set_rule("Discord", true);
        assert!(c.rules_generation() > gen0);
        assert!(c.allowlist().decide("Discord").captures());

        let gen1 = c.rules_generation();
        c.set_rule("VRChat.exe", false);
        assert!(c.rules_generation() > gen1);
        assert!(!c.allowlist().decide("VRChat.exe").captures());
    }

    #[test]
    fn the_starting_rules_come_from_the_config() {
        let c = control();
        let map = c.allowlist().as_map();
        assert_eq!(map.get("VRChat.exe"), Some(&true));
    }
}
