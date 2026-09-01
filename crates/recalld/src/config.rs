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
    pub vad: VadConfig,
    pub runtime: RuntimeConfig,
    pub models: ModelsConfig,
    pub identity: IdentityConfig,
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
    fn unknown_keys_are_rejected_rather_than_silently_ignored() {
        // A typo'd key would otherwise leave capture silently misconfigured.
        let err = toml::from_str::<Config>("[vad]\nthreshhold = 0.7\n");
        assert!(err.is_err());
    }
}
