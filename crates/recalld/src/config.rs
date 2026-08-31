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
    /// Transducer export directory (the 0.6b variant drops in here later).
    pub asr: String,
    pub asr_encoder: String,
    pub asr_decoder: String,
    pub asr_joiner: String,
    pub asr_tokens: String,
    pub embedding: String,
    pub segmentation: String,
    /// ASR is the only stage where throughput is worth threads; Step 0 measured
    /// RTF 0.011 at four, which is still under 1% of a core.
    pub asr_threads: i32,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            dir: None,
            asr: "sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8".into(),
            asr_encoder: "encoder.int8.onnx".into(),
            asr_decoder: "decoder.int8.onnx".into(),
            asr_joiner: "joiner.int8.onnx".into(),
            asr_tokens: "tokens.txt".into(),
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

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub capture: CaptureConfig,
    pub vad: VadConfig,
    pub runtime: RuntimeConfig,
    pub models: ModelsConfig,
    pub identity: IdentityConfig,
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
        assert_eq!(cfg.vad.turn_merge_gap_ms, 1_500);
    }

    #[test]
    fn models_default_to_off_and_to_the_measured_default_asr() {
        let cfg = Config::default();
        assert!(cfg.models.dir.is_none());
        assert!(cfg.models.asr.contains("parakeet_tdt_transducer_110m"));
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
    fn unknown_keys_are_rejected_rather_than_silently_ignored() {
        // A typo'd key would otherwise leave capture silently misconfigured.
        let err = toml::from_str::<Config>("[vad]\nthreshhold = 0.7\n");
        assert!(err.is_err());
    }
}
