//! On-disk configuration: `$XDG_CONFIG_HOME/nx-recall/config.toml`.
//!
//! Step 1 reads the file once at start-up. Hot reload is deliberately out of
//! scope; `recalld allow`/`deny` rewrite the file and the daemon is restarted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::allowlist::Allowlist;

pub const SAMPLE_RATE: u32 = 16_000;

/// A rule may be written as either `"binary" = true` or
/// `"binary" = { allowed = true }`; both are common in hand-edited TOML.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Rule {
    Flag(bool),
    Table { allowed: bool },
}

impl Rule {
    pub fn allowed(&self) -> bool {
        match self {
            Rule::Flag(b) => *b,
            Rule::Table { allowed } => *allowed,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaptureConfig {
    /// Depth of the capture -> VAD hand-off, in seconds of 16 kHz mono audio.
    pub queue_seconds: f32,
    /// Requested PipeWire buffer size, in samples at 16 kHz.
    pub quantum: u32,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            queue_seconds: 30.0,
            quantum: 1024,
        }
    }
}

/// When the microphone stream is open.
///
/// `Follow` is the default and the reason this feature is acceptable at all:
/// the microphone hears the *room*, so it is only open while an allowed
/// application is itself being captured — VRChat running means a conversation
/// is happening; VRChat closed means the room is nobody's business. `Always` is
/// a deliberate second choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MicMode {
    #[default]
    Follow,
    Always,
}

impl MicMode {
    pub fn as_str(self) -> &'static str {
        match self {
            MicMode::Follow => "follow",
            MicMode::Always => "always",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "follow" => Some(MicMode::Follow),
            "always" => Some(MicMode::Always),
            _ => None,
        }
    }
}

/// The user's own microphone as a capture source (DESIGN §5: "mic loopback and
/// push-to-talk boundaries add more [independent signal] when available").
///
/// Deliberately **not** a `[rules]` entry. An allowlist rule is consent about
/// one program's output; this device picks up whoever is in the room, including
/// people who never joined the instance, so it gets its own switch, its own
/// default (off), and its own copy in the UI. `sources.set` refuses the `mic`
/// key for exactly that reason — see `mic.set` in PROTOCOL.md.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct MicConfig {
    pub enabled: bool,
    pub mode: MicMode,
    /// PipeWire `node.name` to capture instead of the default source. Unset
    /// (the normal case) follows whatever the session manager calls default, so
    /// swapping a headset moves the tap instead of breaking it.
    pub device: Option<String>,
    /// How many golden samples the mic's free enrolment is allowed to keep.
    /// Goldens live outside the retention window (DESIGN §6), so the cap is the
    /// only thing bounding them.
    pub max_goldens: usize,
}

impl Default for MicConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: MicMode::Follow,
            device: None,
            max_goldens: 3,
        }
    }
}

impl MicConfig {
    /// The `node.name` this should be capturing, or `None` for "whatever the
    /// session manager routes a plain capture stream to".
    pub fn device_override(&self) -> Option<&str> {
        self.device
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct VadConfig {
    pub threshold: f32,
    pub min_speech_ms: u32,
    pub min_silence_ms: u32,
    pub pad_ms: u32,
    pub max_segment_ms: u32,
    /// Adjacent VAD segments closer together than this are stored as one turn.
    /// Step 0 measured +70% mean segment length and −42% spurious identities
    /// from merging at 1.5 s, so a "segment" downstream is really a turn.
    pub turn_merge_gap_ms: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            threshold: 0.5,
            min_speech_ms: 250,
            min_silence_ms: 500,
            pad_ms: 200,
            max_segment_ms: 30_000,
            turn_merge_gap_ms: 1_500,
        }
    }
}

/// Paths to the analysis models. Every other entry is resolved against `dir`;
/// omitting `dir` turns the analysis leg off entirely and the daemon behaves
/// exactly as it did in Step 1. Nothing here is ever downloaded.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelsConfig {
    pub dir: Option<PathBuf>,
    /// Transducer export directory. A name the catalogue publishes is a
    /// *preference*: `ModelSet::select_asr` resolves it against what is on
    /// disk, so a config still carrying the 0.5.5 English-only default picks
    /// the multilingual set up as soon as it is fetched, and a machine that
    /// only has the old one keeps transcribing on it. Any other name is a
    /// deliberate pin and is used as written.
    pub asr: String,
    pub asr_encoder: String,
    pub asr_decoder: String,
    pub asr_joiner: String,
    pub asr_tokens: String,
    pub embedding: String,
    pub segmentation: String,
    /// ASR is the only stage where throughput is worth threads. Step 0 measured
    /// RTF 0.011 at four on the 110m; the multilingual default is ~3x the
    /// compute and still RTF 0.08 on a *single* thread, so four remains far
    /// more headroom than the workload needs.
    pub asr_threads: i32,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            dir: None,
            asr: crate::models::DEFAULT_ASR.dir.into(),
            asr_encoder: crate::models::DEFAULT_ASR.encoder.into(),
            asr_decoder: crate::models::DEFAULT_ASR.decoder.into(),
            asr_joiner: crate::models::DEFAULT_ASR.joiner.into(),
            asr_tokens: crate::models::DEFAULT_ASR.tokens.into(),
            embedding: "eres2net_en.onnx".into(),
            segmentation: "sherpa-onnx-pyannote-segmentation-3-0/model.onnx".into(),
            asr_threads: 4,
        }
    }
}

/// Speaker-identity operating point.
///
/// The two thresholds are deliberately separate numbers with separate
/// justifications: `label_threshold` is field-calibrated on real lobby audio
/// (the corpus-calibrated 0.45 over-splits real speech roughly 3x), while
/// enrolment is gated far higher *and* on an independent single-speaker signal,
/// because a confident cosine score is provably no defence against equal-loudness
/// overlap.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct IdentityConfig {
    pub label_threshold: f32,
    pub enroll_threshold: f32,
    pub enroll_margin: f32,
    pub enroll_max_overlap: f32,
    pub enroll_min_duration_s: f32,
    /// Segments above this overlap fraction get no speaker at all.
    pub max_overlap: f32,
    pub min_duration_s: f32,
    pub max_prototypes: usize,
    /// `speakers.split`: refuse the split when the two candidate centroids are
    /// at least this similar. Two centroids that close are one voice being cut
    /// in half, and a false split is as damaging as the false merge it was
    /// meant to undo.
    pub split_max_centroid_similarity: f32,
    /// `speakers.split`: a segment whose similarities to the two centroids
    /// differ by less than this is *undecidable*. It keeps the existing speaker
    /// and is recorded at the weaker of the two scores, because guessing here
    /// is exactly how a false merge was created in the first place.
    pub split_ambiguous_margin: f32,
    /// `speakers.split`: seeded restarts of the 2-means search. The seed is
    /// fixed, so the same voicebank always splits the same way.
    pub split_restarts: usize,
    /// **The mint bar, which sits above the label bar (0.6.1).** A turn may
    /// match an existing voice at `label_threshold`, but *minting a new one*
    /// additionally needs this much audio. A grunt — "hm", a laugh, one
    /// syllable through a door — is not an identity, and every one of them that
    /// mints becomes a permanent row in the voicebank the user then has to
    /// sweep up.
    pub mint_min_duration_s: f32,
    /// The other half of the mint bar: a new voice needs words, not just
    /// seconds. Two is the cheapest honest test — one word is "yeah".
    pub mint_min_words: usize,
    /// Proximity inheritance (0.6.1): how far either side of an unlabelled
    /// short turn the daemon will look for a confident neighbour to inherit
    /// from. Beyond this the silence is long enough that somebody else may have
    /// started talking.
    pub proximity_gap_s: f32,
}

impl Default for IdentityConfig {
    fn default() -> Self {
        Self {
            label_threshold: 0.35,
            enroll_threshold: 0.55,
            enroll_margin: 0.06,
            enroll_max_overlap: 0.05,
            enroll_min_duration_s: 3.0,
            max_overlap: 0.1,
            min_duration_s: 1.0,
            max_prototypes: 20,
            split_max_centroid_similarity: 0.6,
            split_ambiguous_margin: 0.05,
            split_restarts: 8,
            mint_min_duration_s: 2.0,
            mint_min_words: 2,
            proximity_gap_s: 2.5,
        }
    }
}

/// The memory graph (docs/GRAPH.md).
///
/// Tiers 1 and 2 are deterministic, always on, and have exactly one knob
/// between them — how long a silence ends a conversation. Everything else here
/// belongs to **Tier 3**, the tiny local model, and the first field is the one
/// that matters: `enabled` is **false**, and that is the shipped default.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GraphConfig {
    /// Silence, in seconds, after which the next turn starts a new
    /// conversation rather than continuing the last one. Twenty is a long
    /// pause in speech and a short one in an evening: below it the same people
    /// are still talking, above it the room has moved on.
    pub thread_gap_s: f32,

    // ---- Tier 3 (GRAPH.md: "opt-in and idle-only") ----------------------
    /// Run the local model over conversations nobody has looked at yet.
    ///
    /// **Off, and off is the default.** GRAPH.md: "Off by default. Its switch
    /// sits next to the mic's, with equally plain copy." Turning it on costs a
    /// 1.9 GB model on disk and four cores of somebody else's idle time; it
    /// never runs while a game is being captured and never in the capture path.
    pub enabled: bool,
    /// Threads handed to llama.cpp (`-t`). Four is the user's stated budget and
    /// the number the bake-off measured on: 3.3 s/case for Qwen2.5-3B Q4.
    pub llm_threads: i32,
    /// Layers offloaded to the GPU (`-ngl`). Zero, and meant to stay zero: the
    /// GPU belongs to whatever is drawing frames. It exists for a machine that
    /// is not gaming at all.
    pub gpu_layers: i32,
    /// Ceiling on one model call, in seconds, after which the child is killed.
    /// Generous next to a 3.3 s median because a cold page-in of 1.9 GB is not
    /// a hang — but finite, because a wedged child holding four cores is.
    pub llm_timeout_s: u64,
    /// Conversations per enrichment batch. The worker stops between batches to
    /// re-check every gate, so this is really "how long the worker commits to
    /// before looking up again".
    pub batch_threads: usize,
    /// Seconds to wait between batches, and between re-checks when a gate is
    /// closed. Idle work has no deadline; being invisible matters more.
    pub batch_pause_s: u64,
    /// Conversations shorter than this are skipped: two lines of transcript
    /// carry no commitment and no topic worth the name, and walking them would
    /// spend the whole budget on nothing.
    pub min_thread_segments: i64,
    /// The most turns of one conversation the model is shown at once. A window,
    /// not a summary: the bake-off ran on short windows because that is the
    /// regime a 3B model is strong in.
    pub llm_window_turns: usize,
    /// Queue depth, in seconds of audio waiting for the inference thread, above
    /// which the worker stands down. A turn storm means the machine is busy
    /// being a tape recorder, which is the job that matters.
    pub max_queue_seconds: i64,
    /// The GGUF, relative to `[models].dir`.
    pub llm_model: String,
    /// The llama.cpp binaries, relative to `[models].dir`. `llama-cli` and the
    /// shared objects it dlopens both live here.
    pub llama_dir: String,
}

impl Default for GraphConfig {
    fn default() -> Self {
        Self {
            thread_gap_s: 20.0,
            enabled: false,
            llm_threads: 4,
            gpu_layers: 0,
            llm_timeout_s: 180,
            batch_threads: 4,
            batch_pause_s: 20,
            min_thread_segments: 3,
            llm_window_turns: 12,
            max_queue_seconds: 5,
            llm_model: "qwen2.5-3b-instruct-q4_k_m.gguf".into(),
            llama_dir: "llama".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    /// nice value applied to the inference thread. The project rule is that
    /// analysis must never win a scheduling contest against a VR frame.
    pub inference_nice: i32,
    /// Optional CPU pin for the inference thread. On the 9950X3D the game keeps
    /// the X3D die (0-15) and inference is confined to 16-31. Empty = no pin.
    pub inference_cpus: Vec<usize>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            inference_nice: 19,
            inference_cpus: Vec::new(),
        }
    }
}

/// The control socket. `$XDG_RUNTIME_DIR/nx-recall.sock` by default (DESIGN §8);
/// `path`, or `NXR_SOCKET` in the environment, overrides it — which is what
/// keeps the test suite off the live daemon's socket.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SocketConfig {
    pub enabled: bool,
    pub path: Option<PathBuf>,
    /// How many past events the daemon can replay to a reconnecting client
    /// before it has to answer `resync`.
    pub replay_events: usize,
    /// Per-client outbox depth. A client that cannot keep up is disconnected
    /// at this bound rather than being allowed to stall the pipeline.
    pub client_outbox: usize,
}

impl Default for SocketConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: None,
            replay_events: 10_000,
            client_outbox: 256,
        }
    }
}

/// The VRChat roster tailer (DESIGN §7). `log_dir` overrides the discovered
/// Proton prefix; the tailer never errors the daemon when there is nothing to
/// read, it just keeps looking.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RosterConfig {
    pub enabled: bool,
    pub log_dir: Option<PathBuf>,
    /// How often to read new lines from the current log.
    pub poll_ms: u64,
    /// How often to look again when no log directory exists at all.
    pub retry_s: u64,
}

impl Default for RosterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            log_dir: None,
            poll_ms: 500,
            retry_s: 30,
        }
    }
}

/// Tiered retention (DESIGN §8): audio is the heavy, short-lived artifact;
/// text and identity are the light, long-lived ones. `0` disables a tier —
/// "keep it" for the age limits, "purge immediately" is not expressible on
/// purpose.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    pub enabled: bool,
    /// How often the sweeper runs.
    pub sweep_interval_s: u64,
    /// How long a soft-deleted row stays undoable before it is really gone.
    pub undo_window_days: u32,
    /// Age at which a segment's audio file is dropped; the transcript stays.
    pub audio_days: u32,
    /// Reconcile loose files against the database: orphaned files are removed,
    /// dangling paths are logged. Both are crash residue (DESIGN §6).
    pub reconcile: bool,
    /// `VACUUM` after a sweep that actually purged rows.
    pub vacuum_after_purge: bool,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sweep_interval_s: 6 * 3600,
            undo_window_days: 7,
            audio_days: 30,
            reconcile: true,
            vacuum_after_purge: true,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub capture: CaptureConfig,
    pub mic: MicConfig,
    pub vad: VadConfig,
    pub runtime: RuntimeConfig,
    pub models: ModelsConfig,
    pub identity: IdentityConfig,
    pub graph: GraphConfig,
    pub socket: SocketConfig,
    pub roster: RosterConfig,
    pub retention: RetentionConfig,
    /// Keyed on the match key (see `allowlist::SourceIdent::match_key`).
    pub rules: BTreeMap<String, Rule>,
}

impl Config {
    pub fn allowlist(&self) -> Allowlist {
        Allowlist::from_rules(self.rules.iter().map(|(k, v)| (k.clone(), v.allowed())))
    }

    pub fn set_rule(&mut self, match_key: &str, allowed: bool) {
        self.rules
            .insert(match_key.to_string(), Rule::Table { allowed });
    }

    /// Load from `path`. A missing file is not an error — it means "capture
    /// nothing yet", which is the correct default-deny starting state.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        Ok(cfg)
    }

    /// Serialise back out. Comments in the original file are not preserved.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let body = toml::to_string_pretty(self).context("serialising config")?;
        let text = format!(
            "# NX Recall daemon configuration.\n\
             #\n\
             # Capture is default-deny: a program is recorded only if it has an\n\
             # explicit `allowed = true` rule below. Keys are the process binary\n\
             # (`application.process.binary`), except for Wine programs, which are\n\
             # keyed on the PE name (`VRChat.exe`) because every Wine program shares\n\
             # one loader binary.\n\
             #\n\
             # `recalld sources` lists everything seen so far; `recalld allow <key>`\n\
             # and `recalld deny <key>` edit this file. Restart the daemon to apply.\n\
             #\n\
             # Transcription and speaker identity stay off until `[models].dir`\n\
             # points at a directory holding the ONNX models; `recalld models\n\
             # status` reports what is present.\n\
             #\n\
             # `[mic]` is the one source that is NOT an application rule: it is\n\
             # the default microphone, it hears the ROOM rather than a program,\n\
             # and it is off until you turn it on with `recalld mic on`. In the\n\
             # default \"follow\" mode it only records while an allowed program is\n\
             # itself being captured.\n\
             \n{body}"
        );
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("installing {}", path.display()))?;
        Ok(())
    }
}

/// `$XDG_CONFIG_HOME/nx-recall/config.toml`, else `~/.config/nx-recall/config.toml`.
pub fn default_config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().context("no config directory (XDG_CONFIG_HOME / HOME unset)")?;
    Ok(base.join("nx-recall").join("config.toml"))
}

/// `$XDG_DATA_HOME/nx-recall`, else `~/.local/share/nx-recall`.
pub fn default_data_dir() -> Result<PathBuf> {
    let base = dirs::data_dir().context("no data directory (XDG_DATA_HOME / HOME unset)")?;
    Ok(base.join("nx-recall"))
}

/// Where the control socket lives, in precedence order: the config file, then
/// `NXR_SOCKET`, then `$XDG_RUNTIME_DIR/nx-recall.sock`, then a path under the
/// data dir for systems without a runtime dir.
pub fn socket_path(cfg: &SocketConfig, data_dir: &Path) -> PathBuf {
    if let Some(p) = &cfg.path {
        return p.clone();
    }
    if let Some(p) = std::env::var_os("NXR_SOCKET").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(dir) = dirs::runtime_dir() {
        return dir.join("nx-recall.sock");
    }
    data_dir.join("nx-recall.sock")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::allowlist::Decision;

    #[test]
    fn missing_file_yields_a_default_deny_config() {
        let cfg = Config::load(Path::new("/nonexistent/nx-recall/config.toml")).unwrap();
        assert!(cfg.rules.is_empty());
        assert_eq!(cfg.allowlist().decide("anything"), Decision::Unknown);
        assert_eq!(cfg.vad.threshold, 0.5);
        assert_eq!(cfg.capture.queue_seconds, 30.0);
    }

    #[test]
    fn both_rule_spellings_parse() {
        let cfg: Config = toml::from_str(
            r#"
            [rules]
            "VRChat.exe" = true
            "Discord" = { allowed = true }
            "firefox" = { allowed = false }
            "#,
        )
        .unwrap();
        let list = cfg.allowlist();
        assert_eq!(list.decide("VRChat.exe"), Decision::Allow);
        assert_eq!(list.decide("Discord"), Decision::Allow);
        assert_eq!(list.decide("firefox"), Decision::Deny);
        assert_eq!(list.decide("Moonlight"), Decision::Unknown);
    }

    #[test]
    fn partial_sections_keep_the_remaining_defaults() {
        let cfg: Config = toml::from_str("[vad]\nthreshold = 0.7\n").unwrap();
        assert_eq!(cfg.vad.threshold, 0.7);
        assert_eq!(cfg.vad.min_speech_ms, 250);
        assert_eq!(cfg.vad.max_segment_ms, 30_000);
        assert_eq!(cfg.runtime.inference_nice, 19);
    }

    #[test]
    fn save_then_load_round_trips_rules() {
        let dir = std::env::temp_dir().join(format!("nx-recall-cfg-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);

        let mut cfg = Config::default();
        cfg.set_rule("VRChat.exe", true);
        cfg.set_rule("firefox", false);
        cfg.vad.threshold = 0.42;
        cfg.save(&path).unwrap();

        let back = Config::load(&path).unwrap();
        assert_eq!(back.allowlist().decide("VRChat.exe"), Decision::Allow);
        assert_eq!(back.allowlist().decide("firefox"), Decision::Deny);
        assert_eq!(back.vad.threshold, 0.42);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_identity_operating_point_has_the_measured_defaults() {
        let cfg = Config::default();
        // Field-calibrated, not corpus-calibrated: 0.45 over-splits real speech
        // roughly threefold.
        assert_eq!(cfg.identity.label_threshold, 0.35);
        // Enrolling is a separate, stricter decision.
        assert_eq!(cfg.identity.enroll_threshold, 0.55);
        assert_eq!(cfg.identity.enroll_margin, 0.06);
        assert_eq!(cfg.identity.enroll_max_overlap, 0.05);
        assert_eq!(cfg.identity.enroll_min_duration_s, 3.0);
        assert_eq!(cfg.identity.max_overlap, 0.1);
        assert_eq!(cfg.identity.min_duration_s, 1.0);
        assert_eq!(cfg.identity.max_prototypes, 20);
        // Undoing a false merge must not be able to invent a second person:
        // two centroids this close are one voice.
        assert_eq!(cfg.identity.split_max_centroid_similarity, 0.6);
        assert_eq!(cfg.identity.split_ambiguous_margin, 0.05);
        assert_eq!(cfg.identity.split_restarts, 8);
        assert_eq!(cfg.vad.turn_merge_gap_ms, 1_500);
    }

    #[test]
    fn models_default_to_off_and_to_the_measured_default_asr() {
        let cfg = Config::default();
        assert!(cfg.models.dir.is_none());
        // Multilingual by default since 0.5.6: the English-only export scores
        // 103% WER on German, and German is half of what this daemon hears.
        assert_eq!(cfg.models.asr, "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8");
        assert_eq!(cfg.models.asr_encoder, "encoder.int8.onnx");
        assert_eq!(cfg.models.asr_tokens, "tokens.txt");
        assert_eq!(cfg.models.embedding, "eres2net_en.onnx");
    }

    #[test]
    fn the_new_sections_parse_and_keep_the_remaining_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            [models]
            dir = "/srv/models"

            [identity]
            label_threshold = 0.30
            "#,
        )
        .unwrap();
        assert_eq!(cfg.models.dir.unwrap().to_str().unwrap(), "/srv/models");
        assert_eq!(cfg.identity.label_threshold, 0.30);
        assert_eq!(cfg.identity.enroll_threshold, 0.55);
        assert_eq!(cfg.models.asr_threads, 4);
    }

    #[test]
    fn the_step_4_sections_have_their_documented_defaults() {
        let cfg = Config::default();
        assert!(cfg.socket.enabled);
        assert!(cfg.socket.path.is_none());
        assert_eq!(cfg.socket.replay_events, 10_000);
        assert_eq!(cfg.socket.client_outbox, 256);
        assert!(cfg.roster.enabled);
        assert_eq!(cfg.roster.poll_ms, 500);
        // Tiered retention: audio is the short-lived tier, text outlives it.
        assert_eq!(cfg.retention.audio_days, 30);
        assert_eq!(cfg.retention.undo_window_days, 7);
        assert_eq!(cfg.retention.sweep_interval_s, 6 * 3600);
        assert!(cfg.retention.reconcile);
    }

    #[test]
    fn an_explicit_socket_path_wins_over_the_runtime_dir() {
        let sock = SocketConfig {
            path: Some(PathBuf::from("/tmp/isolated.sock")),
            ..Default::default()
        };
        assert_eq!(
            socket_path(&sock, Path::new("/data")),
            PathBuf::from("/tmp/isolated.sock")
        );
        // With no override and no runtime dir the data dir is the fallback, so
        // the daemon always has somewhere to listen.
        let bare = SocketConfig::default();
        let resolved = socket_path(&bare, Path::new("/data"));
        assert!(resolved.ends_with("nx-recall.sock"));
    }

    #[test]
    fn the_microphone_is_off_and_following_until_it_is_asked_otherwise() {
        let cfg = Config::default();
        // The ordering IS the feature: off by default, and when on, only while
        // an allowed application is being captured.
        assert!(!cfg.mic.enabled);
        assert_eq!(cfg.mic.mode, MicMode::Follow);
        assert_eq!(cfg.mic.device_override(), None);
        assert_eq!(cfg.mic.max_goldens, 3);
    }

    #[test]
    fn the_mic_section_parses_both_modes_and_a_device_pin() {
        let cfg: Config = toml::from_str(
            r#"
            [mic]
            enabled = true
            mode = "always"
            device = "alsa_input.usb-Blue_Yeti"
            "#,
        )
        .unwrap();
        assert!(cfg.mic.enabled);
        assert_eq!(cfg.mic.mode, MicMode::Always);
        assert_eq!(cfg.mic.device_override(), Some("alsa_input.usb-Blue_Yeti"));
        // Everything else keeps its default.
        assert_eq!(cfg.mic.max_goldens, 3);

        let back: Config = toml::from_str("[mic]\nmode = \"follow\"\n").unwrap();
        assert_eq!(back.mic.mode, MicMode::Follow);
        // A blank device string is "no override", not a node named "".
        let blank: Config = toml::from_str("[mic]\ndevice = \"  \"\n").unwrap();
        assert_eq!(blank.mic.device_override(), None);
    }

    #[test]
    fn a_nonsense_mic_mode_is_refused_rather_than_defaulted() {
        assert!(toml::from_str::<Config>("[mic]\nmode = \"sometimes\"\n").is_err());
        assert_eq!(MicMode::parse("Always"), Some(MicMode::Always));
        assert_eq!(MicMode::parse("follow"), Some(MicMode::Follow));
        assert_eq!(MicMode::parse("whenever"), None);
    }

    #[test]
    fn the_mic_switch_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!("nx-recall-mic-cfg-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);

        let mut cfg = Config::default();
        cfg.mic.enabled = true;
        cfg.mic.mode = MicMode::Always;
        cfg.save(&path).unwrap();

        let back = Config::load(&path).unwrap();
        assert!(back.mic.enabled);
        assert_eq!(back.mic.mode, MicMode::Always);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The default that carries the whole privacy argument for Tier 3: the
    /// local model is **off**, and nothing about a fresh install runs it.
    #[test]
    fn the_memory_graphs_local_model_is_off_by_default() {
        let cfg = Config::default();
        assert!(!cfg.graph.enabled);
        assert_eq!(cfg.graph.thread_gap_s, 20.0);
        // The user's stated budget, and the figure the bake-off measured on.
        assert_eq!(cfg.graph.llm_threads, 4);
        // The GPU belongs to whatever is drawing frames.
        assert_eq!(cfg.graph.gpu_layers, 0);
        assert_eq!(cfg.graph.llm_model, "qwen2.5-3b-instruct-q4_k_m.gguf");
        assert_eq!(cfg.graph.llama_dir, "llama");
        assert_eq!(cfg.graph.min_thread_segments, 3);
        assert_eq!(cfg.graph.max_queue_seconds, 5);
    }

    #[test]
    fn the_graph_section_parses_and_keeps_the_remaining_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            [graph]
            enabled = true
            llm_threads = 2
            "#,
        )
        .unwrap();
        assert!(cfg.graph.enabled);
        assert_eq!(cfg.graph.llm_threads, 2);
        assert_eq!(cfg.graph.gpu_layers, 0);
        assert_eq!(cfg.graph.thread_gap_s, 20.0);

        // …and it round-trips, because the GUI's switch writes it back.
        let dir = std::env::temp_dir().join(format!("nx-recall-graph-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        cfg.save(&path).unwrap();
        let back = Config::load(&path).unwrap();
        assert!(back.graph.enabled);
        assert_eq!(back.graph.llm_threads, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_keys_are_rejected_rather_than_silently_ignored() {
        // A typo'd key would otherwise leave capture silently misconfigured.
        let err = toml::from_str::<Config>("[vad]\nthreshhold = 0.7\n");
        assert!(err.is_err());
    }
}
