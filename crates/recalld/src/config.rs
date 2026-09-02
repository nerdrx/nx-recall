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

// ---- 0.10.0, the room microphone ------------------------------------------

/// A second, physical microphone: the desk or room mic that hears the people
/// who are *in the room* and not in the instance.
///
/// It is deliberately not `[mic]` with a second device. The headset mic is
/// provenance — whatever it hears is the user, and its turns are pinned to the
/// "You" speaker with no comparison made. A room mic is the opposite: the
/// voices on it are strangers to the voicebank and have to be matched, minted
/// and enrolled like any voice coming out of an application. Two different
/// identity routes are two different switches.
///
/// `device` is **required**. There is no sensible default for a second input:
/// following `default.audio.source` would open the same headset the `[mic]` tap
/// is already on and record the user twice, under two identities.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoomConfig {
    pub enabled: bool,
    /// `follow` / `always`, exactly as `[mic]` means them — the two switches
    /// answer the same question ("when is a microphone open?") and answering it
    /// differently in two places would be a second thing to learn.
    pub mode: MicMode,
    /// PipeWire `node.name` of the room input. No default: see above.
    pub device: Option<String>,
}

impl Default for RoomConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: MicMode::Follow,
            device: None,
        }
    }
}

impl RoomConfig {
    /// The `node.name` the room tap must open, or `None` when none is pinned —
    /// which, unlike `[mic]`, means "cannot open" rather than "follow the
    /// default".
    pub fn device_override(&self) -> Option<&str> {
        self.device
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

// ---- end 0.10.0 -----------------------------------------------------------

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
    /// The optional text-embedding model for semantic search, as a directory
    /// under the models root plus the two files in it. Separate keys rather
    /// than one path because the directory name is what the stored
    /// `model_id` is derived from, exactly as it is for the ASR export.
    pub semantic: String,
    pub semantic_model: String,
    pub semantic_tokenizer: String,
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
            semantic: crate::models::SEMANTIC_DIR.into(),
            semantic_model: "model.onnx".into(),
            semantic_tokenizer: "tokenizer.json".into(),
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

    // ---- 0.11.0: the source-aware prior (`crate::identity_prior`) --------
    /// Whether where the audio came from is allowed to affect who wins a
    /// segment at all. **Off by default**, and that is a measurement rather
    /// than caution: on this install's 92 Discord turns with Discord's own
    /// ground truth, every candidate the ladder was choosing between was
    /// already native to Discord, so the prior changed nothing and there is no
    /// evidence yet that it helps (FINDINGS §17). It is a live, tested code
    /// path with a measured null result, not a guess — turn it on and
    /// `recalld identity audit` will say what it does.
    pub source_prior: bool,
    /// How much extra score a voice needs to win a segment on a source it has
    /// never been heard on. Added to `label_threshold`, so the default puts a
    /// foreign voice's bar at 0.45 against a native voice's 0.35.
    pub foreign_source_margin: f32,
    /// How many turns a voice must have, in total, before its *absence* from a
    /// source counts as evidence. Below this the voice is simply new, and
    /// calling it foreign would freeze the bank at whichever app happened to
    /// hear each person first.
    pub foreign_after_segments: i64,
    /// Whether Discord's speaking events may exclude a candidate outright on a
    /// Discord-sourced segment. On: this is Discord's own word about its own
    /// call, and the soft rules are for evidence that is weaker than that.
    /// Costs nothing when the truth bridge has never run — no data in the
    /// window means the rule does not fire.
    pub presence_hard: bool,
    /// Which capture sources count as VRChat, as lower-case substrings matched
    /// against a source's match key and display name. The mirror of
    /// `[truth].sources`, which says the same thing about Discord, and kept
    /// separate for the same reason: a client under another name should be a
    /// config edit rather than a rebuild.
    pub vrchat_sources: Vec<String>,
    // ---- 0.11.0: learned identity -----------------------------------------
    /// May the daemon fit its own operating point from ground truth?
    ///
    /// On by default, and that is safe rather than optimistic: the nightly
    /// pass never installs a value it has not first beaten the incumbent with
    /// on **held-out** truth rows, and precision has a veto
    /// (`calib::swap_is_safe`). On the corpus this shipped against the answer
    /// was "change nothing" — 239 usable rows over one evening and three
    /// linked voices, where the confusions that cost precision come from
    /// voices with almost no truth rows at all (FINDINGS §18). The switch
    /// exists so the pass can act the evening the evidence arrives, and so
    /// somebody who would rather it never did can say so.
    ///
    /// Off means the globals, exactly as 0.10.2 used them. Every learned
    /// value is inspectable with `recalld identity calibrate`, and
    /// `--reset` puts every voice back on the globals.
    pub learn: bool,
    // ---- end 0.11.0 -------------------------------------------------------
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
            // ---- 0.11.0 ----------------------------------------------
            source_prior: false,
            foreign_source_margin: 0.10,
            foreign_after_segments: 20,
            presence_hard: true,
            vrchat_sources: vec!["vrchat".into()],
            learn: true,
        }
    }
}

/// The conversational language prior (0.7.7).
///
/// The insight this implements, in the user's words: *if something doesn't get
/// recognised and everything was German before, the undecoded stuff is likely
/// to be German too.* A conversation has a language, that language is stable
/// across turns, and a turn that reads as the other one — in a thread where ten
/// turns running read as German — is far more likely to be a decoder flip than
/// a genuine switch.
///
/// The numbers here are two different kinds of thing and are kept apart on
/// purpose. `context_*` are **judgement**: how much agreement counts as a
/// conversation having a language. `arbiter_*` are **measurement**: the points
/// where `spike/arbiter_de.py` found re-decoding stops being an improvement,
/// and changing them without re-running that spike is changing what the daemon
/// claims to know.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LangConfig {
    /// How many of a thread's most recent *clear* language stamps the context
    /// is read from. Ten is roughly a minute of lobby back-and-forth: long
    /// enough that one odd turn cannot move it, short enough that a room which
    /// really does switch language is followed rather than argued with.
    ///
    /// `0` turns the context off entirely — no inheritance, no context-driven
    /// arbitration, and the 0.6.1 per-speaker correction behaves exactly as it
    /// always did.
    pub context_window: usize,
    /// Clear stamps needed before a thread has a language at all. Below this
    /// there is no context, and "unclear" stays unclear: two turns are a
    /// greeting, not a conversation with a language.
    pub context_min_clear: usize,
    /// Share of those stamps that must agree. 0.7 means three turns must be
    /// unanimous (2/3 is 0.67 and does not clear it) while ten turns may carry
    /// three dissenters — which is the right shape, because a bilingual room
    /// should not get a context at all.
    pub context_min_agree: f32,
    /// **Measured.** Below this many seconds a re-decode may only *flag*, never
    /// replace: `spike/arbiter_de.py` put the arbiter's word precision at 28%
    /// on 1.0 s fragments against 54% at 1.5 s. Replacing a wrong transcript
    /// with a differently wrong one is not a correction.
    pub arbiter_min_duration_s: f32,
    /// **Measured.** A one-word arbiter output is not evidence of anything, and
    /// the empty-output rate is only ~0% above `arbiter_min_duration_s`.
    pub arbiter_min_words: usize,
}

impl Default for LangConfig {
    fn default() -> Self {
        Self {
            context_window: 10,
            context_min_clear: 3,
            context_min_agree: 0.7,
            arbiter_min_duration_s: 1.5,
            arbiter_min_words: 2,
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

    // ---- Tier 3 (GRAPH.md: "opt-in, and running whenever it is on") ------
    /// Run the local model over conversations nobody has looked at yet.
    ///
    /// **Off, and off is the default.** GRAPH.md: "Off by default. Its switch
    /// sits next to the mic's, with equally plain copy." Turning it on costs a
    /// 1.9 GB model on disk and `llm_threads` pinned cores at nice 19 — and
    /// once it is on it runs, including while a game is being captured (0.7.2,
    /// see `crate::enrich`). Never in the capture path.
    pub enabled: bool,
    /// Threads handed to llama.cpp (`-t`), and the one number in this table a
    /// person is expected to turn: it is how much of the machine the model may
    /// use while they are playing. Four is the user's stated budget and the
    /// number the bake-off measured on (3.3 s/case for Qwen2.5-3B Q4); the
    /// Memory tab sets it live over `graph.set`, clamped to
    /// [`crate::control::GRAPH_THREADS`], and it applies to the next model call
    /// because threads are an argument to an invocation.
    pub llm_threads: i32,
    /// Layers offloaded to the GPU (`-ngl`). Zero, and meant to stay zero: the
    /// GPU belongs to whatever is drawing frames. It exists for a machine that
    /// is not gaming at all.
    pub gpu_layers: i32,
    /// Ceiling on one model call, in seconds, after which the child is killed.
    /// Generous next to a 3.3 s median because a cold page-in of 1.9 GB is not
    /// a hang — but finite, because a wedged child holding its cores is.
    pub llm_timeout_s: u64,
    /// Conversations per enrichment batch. The worker stops between batches to
    /// re-check every gate, so this is really "how long the worker commits to
    /// before looking up again".
    pub batch_threads: usize,
    /// Seconds to wait between batches, and between re-checks when a gate is
    /// closed. Background work has no deadline; being invisible matters more.
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

/// The accuracy round (0.8.0): what the idle quality worker is allowed to do to
/// a transcript after the fact.
///
/// Nothing here touches the live path. Both passes run in one background thread
/// behind the same gates the enrichment worker uses, and both are measured
/// rather than assumed — the two `spike/` benches named on the fields are what
/// set the defaults.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AsrConfig {
    /// Re-decode short turns with the audio around them and keep the words that
    /// fall inside the turn.
    ///
    /// **Measured** (`spike/context_redecode_bench.py`, FLEURS German +
    /// LibriSpeech through Opus 24k, 125 turns): 56.7% → 20.4% WER at 1.5 s
    /// (64% relative) and 34.3% → 17.1% at 2.5 s (50% relative). It is on by
    /// default because that is the largest accuracy gain in the whole round and
    /// it costs nothing a person can feel.
    pub context_redecode: bool,
    /// Turns shorter than this are re-decoded with their neighbours; longer
    /// ones already carry their own context and are left alone. 2.5 s is where
    /// the measured gain is still 50% relative and the median real turn (2.7 s)
    /// sits just above it.
    pub context_redecode_below_s: f32,
    /// How much audio either side of the turn goes into the window.
    pub context_pad_s: f32,
    /// A silence longer than this between two stored clips truncates the
    /// window. There is no continuous recording — the daemon stores one WAV per
    /// turn — so a window is built by butting clips together with their real
    /// silences in between, and past a second of silence what is on the other
    /// side is a different piece of speech, not context.
    pub context_max_gap_s: f32,
    /// Cross-check transcripts against a second decoder and flag the ones it
    /// disagrees with (`crate::canary`). Needs `models fetch --confidence`;
    /// without it every `asr_confidence` stays null.
    pub confidence: bool,
    /// Agreement at or above which the two decoders count as agreeing.
    ///
    /// **Measured** (`spike/confidence_bench.py`, 257 items): at 0.5 the
    /// disagreeing turns carry 76.4% word error against 18.2% for the agreeing
    /// ones — a 4.2× split. The ratio is flat from 0.4 to 0.8 when the source
    /// language is known; 0.5 is the highest value that still holds ≥3× when it
    /// is not and both languages have to be tried.
    pub confidence_tau: f32,
    /// Segments per batch, for both passes. The worker re-checks every gate
    /// between batches, so this is how long it commits to before looking up.
    pub batch_segments: usize,
    /// Seconds between batches, and between re-checks while a gate is closed.
    pub batch_pause_s: u64,
    /// Queue depth, in seconds of audio waiting for the inference thread, above
    /// which the worker stands down — the same rule, and the same reason, as
    /// `[graph].max_queue_seconds`.
    pub max_queue_seconds: i64,
    /// Ceiling on the vocabulary's `effective` list.
    pub vocab_max_terms: usize,
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            context_redecode: true,
            context_redecode_below_s: 2.5,
            context_pad_s: 3.0,
            context_max_gap_s: 1.0,
            confidence: true,
            confidence_tau: 0.5,
            batch_segments: 40,
            batch_pause_s: 10,
            max_queue_seconds: 5,
            vocab_max_terms: 500,
        }
    }
}

// ---- the night shift (0.9.0) ---------------------------------------------

/// A second reading of the day's shaky rows, on the GPU, while nobody is using
/// the machine (`crate::night`).
///
/// **Off, and off is the default**, for two independent reasons. The model is a
/// gigabyte that nothing else needs and the runtime has to be *compiled* on
/// this machine (`recalld models build-night`); and the GPU it wants is the one
/// drawing the user's frames. Turning it on is a decision about a machine, not
/// a preference, which is why nothing here guesses.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NightConfig {
    /// Run the night shift at all.
    pub enabled: bool,
    /// `HH:MM-HH:MM`, local time, wrapping over midnight. The hours the machine
    /// is assumed to be nobody's — outside them the worker does not run even if
    /// everything else is idle, because "the GPU looks free" and "the person is
    /// not using their computer" are different claims and only the clock can
    /// make the second one.
    pub window: String,
    /// Minutes of no capture activity after which the worker may also run
    /// outside the window. Zero switches the idle path off entirely and leaves
    /// only the clock.
    pub also_when_idle_min: i64,
    /// GPU utilisation, in percent, above which a batch does not start. Checked
    /// before **every** batch, not once at the top of the night: a person who
    /// sits down at 04:00 and puts a headset on gets the GPU back within one
    /// batch. 50, not 20: amdgpu's busy counter reads 25–30 on a bare desktop
    /// (compositing counts) and 90–100 in a VR session, so 50 separates the
    /// two cases and 20 separated nothing (FINDINGS §13 addendum).
    pub gpu_busy_max_pct: u32,
    /// The most rows one night will re-read. A ceiling on a background job that
    /// would otherwise walk the entire history the first time it is switched
    /// on.
    pub max_rows_per_night: usize,
    /// Clips concatenated into one decode. The model load dominates a
    /// two-second clip by two orders of magnitude, so the batch is the whole
    /// reason this is affordable; the clips are butted together with
    /// `gap_s` of silence and split apart again by offset.
    pub batch_rows: usize,
    /// Silence between two clips in a batch. One second: long enough that the
    /// decoder does not run two turns into one sentence, short enough not to
    /// invite a caption hallucination about the quiet.
    pub gap_s: f32,
    /// Ceiling on one decode, in seconds, after which the child is killed. A
    /// batch of eight two-second clips takes a few seconds on the GPU; this is
    /// sized for a cold model load on a spinning disk, and is finite because a
    /// wedged child holding a GPU is worse than a lost batch.
    pub timeout_s: u64,
    /// The whisper.cpp binaries, relative to `[models].dir`. `whisper-cli` and
    /// the shared objects it loads both live here, and `models build-night`
    /// puts them there.
    pub whisper_dir: String,
    /// The GGML model, relative to `[models].dir`.
    pub model: String,
    /// Whether the vote may **replace** words, or only annotate them.
    ///
    /// **True, because it was measured** (`spike/night_vote_bench.py`,
    /// FINDINGS §13): on 35 shaky lab spans with human references, decoded by
    /// the build this daemon ships, the shipped rule — two of three readings
    /// agreeing against the row, then the arbiter's guards — cuts word error
    /// from 91.1% to 44.3% (51.4% relative) and makes **none** of the 20 rows
    /// it touches worse. The gate was ≥30% relative with under 5% harmed, and
    /// that is the whole argument for this default being what it is. Zero out
    /// of twenty is "no sign of harm", not "harmless".
    ///
    /// It stays a switch because the other outcome is a real product rather
    /// than a disabled feature: with it off the night reading is stored as
    /// `night_text` and shown beside the row ("the night shift read: …"), and
    /// the transcript is never touched. A person who does not want a machine
    /// editing their evening's words gets the second opinion anyway.
    pub replace: bool,
}

impl Default for NightConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            window: "03:00-07:00".into(),
            also_when_idle_min: 20,
            gpu_busy_max_pct: 50,
            max_rows_per_night: 400,
            batch_rows: 8,
            gap_s: 1.0,
            timeout_s: 600,
            whisper_dir: "whisper".into(),
            model: "ggml-large-v3-q5_0.bin".into(),
            replace: true,
        }
    }
}

// ---- 0.9.0, the assistant -------------------------------------------------

/// The three things the daemon does *for* you rather than *to* the recording
/// (0.9.0): reminders that fire, one paragraph per conversation, and a
/// translation on a turn you cannot read.
///
/// Everything here is off-by-default in the way that matters. Reminders need a
/// note with a time in it, which only exists because somebody said one out
/// loud. The digest and the translation both need `[graph]` — they are the same
/// 1.9 GB local model, under the same jail — so on a machine that has never run
/// `models fetch --graph` this whole section is inert, and the two switches
/// below decide only whether an enabled model spends its budget here.
///
/// `translate_to` is empty by default and is the one setting with no sensible
/// guess: which language you read is not something a daemon can measure, and
/// translating into the wrong one would be worse than translating into none.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AssistConfig {
    /// Announce notes whose time reference has come round.
    pub reminders: bool,
    /// How often the scheduler looks. Thirty seconds is the resolution of the
    /// feature, and it is deliberately coarse: a reminder is a note somebody
    /// dictated in a headset, not an alarm clock.
    pub reminder_tick_s: u64,
    /// Reminders one tick may announce. A daemon that was off overnight has a
    /// backlog, and a tick that can publish two hundred events is a tick that
    /// can hang up every client's outbox.
    pub reminder_batch: usize,

    /// Summarise conversations once they have settled.
    pub digest: bool,
    /// How long after a conversation's last turn it becomes eligible. A thread
    /// is only over in retrospect — the threading rule keeps extending it while
    /// anybody speaks — so this is how long the daemon waits to be sure.
    pub digest_settle_min: i64,
    /// Turns with words in them a conversation needs before it is worth a
    /// paragraph. Eight is the bench's floor and it is the number the trap
    /// cases are built at: eight turns of "ja" is a conversation by the
    /// threading rule and nothing at all by content.
    pub digest_min_turns: i64,
    /// Turns of a conversation the model is shown. Past this a summary is a
    /// summary of a summary, and the budget is a model call either way.
    pub digest_max_turns: usize,

    /// Translate committed turns into this language tag (`"de"`, `"en"`).
    /// Empty is off, and is the shipped value.
    pub translate_to: String,
    /// Turns shorter than this are not translated. Three words, because "ja
    /// klar" translated is "yeah sure" and nobody needed it — and because a
    /// two-word turn is where the decoder is least reliable to begin with.
    pub translate_min_words: usize,

    // ---- 0.10.2, the translation controls ---------------------------------
    /// The languages the person reading already has. A turn in one of these is
    /// never translated, and `translate_to` is always implicitly one of them —
    /// it is not stored here, because a target you did not also list as read
    /// would be a setting that contradicts itself.
    ///
    /// `["de", "en"]` by default, which is this daemon's two native languages
    /// and the pair the classifier can actually check. Everything else is a
    /// candidate; see `crate::lang::guess_other` for how a third language is
    /// recognised at all.
    pub read_languages: Vec<String>,
    /// Where the translation goes on a transcript row: `"main"` puts it where
    /// the words normally are with the original beneath it as subtext, and
    /// `"under"` is 0.9.0's layout — the original on the line and the
    /// translation under it.
    ///
    /// `"main"` is the default and that is a deliberate change of mind. 0.9.0
    /// argued the original must lead because the transcript is a record; a
    /// reader who cannot read the original is not reading a record, they are
    /// looking at a wall of text with a hint under each line. The original
    /// never leaves the row either way, and it keeps its language code — which
    /// is what stops "main" from being a quotation nobody can check.
    pub translation_display: String,
    // ---- end 0.10.2 -------------------------------------------------------
    /// Rows per batch, for both model passes.
    pub batch: usize,
    /// Seconds between batches, and between re-checks while a gate is closed.
    pub batch_pause_s: u64,
    /// Queue depth, in seconds of audio waiting for the inference thread, above
    /// which the worker stands down — the same rule, and the same reason, as
    /// `[graph].max_queue_seconds`.
    pub max_queue_seconds: i64,
}

impl Default for AssistConfig {
    fn default() -> Self {
        Self {
            reminders: true,
            reminder_tick_s: 30,
            reminder_batch: 20,
            digest: true,
            digest_settle_min: 30,
            digest_min_turns: 8,
            digest_max_turns: 40,
            translate_to: String::new(),
            translate_min_words: 3,
            read_languages: vec!["de".to_string(), "en".to_string()],
            translation_display: crate::translate::DISPLAY_MAIN.to_string(),
            batch: 8,
            batch_pause_s: 10,
            max_queue_seconds: 5,
        }
    }
}

// ---- end 0.9.0 ------------------------------------------------------------

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

// ---- 0.9.0 (ground truth from Discord) ------------------------------------

/// Ground truth from Discord (0.9.0): a loopback ingest the Vencord plugin
/// posts who-spoke-when to, and the idle pass that scores the voicebank
/// against it.
///
/// **Off by default, and the one TCP listener in the program.** DESIGN §8 says
/// no TCP, ever, and this is the exception with its reasons written down:
/// Discord's renderer can only reach a local service over HTTP, the listener
/// binds `127.0.0.1` and refuses anything else, and the access control that
/// the control socket gets from being a 0600 unix socket this one gets from a
/// bearer token in a 0600 file. It stays off until somebody turns it on.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TruthConfig {
    /// Whether the loopback ingest listens at all. Off until asked.
    pub enabled: bool,
    /// Loopback port. Must match the plugin's.
    pub port: u16,
    /// Run the labelling pass over Discord segments. Separate from `enabled`
    /// on purpose: truth that has already been collected is still worth
    /// labelling against after the plugin has been turned off.
    pub label: bool,
    /// Enrol clean, well-covered, single-speaker turns into the linked
    /// speaker's voicebank. Off by default — an automatic write to the
    /// voicebank is the one thing here that changes future behaviour rather
    /// than merely measuring it.
    pub enrol: bool,
    /// A speaking row with no stop is closed this long after it started. The
    /// plugin sends a stop for every start, so this only fires when Discord
    /// was killed, the plugin was disabled mid-word, or a batch was dropped.
    pub open_span_timeout_s: u64,
    /// Segments per labelling batch, and how long the worker waits between
    /// batches. The same two knobs `[asr]` has, for the same reason.
    pub batch_segments: usize,
    pub batch_pause_s: u64,
    /// How much audio may be waiting before the worker stands down. Capture
    /// comes first (`crate::quality::gate`).
    pub max_queue_seconds: i64,
    /// Which capture sources count as Discord, as lower-case substrings
    /// matched against a source's match key and display name. VRChat is not
    /// Discord and never matches any of these; the list is here so a fork of
    /// the client under another name can be told about without a rebuild.
    pub sources: Vec<String>,
}

impl Default for TruthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: 7797,
            label: true,
            enrol: false,
            open_span_timeout_s: 30,
            batch_segments: 200,
            batch_pause_s: 20,
            max_queue_seconds: 5,
            sources: vec!["discord".into(), "vesktop".into()],
        }
    }
}

// ---- end 0.9.0 ------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub capture: CaptureConfig,
    pub mic: MicConfig,
    /// The second, physical microphone (0.10.0). Off by default, and unlike
    /// `[mic]` it cannot open at all until `device` names one.
    pub room: RoomConfig,
    pub vad: VadConfig,
    pub runtime: RuntimeConfig,
    pub models: ModelsConfig,
    pub identity: IdentityConfig,
    pub lang: LangConfig,
    /// The accuracy round's idle worker (0.8.0).
    pub asr: AsrConfig,
    /// The night shift (0.9.0).
    pub night: NightConfig,
    pub graph: GraphConfig,
    /// The assistant round (0.9.0): reminders, digests, translation.
    pub assist: AssistConfig,
    pub socket: SocketConfig,
    pub roster: RosterConfig,
    pub retention: RetentionConfig,
    /// Ground truth from Discord (0.9.0). Off by default.
    pub truth: TruthConfig,
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
             #\n\
             # `[room]` is a SECOND, physical microphone — a desk mic that hears\n\
             # the people sitting with you, who never joined the instance. It is\n\
             # off by default and it needs `device = \"<pipewire node.name>\"`;\n\
             # `recalld devices` lists them. Its voices are matched and enrolled\n\
             # like anybody else's, so they appear in the voicebank by name.\n\
             #\n\
             # `[identity].source_prior` makes WHERE a turn came from evidence\n\
             # about who is on it: a voice heard thousands of times on Discord\n\
             # and never in VRChat needs a higher score to win a VRChat turn,\n\
             # and Discord's own speaking events can rule a linked account out\n\
             # of a Discord turn entirely. It is OFF because it was measured and\n\
             # found to change nothing yet on this corpus — 236 candidates\n\
             # removed across 161 ground-truth turns, 0 labels changed, because\n\
             # every voice in a Discord call already lives on Discord (spike/\n\
             # FINDINGS.md §17). `recalld identity audit` says what it would do\n\
             # to your database without turning it on.\n\
             #\n\
             # `[identity].learn` lets the nightly pass fit its own operating\n\
             # point — a label threshold per voice, and a learned embedding\n\
             # space — from the turns Discord itself labelled. It is ON, and it\n\
             # is still conservative: nothing is installed that has not beaten\n\
             # what is already there on the last 40%% of truth rows by time,\n\
             # and a candidate that lowers held-out precision is refused\n\
             # whatever it does to recall. `recalld identity calibrate` prints\n\
             # the whole table without writing; `--apply` writes what cleared\n\
             # the gate; `--reset` puts every voice back on the globals.\n\
             \n{body}"
        );
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("installing {}", path.display()))?;
        Ok(())
    }
}

/// `$XDG_CONFIG_HOME/nx-recall`, else `~/.config/nx-recall`.
pub fn default_config_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("no config directory (XDG_CONFIG_HOME / HOME unset)")?;
    Ok(base.join("nx-recall"))
}

/// `$XDG_CONFIG_HOME/nx-recall/config.toml`, else `~/.config/nx-recall/config.toml`.
pub fn default_config_path() -> Result<PathBuf> {
    Ok(default_config_dir()?.join("config.toml"))
}

/// Where the truth ingest's bearer token lives (0.9.0): beside the config
/// file, or wherever `NXR_TRUTH_TOKEN` points. The env override exists for the
/// same reason `NXR_SOCKET` does — it is what keeps a test off the live
/// daemon's token.
pub fn truth_token_path(config_path: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("NXR_TRUTH_TOKEN").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    match config_path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.join("truth.token"),
        _ => PathBuf::from("truth.token"),
    }
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

        // 0.11.0. The prior ships OFF, and that is a measurement (FINDINGS
        // §17): on 161 turns with Discord's own ground truth it removed 236
        // candidates and changed nothing, because every voice in a Discord call
        // already lives on Discord. Flipping this default is a claim about
        // accuracy and needs a corpus that crosses sources to back it.
        assert!(!cfg.identity.source_prior);
        assert_eq!(cfg.identity.foreign_source_margin, 0.10);
        assert_eq!(cfg.identity.foreign_after_segments, 20);
        // Learning is on, and the gate is what makes that safe.
        assert!(cfg.identity.learn);
        // The hard rule is on, and costs nothing while the prior is off — and
        // nothing again on an install whose truth bridge has never run.
        assert!(cfg.identity.presence_hard);
        assert_eq!(cfg.identity.vrchat_sources, vec!["vrchat".to_string()]);
        // Its mirror, which says the same about Discord and is not duplicated.
        assert_eq!(cfg.truth.sources, vec!["discord", "vesktop"]);
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

    // ---- 0.10.0, the room microphone -------------------------------------

    #[test]
    fn the_room_microphone_is_off_and_has_no_device_until_it_is_given_one() {
        let cfg = Config::default();
        assert!(!cfg.room.enabled);
        assert_eq!(cfg.room.mode, MicMode::Follow);
        // The difference from `[mic]` that matters: no default device, and no
        // fallback to the system default either. See room.rs.
        assert_eq!(cfg.room.device_override(), None);
    }

    #[test]
    fn the_room_switch_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!("nx-recall-room-cfg-{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);

        let mut cfg = Config::default();
        cfg.room.enabled = true;
        cfg.room.mode = MicMode::Always;
        cfg.room.device = Some("alsa_input.usb-Desk_Mic".to_string());
        cfg.save(&path).unwrap();

        let back = Config::load(&path).unwrap();
        assert!(back.room.enabled);
        assert_eq!(back.room.mode, MicMode::Always);
        assert_eq!(
            back.room.device_override(),
            Some("alsa_input.usb-Desk_Mic"),
            "the device pin is the one field that cannot be re-derived"
        );
        // And the headset's own switch is untouched by any of it: two devices,
        // two decisions.
        assert!(!back.mic.enabled);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_room_section_parses_like_the_mic_section_and_refuses_nonsense() {
        let cfg: Config =
            toml::from_str("[room]\nenabled = true\nmode = \"follow\"\ndevice = \"  desk  \"\n")
                .unwrap();
        assert!(cfg.room.enabled);
        assert_eq!(cfg.room.device_override(), Some("desk"));
        assert!(toml::from_str::<Config>("[room]\nmode = \"sometimes\"\n").is_err());
        // A blank device is "none pinned", not a node named "".
        let blank: Config = toml::from_str("[room]\ndevice = \"\"\n").unwrap();
        assert_eq!(blank.room.device_override(), None);
    }

    // ---- end 0.10.0 -------------------------------------------------------

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

    /// The night shift's defaults carry two separate facts, and both are
    /// assertions rather than comments: it does not run at all until somebody
    /// turns it on, and the rule it would run is the one the bench measured
    /// (FINDINGS §13) rather than a cautious guess.
    #[test]
    fn the_night_shift_is_off_but_its_measured_rule_is_armed() {
        let cfg = Config::default();
        assert!(!cfg.night.enabled);
        assert!(
            cfg.night.replace,
            "the measured rule (FINDINGS §13) is allowed to replace; the gate was cleared"
        );
        assert_eq!(cfg.night.window, "03:00-07:00");
        assert_eq!(cfg.night.also_when_idle_min, 20);
        assert_eq!(cfg.night.gpu_busy_max_pct, 50);
        assert_eq!(cfg.night.model, "ggml-large-v3-q5_0.bin");
        assert_eq!(cfg.night.whisper_dir, "whisper");
        assert_eq!(cfg.night.gap_s, 1.0);
    }

    #[test]
    fn the_night_section_parses_and_keeps_the_remaining_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            [night]
            enabled = true
            window = "02:30-06:15"
            "#,
        )
        .expect("parses");
        assert!(cfg.night.enabled);
        assert_eq!(cfg.night.window, "02:30-06:15");
        assert!(cfg.night.replace, "an unrelated key must not move this one");
        assert_eq!(cfg.night.gpu_busy_max_pct, 50);
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
