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
//! - **The microphone switch** is the same story with a second counter, plus
//!   one flag going the other way: the capture thread is the only thing that
//!   knows whether the mic stream is really open, and `status` has to be able
//!   to say so.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::allowlist::Allowlist;
use crate::analysis::AnalysisStats;
use crate::clock::utc_now_ns;
use crate::config::{IdentityConfig, MicConfig, MicMode};
use crate::pipeline::Stats;
use crate::queue::EventQueue;

pub struct Control {
    paused: AtomicBool,
    paused_since_ns: AtomicU64,
    rules: Mutex<BTreeMap<String, bool>>,
    /// Bumped on every rule change. The capture loop compares it against what
    /// it last applied; equal means there is nothing to do.
    rules_gen: AtomicU64,
    /// The microphone switch, live. Separate from `rules` on purpose: the mic
    /// is not an application and is deliberately not expressible as one.
    mic: Mutex<MicConfig>,
    mic_gen: AtomicU64,
    /// Set by the capture thread when the mic stream is open. Read by `status`
    /// — "enabled" and "recording right now" are different facts and follow
    /// mode is the whole reason they differ.
    mic_active: AtomicBool,
    started_at_ns: i64,
    /// Where `sources.set` persists a rule, so a toggle survives a restart.
    pub config_path: Option<PathBuf>,
    pub data_dir: PathBuf,
    pub queue: Option<Arc<EventQueue>>,
    pub stats: Arc<Stats>,
    pub analysis: Arc<AnalysisStats>,
    /// The operating point the socket's own identity work reads —
    /// `speakers.split` re-clusters a voicebank and needs the same thresholds
    /// the pipeline was labelling with.
    pub identity: IdentityConfig,
    models: Mutex<Vec<String>>,
}

impl Control {
    pub fn new(data_dir: PathBuf, config_path: Option<PathBuf>, rules: &Allowlist) -> Arc<Self> {
        Arc::new(Self {
            paused: AtomicBool::new(false),
            paused_since_ns: AtomicU64::new(0),
            rules: Mutex::new(rules.as_map()),
            rules_gen: AtomicU64::new(0),
            mic: Mutex::new(MicConfig::default()),
            mic_gen: AtomicU64::new(0),
            mic_active: AtomicBool::new(false),
            started_at_ns: utc_now_ns(),
            config_path,
            data_dir,
            queue: None,
            stats: Arc::new(Stats::default()),
            analysis: Arc::new(AnalysisStats::default()),
            identity: IdentityConfig::default(),
            models: Mutex::new(Vec::new()),
        })
    }

    /// The configured identity operating point, if it differs from the
    /// defaults. Set before the daemon shares this handle, like the rest of the
    /// wiring.
    pub fn with_identity(mut self: Arc<Self>, identity: IdentityConfig) -> Arc<Self> {
        let this = Arc::get_mut(&mut self).expect("wiring happens before sharing");
        this.identity = identity;
        self
    }

    /// The configured microphone switch. Set before the handle is shared, like
    /// the rest of the wiring.
    pub fn with_mic(mut self: Arc<Self>, mic: MicConfig) -> Arc<Self> {
        let this = Arc::get_mut(&mut self).expect("wiring happens before sharing");
        *this.mic.get_mut().unwrap_or_else(|p| p.into_inner()) = mic;
        self
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

    // ---- microphone ------------------------------------------------------

    pub fn mic(&self) -> MicConfig {
        self.mic.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Flip the switch, or the mode, or both. Returns the resulting config so
    /// the caller answers with what is true rather than with what it asked for.
    pub fn set_mic(&self, enabled: Option<bool>, mode: Option<MicMode>) -> MicConfig {
        let mut guard = self.mic.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(e) = enabled {
            guard.enabled = e;
        }
        if let Some(m) = mode {
            guard.mode = m;
        }
        let out = guard.clone();
        drop(guard);
        self.mic_gen.fetch_add(1, Ordering::SeqCst);
        out
    }

    pub fn mic_generation(&self) -> u64 {
        self.mic_gen.load(Ordering::SeqCst)
    }

    /// The capture thread reporting whether the mic stream is actually open.
    pub fn set_mic_active(&self, active: bool) {
        self.mic_active.store(active, Ordering::SeqCst);
    }

    pub fn mic_active(&self) -> bool {
        self.mic_active.load(Ordering::SeqCst)
    }

    /// The one string `status` carries. Five states, and the pair that matters
    /// is `following:idle` vs `following:active`: enabled but waiting for an
    /// allowed application, versus recording the room right now.
    ///
    /// `always:idle` is the honest answer when the switch is on, the mode is
    /// `always`, and the daemon still has not managed to open a device — a
    /// missing microphone must not be able to read as "recording".
    pub fn mic_state(&self) -> &'static str {
        let cfg = self.mic();
        if !cfg.enabled {
            return "off";
        }
        match (cfg.mode, self.mic_active()) {
            (MicMode::Follow, false) => "following:idle",
            (MicMode::Follow, true) => "following:active",
            (MicMode::Always, false) => "always:idle",
            (MicMode::Always, true) => "always:active",
        }
    }

    /// The mic block every client-facing payload embeds, in one place so the
    /// `status` method, the `status` event and the `mic` event cannot drift.
    pub fn mic_json(&self) -> Value {
        let cfg = self.mic();
        json!({
            "enabled": cfg.enabled,
            "mode": cfg.mode.as_str(),
            "active": self.mic_active(),
            "state": self.mic_state(),
            "device": cfg.device_override(),
        })
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
    fn the_microphone_state_string_distinguishes_waiting_from_recording() {
        let c = control();
        // Off is off no matter what the capture thread last reported.
        assert_eq!(c.mic_state(), "off");
        c.set_mic_active(true);
        assert_eq!(c.mic_state(), "off");
        c.set_mic_active(false);

        c.set_mic(Some(true), None);
        assert_eq!(c.mic_state(), "following:idle");
        c.set_mic_active(true);
        assert_eq!(c.mic_state(), "following:active");

        c.set_mic(None, Some(crate::config::MicMode::Always));
        assert_eq!(c.mic_state(), "always:active");
        // A microphone that will not open must not read as recording.
        c.set_mic_active(false);
        assert_eq!(c.mic_state(), "always:idle");

        c.set_mic(Some(false), None);
        assert_eq!(c.mic_state(), "off");
    }

    #[test]
    fn a_mic_change_bumps_its_own_generation_not_the_rules_one() {
        let c = control();
        let rules0 = c.rules_generation();
        let mic0 = c.mic_generation();

        let after = c.set_mic(Some(true), Some(crate::config::MicMode::Always));
        assert!(after.enabled);
        assert_eq!(after.mode, crate::config::MicMode::Always);
        assert!(c.mic_generation() > mic0);
        // The microphone is not an allowlist rule and must never move one.
        assert_eq!(c.rules_generation(), rules0);
        assert!(!c.allowlist().as_map().contains_key("mic"));
    }

    #[test]
    fn the_mic_payload_says_the_same_thing_the_state_string_does() {
        let c = control();
        c.set_mic(Some(true), None);
        c.set_mic_active(true);
        let v = c.mic_json();
        assert_eq!(v["enabled"], serde_json::json!(true));
        assert_eq!(v["mode"], serde_json::json!("follow"));
        assert_eq!(v["active"], serde_json::json!(true));
        assert_eq!(v["state"], serde_json::json!("following:active"));
        assert_eq!(v["device"], serde_json::Value::Null);
    }

    #[test]
    fn the_starting_rules_come_from_the_config() {
        let c = control();
        let map = c.allowlist().as_map();
        assert_eq!(map.get("VRChat.exe"), Some(&true));
    }
}
