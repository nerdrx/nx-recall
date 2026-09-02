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
use crate::config::{GraphConfig, IdentityConfig, MicConfig, MicMode};
use crate::enrich::GraphState;
use crate::pipeline::Stats;
use crate::queue::EventQueue;

/// What `[graph].llm_threads` may be set to, over the wire and in the UI.
///
/// One is "as little of the machine as the model can be given and still run";
/// thirty-two is past any consumer core count this daemon has ever been pointed
/// at, and well past the point where more threads stop buying tokens per second
/// on a Q4 3B. The number is a share of the machine, not a promise about
/// latency: whatever it is set to, the child is still pinned to
/// `[runtime] inference_cpus` and still runs at nice 19.
pub const GRAPH_THREADS: std::ops::RangeInclusive<i32> = 1..=32;

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
    /// Where the analysis models live, when `[models].dir` names anywhere at
    /// all. The socket needs it to answer whether the graph's optional model is
    /// actually on disk, which is a different question from whether it is on.
    pub models_root: Option<PathBuf>,
    pub queue: Option<Arc<EventQueue>>,
    pub stats: Arc<Stats>,
    pub analysis: Arc<AnalysisStats>,
    /// The operating point the socket's own identity work reads —
    /// `speakers.split` re-clusters a voicebank and needs the same thresholds
    /// the pipeline was labelling with.
    pub identity: IdentityConfig,
    /// The conversational language prior's thresholds and the arbiter's
    /// measured guards (0.7.7). Here for the same reason `identity` is: the
    /// socket runs the repair walk, and it has to run it with the daemon's
    /// numbers rather than with the defaults.
    pub lang: crate::config::LangConfig,
    models: Mutex<Vec<String>>,
    /// How much disk the program is using, as last measured by the retention
    /// sweeper (and once at start-up). Cached rather than computed on demand
    /// because `status` is polled every three seconds by every open client and
    /// the answer costs a walk of the whole data directory.
    storage: Mutex<Option<crate::retention::StorageUsage>>,
    /// What the retention sweeper did on its last pass. `None` until one has
    /// run — a client renders that as "not swept yet", which is honest, where
    /// a block of zeroes would claim a clean sweep that never happened.
    last_sweep: Mutex<Option<Value>>,
    /// The memory graph's settings, live. The Tier 3 switch is the third thing
    /// in this file that has to be movable while the daemon runs, and for the
    /// same reason as the other two: it has a switch in the UI, and a switch
    /// that needs a restart is not a switch.
    graph: Mutex<GraphConfig>,
    /// What the enrichment worker is doing right now, as it reports it.
    graph_state: Mutex<GraphState>,
    /// The accuracy round's idle worker (0.8.0), live for the same reason the
    /// graph's settings are: both its passes have switches.
    asr: Mutex<crate::config::AsrConfig>,
    /// What that worker has done since the daemon started.
    pub quality: Arc<crate::quality::QualityStats>,
    /// The night shift's settings (0.9.0), live for the same reason: it has a
    /// switch, and a switch that needs a restart is not a switch.
    night: Mutex<crate::config::NightConfig>,
    /// What the night shift has done since the daemon started.
    pub night_stats: Arc<crate::night::NightStats>,
    // ---- 0.9.0, the assistant -------------------------------------------
    /// Reminders, digests and translation. Live like `asr` and `graph` and for
    /// the same reason: every one of them has a switch.
    assist: Mutex<crate::config::AssistConfig>,
    /// What the assistant worker has done, read back from the rows.
    pub assist_stats: Arc<crate::assist::AssistStats>,
    // ---- end 0.9.0 -------------------------------------------------------
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
            models_root: None,
            queue: None,
            stats: Arc::new(Stats::default()),
            analysis: Arc::new(AnalysisStats::default()),
            identity: IdentityConfig::default(),
            lang: crate::config::LangConfig::default(),
            models: Mutex::new(Vec::new()),
            storage: Mutex::new(None),
            last_sweep: Mutex::new(None),
            graph: Mutex::new(GraphConfig::default()),
            graph_state: Mutex::new(GraphState::default()),
            asr: Mutex::new(crate::config::AsrConfig::default()),
            quality: Arc::new(crate::quality::QualityStats::default()),
            night: Mutex::new(crate::config::NightConfig::default()),
            night_stats: Arc::new(crate::night::NightStats::default()),
            // 0.9.0.
            assist: Mutex::new(crate::config::AssistConfig::default()),
            assist_stats: Arc::new(crate::assist::AssistStats::default()),
        })
    }

    /// The configured memory graph, and where its optional model would live.
    /// Set before the handle is shared, like the rest of the wiring.
    pub fn with_graph(
        mut self: Arc<Self>,
        graph: GraphConfig,
        models_root: Option<PathBuf>,
    ) -> Arc<Self> {
        let this = Arc::get_mut(&mut self).expect("wiring happens before sharing");
        *this.graph.get_mut().unwrap_or_else(|p| p.into_inner()) = graph;
        this.models_root = models_root;
        self
    }

    /// The configured identity operating point, if it differs from the
    /// defaults. Set before the daemon shares this handle, like the rest of the
    /// wiring.
    pub fn with_identity(mut self: Arc<Self>, identity: IdentityConfig) -> Arc<Self> {
        let this = Arc::get_mut(&mut self).expect("wiring happens before sharing");
        this.identity = identity;
        self
    }

    /// The configured language prior. Set before the handle is shared, like the
    /// rest of the wiring.
    pub fn with_lang(mut self: Arc<Self>, lang: crate::config::LangConfig) -> Arc<Self> {
        let this = Arc::get_mut(&mut self).expect("wiring happens before sharing");
        this.lang = lang;
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

    // ---- storage ---------------------------------------------------------

    /// The sweeper reporting what it just measured.
    pub fn set_storage(&self, usage: crate::retention::StorageUsage) {
        *self.storage.lock().unwrap_or_else(|p| p.into_inner()) = Some(usage);
    }

    pub fn storage(&self) -> Option<crate::retention::StorageUsage> {
        *self.storage.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The `storage` block `status` carries, or `null` before the first sweep
    /// has measured anything. Null is an honest answer and a client renders it
    /// as "not measured yet"; zeroes would be a lie about an empty disk.
    pub fn storage_json(&self) -> Value {
        match self.storage() {
            Some(usage) => usage.to_json(),
            None => Value::Null,
        }
    }

    /// The sweeper reporting what it just did (`retention::SweepReport`).
    pub fn set_last_sweep(&self, report: Value) {
        *self.last_sweep.lock().unwrap_or_else(|p| p.into_inner()) = Some(report);
    }

    /// The `last_sweep` block `status` carries, or `null` before the first
    /// sweep. Unlink failures, orphan removals and dangling paths reached no
    /// client at all before this (audit finding #22).
    pub fn last_sweep_json(&self) -> Value {
        self.last_sweep
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .unwrap_or(Value::Null)
    }

    // ---- the accuracy round's idle worker (0.8.0) ------------------------

    pub fn asr(&self) -> crate::config::AsrConfig {
        self.asr.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Point the worker at the running config. Set before the handle is shared,
    /// like the rest of the wiring.
    pub fn with_asr(mut self: Arc<Self>, cfg: crate::config::AsrConfig) -> Arc<Self> {
        let this = Arc::get_mut(&mut self).expect("wiring happens before sharing");
        *this.asr.get_mut().unwrap_or_else(|p| p.into_inner()) = cfg;
        self
    }

    // ---- the night shift (0.9.0) -----------------------------------------

    pub fn night(&self) -> crate::config::NightConfig {
        self.night.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Point the night shift at the running config. Set before the handle is
    /// shared, like the rest of the wiring.
    pub fn with_night(mut self: Arc<Self>, cfg: crate::config::NightConfig) -> Arc<Self> {
        let this = Arc::get_mut(&mut self).expect("wiring happens before sharing");
        *this.night.get_mut().unwrap_or_else(|p| p.into_inner()) = cfg;
        self
    }

    /// Minutes since the last turn was written, or since the daemon started if
    /// none has been.
    ///
    /// This is what "the machine is idle" means here, and it is deliberately
    /// about *capture* rather than about input devices: the night shift's
    /// question is whether transcription is still happening, not whether
    /// somebody is at the keyboard. A machine playing a film with no allowed
    /// application open is idle by this definition, and that is correct — the
    /// GPU check is the separate gate that covers the film.
    pub fn idle_minutes(&self) -> i64 {
        let last = self
            .stats
            .last_segment_ns
            .load(std::sync::atomic::Ordering::Relaxed);
        let since = if last > 0 { last } else { self.started_at_ns };
        (crate::clock::utc_now_ns() - since).max(0) / 60_000_000_000
    }
    // ---- the assistant round (0.9.0) -------------------------------------

    pub fn assist(&self) -> crate::config::AssistConfig {
        self.assist
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// Point the assistant at the running config. Set before the handle is
    /// shared, like the rest of the wiring.
    pub fn with_assist(mut self: Arc<Self>, cfg: crate::config::AssistConfig) -> Arc<Self> {
        let this = Arc::get_mut(&mut self).expect("wiring happens before sharing");
        *this.assist.get_mut().unwrap_or_else(|p| p.into_inner()) = cfg;
        self
    }

    // ---- end 0.9.0 --------------------------------------------------------

    // ---- the memory graph (GRAPH.md Tiers 2 and 3) -----------------------

    pub fn graph(&self) -> GraphConfig {
        self.graph.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Flip the Tier 3 switch. Returns the resulting settings, so the caller
    /// answers with what is true rather than with what it asked for.
    ///
    /// Nothing is interrupted here: the worker reads this between conversations
    /// and stands down at the next boundary, which is at most one model call
    /// away. Killing a running inference to honour a switch instantly would
    /// leave a conversation half-annotated for no benefit anybody can see.
    pub fn set_graph_enabled(&self, enabled: bool) -> GraphConfig {
        let mut guard = self.graph.lock().unwrap_or_else(|p| p.into_inner());
        guard.enabled = enabled;
        guard.clone()
    }

    /// Change one or more of the numbers the worker runs under. `None` leaves a
    /// field alone, so a client can set the switch without also having an
    /// opinion about thread counts.
    ///
    /// Both are clamped rather than trusted. `llm_threads` is the one a person
    /// actually turns (0.7.2 puts it in the Memory tab): [`GRAPH_THREADS`] is
    /// its range, and it is a range rather than a free integer because the
    /// number is how much of the machine the model may use while a game is
    /// running — the setting that replaced standing down.
    pub fn set_graph_tuning(
        &self,
        llm_threads: Option<i32>,
        gpu_layers: Option<i32>,
    ) -> GraphConfig {
        let mut guard = self.graph.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(t) = llm_threads {
            guard.llm_threads = t.clamp(*GRAPH_THREADS.start(), *GRAPH_THREADS.end());
        }
        if let Some(g) = gpu_layers {
            guard.gpu_layers = g.max(0);
        }
        guard.clone()
    }

    pub fn graph_state(&self) -> GraphState {
        self.graph_state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The worker reporting what it is doing. Returns whether anything a client
    /// would notice actually changed, so a loop that ticks every twenty seconds
    /// does not push twenty identical events an hour.
    pub fn set_graph_state(&self, next: GraphState) -> bool {
        let mut guard = self.graph_state.lock().unwrap_or_else(|p| p.into_inner());
        let changed = !guard.same_to_a_client(&next);
        *guard = next;
        changed
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
