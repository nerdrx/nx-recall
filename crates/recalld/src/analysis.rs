//! The analysis leg: overlap gate, ASR, speaker identity.
//!
//! Order matters and is measured, not arbitrary:
//!
//! 1. **Overlap** first, because its answer decides whether the identity branch
//!    is allowed to run at all.
//! 2. **ASR on every turn regardless of overlap** — a transcript of the mix is
//!    still what the user wants to search, and Parakeet's insertion rate stays
//!    at or under 1.5% in the realistic regime.
//! 3. **Identity only on turns the gate approved**, so a blended embedding is
//!    never even computed, let alone stored.
//!
//! The microphone leg (`commit_mic`) short-circuits step 3's *question* without
//! skipping its *guard*: the speaker is known from where the audio came, so the
//! voicebank is never consulted, but the overlap detector still runs and still
//! decides whether anything may be enrolled.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use tracing::{debug, info, warn};

use crate::arbiter::{Arbiters, Arbitration};
use crate::asr::normalise_words;
use crate::config::{IdentityConfig, LangConfig, SAMPLE_RATE, TruthConfig};
use crate::embed::{Embedder, Embedding};
use crate::identity::{self, Decision, Refusal};
use crate::lang;
use crate::langctx::{self, ContextFix, Intent};
use crate::models::ModelSet;
use crate::overlap::OverlapDetector;
use crate::store::{SegmentAnalysis, Store, lang_via};

/// What inference learned about a turn, before anything is written down.
///
/// Splitting this out keeps the model work off the store mutex: the daemon
/// runs `prepare` unlocked and only takes the lock for `commit`, so a 30 s turn
/// cannot stall PipeWire's graph callbacks for the length of an ASR pass.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub overlap_frac: f32,
    pub duration_s: f32,
    pub text: Option<String>,
    pub asr_model_id: String,
    /// The transcript's language, when the model is one that only speaks one.
    /// `None` from the multilingual export — see `Asr::lang`. This is the
    /// model's constraint, not a reading of the words; the text classifier
    /// fills the gap when it is absent (`language_of`).
    pub model_lang: Option<&'static str>,
    /// `None` exactly when `refusal` is `Some`: audio the gate rejects is never
    /// embedded, so no blended vector can reach the voicebank.
    pub embedding: Option<Embedding>,
    pub refusal: Option<Refusal>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub overlap_frac: f32,
    pub text: Option<String>,
    pub decision: Decision,
    pub speaker_id: Option<i64>,
    pub match_score: Option<f32>,
    pub enrolled: bool,
    /// A golden sample was written for this turn (mic enrolment only).
    pub golden: bool,
    /// What the wrong-language correction did, if anything (0.6.1).
    pub language_fix: Option<LanguageFix>,
    /// What the source-aware prior took off the candidate list before the
    /// ladder saw it (0.11.0). `None` on every path that never consulted the
    /// voicebank — the microphone's pin, a refused turn — and `Some` with an
    /// empty `dropped` when the prior looked and changed nothing.
    pub prior: Option<crate::identity_prior::Applied>,
    /// **Other** segments this turn's arrival changed — proximity inheritance
    /// labels the turn *before* this one, once this one proves what came after
    /// it. The caller publishes them, so a GUI sees the row change without
    /// re-querying.
    pub also_changed: Vec<i64>,
    /// The spoken-language identifier was run on this turn (0.11.0). The
    /// number that says what the Japanese feature *costs*: it is the count of
    /// turns whose transcript nobody could read, which is a fact worth being
    /// able to watch whether or not any of them turned out to be Japanese.
    pub lid_checked: bool,
    /// The turn was re-decoded by a CJK decoder and the row now says so
    /// (0.11.0 for `ja`, 0.11.6 for `ko`/`zh`). `None` on every turn that was
    /// none of the three, which is nearly all of them. The tag it carries is
    /// what the decoder's *script* turned out to be, not what the identifier
    /// said — see `crate::asr_cjk::judge`.
    pub routed_cjk: Option<crate::asr_cjk::Routed>,
    /// The turn was re-decoded by a decoder forced to the language the
    /// identifier named — French, Spanish, Italian, Portuguese, Dutch or
    /// Polish (0.11.8, `crate::polyglot`). Its own field beside `routed_cjk`
    /// rather than folded into it because the two carry different claims: that
    /// one means "the decoder could not spell this language", this one means
    /// "the decoder heard the wrong one".
    pub routed_other: Option<crate::polyglot::Routed>,
}

/// Everything the microphone leg needs that the matching leg does not: who the
/// audio belongs to by construction, and where the kept clips go.
#[derive(Debug, Clone, Copy)]
pub struct MicEnroll<'a> {
    /// The pinned "You" speaker (`Store::ensure_you_speaker`).
    pub speaker_id: i64,
    pub data_dir: &'a Path,
    pub max_goldens: usize,
}

/// The general case the microphone turned out to be one of (0.12.1).
///
/// "This audio is one known person, by construction" is a claim two sources
/// can make. The headset makes it because one person wears one headset; a
/// per-user Discord stream makes it because the packets were decoded from one
/// person's connection. What differs is only *which* voice, what provenance to
/// stamp, and whether keeping clips of it forever is defensible — so those are
/// the three fields, and everything else is shared.
#[derive(Debug, Clone, Copy)]
pub struct PinnedLeg<'a> {
    pub speaker_id: i64,
    /// What goes in `segments.label_via`: `mic`, or `discord-stream`.
    pub label_via: &'static str,
    /// Where retention-exempt clips are kept, and how many. `None` keeps none
    /// — which is the answer for anybody who is not the user.
    pub goldens: Option<(&'a Path, usize)>,
    /// Whether a prototype may be added at all, before the audio-quality bar is
    /// even consulted. The mic's is unconditional; Discord's follows
    /// `[truth].enrol`, because writing to the voicebank on the strength of
    /// something that arrived over a socket is the one thing here that changes
    /// future behaviour rather than merely recording the present.
    pub enrol: bool,
}

impl<'a> PinnedLeg<'a> {
    /// The headset's, so `commit_mic` stays exactly what it was.
    fn mic(mic: &MicEnroll<'a>) -> Self {
        Self {
            speaker_id: mic.speaker_id,
            label_via: crate::store::label_via::MIC,
            goldens: Some((mic.data_dir, mic.max_goldens)),
            enrol: true,
        }
    }
}

/// How the overlap gate applies to one turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    /// `identity::gate`: too overlapped, or too short, and no embedding is
    /// taken. What every source has always used.
    Full,
    /// Duration only (0.12.1). For a source whose audio is single-speaker **by
    /// construction** — one Discord user's own stream — where a positive
    /// overlap reading is the detector being wrong about reverb rather than a
    /// second person, and refusing the embedding would throw away the cleanest
    /// enrolment material this daemon will ever see.
    ///
    /// It does not lower the duration floor. A 0.4 s grunt makes an embedding
    /// that is noise whoever it belongs to, and the floor is about the vector
    /// being worth storing rather than about the name being in doubt.
    SingleSpeaker,
}

/// `goldens/<speaker>/golden-<segment>.wav`, relative to the data dir.
///
/// Deliberately **not** under `segments/`: the retention sweeper walks that
/// tree for orphans and unlinks what the `segments` table has aged out, and a
/// golden must outlive both (DESIGN §5/§6 — a golden is what a future embedding
/// model gets re-enrolled from). Living in its own directory is what makes it
/// retention-exempt, with no special case in the sweeper at all.
pub fn golden_path(speaker_id: i64, segment_id: i64) -> PathBuf {
    PathBuf::from("goldens")
        .join(format!("{speaker_id:06}"))
        .join(format!("golden-{segment_id:06}.wav"))
}

/// What the language correction did to one row, when it did anything.
#[derive(Debug, Clone, PartialEq)]
pub enum LanguageFix {
    /// The audio was decoded again under a hard language constraint and the
    /// new transcript won. `text` is what the row says now, and `lang` is the
    /// constraint it was produced under — which is also the direction the
    /// counters split on.
    Redecoded {
        text: String,
        asr_model_id: String,
        lang: String,
    },
    /// The transcript and the speaker's declared language disagree and nothing
    /// in the catalogue can settle it. The words are kept and the row is
    /// marked; `lang` goes to NULL rather than to a guess.
    Marked { read_as: &'static str },
}

pub struct Analyzer {
    performance: std::sync::Arc<crate::performance::Performance>,
    overlap: OverlapDetector,
    /// One transducer, behind whichever binding `[identity].split_turns` asked
    /// for at load (0.12.4). See [`crate::asr::Decoder`].
    asr: crate::asr::Decoder,
    embedder: Embedder,
    cfg: IdentityConfig,
    /// The conversational language prior's thresholds and the arbiter's
    /// measured guards (0.7.7). Defaulted rather than a `load` parameter so a
    /// caller that does not care about language — the acceptance rig, the mic
    /// suite — keeps the two-argument constructor it always had.
    lang_cfg: LangConfig,
    /// The two constrained decoders, each loaded the first time a suspected
    /// flip needs it and resident from then on (`crate::arbiter`).
    arbiters: Arbiters,
    /// Which capture sources count as Discord (0.11.0). Read from
    /// `[truth].sources` rather than copied into `[identity]`, and defaulted
    /// like `lang_cfg` so a caller that does not care — the acceptance rig, the
    /// mic suite — keeps the two-argument constructor it always had.
    truth_cfg: TruthConfig,

    // ---- Japanese, Korean, Chinese (0.11.0/0.11.6, `crate::asr_cjk`) -----
    /// The CJK decoders and the spoken-language identifier that routes to
    /// them, each loaded the first time it is needed and resident from then on.
    cjk: crate::asr_cjk::Cjk,
    /// The switch and the operating point the router reads. Defaulted like
    /// `lang_cfg`, and set from the running config by [`Analyzer::
    /// set_asr_config`].
    asr_cfg: crate::config::AsrConfig,
    // ---- the other languages (0.11.8, `crate::polyglot`) -------------------
    /// The forced decoders for every other language the identifier can name,
    /// each loaded the first time a turn needs it. Holds no identifier of its
    /// own: it reads the one `japanese` above already ran.
    polyglot: crate::polyglot::Polyglot,
}

impl Analyzer {
    pub fn load(models: &ModelSet, cfg: &IdentityConfig) -> Result<Self> {
        let missing = models.missing();
        if !missing.is_empty() {
            let list = missing
                .iter()
                .map(|e| format!("{} ({})", e.role, e.path.display()))
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!("analysis models are incomplete: {list}");
        }
        Ok(Self {
            performance: std::sync::Arc::default(),
            overlap: OverlapDetector::load(&models.segmentation)?,
            asr: crate::asr::Decoder::load(models, cfg.split_turns)?,
            embedder: Embedder::load(models)?,
            cfg: cfg.clone(),
            lang_cfg: LangConfig::default(),
            arbiters: Arbiters::new(models),
            truth_cfg: TruthConfig::default(),
            cjk: crate::asr_cjk::Cjk::new(models, &crate::config::AsrConfig::default()),
            asr_cfg: crate::config::AsrConfig::default(),
            polyglot: crate::polyglot::Polyglot::new(
                models,
                &crate::config::AsrConfig::default(),
                &crate::config::NightConfig::default(),
                &crate::config::RuntimeConfig::default(),
            ),
        })
    }

    /// Point the language prior at the running config. Set once, before the
    /// inference thread starts.
    pub fn set_lang_config(&mut self, cfg: &LangConfig) {
        self.lang_cfg = cfg.clone();
    }

    /// Point the source-aware prior at the running config's Discord source
    /// patterns (0.11.0). Set once, in the same place and for the same reason.
    pub fn set_truth_config(&mut self, cfg: &TruthConfig) {
        self.truth_cfg = cfg.clone();
    }

    /// The pattern lists the prior's family test reads.
    fn prior_sources(&self) -> crate::identity_prior::Sources<'_> {
        crate::identity_prior::Sources {
            discord: &self.truth_cfg.sources,
            vrchat: &self.cfg.vrchat_sources,
        }
    }

    // ---- Japanese (0.11.0) -----------------------------------------------
    /// Point both audio-language routers at the running config. Set once, in
    /// the same place and for the same reason as [`Analyzer::set_lang_config`]
    /// — **before** the inference thread starts, because it rebuilds them and
    /// they own lazily loaded models.
    ///
    /// `night` and `runtime` are here for the polyglot route (0.11.8), whose
    /// only measured decoder is the night shift's — see [`crate::polyglot`] for
    /// why the cheap local one was rejected.
    pub fn set_asr_config(
        &mut self,
        models: &ModelSet,
        cfg: &crate::config::AsrConfig,
        night: &crate::config::NightConfig,
        runtime: &crate::config::RuntimeConfig,
    ) {
        self.cjk = crate::asr_cjk::Cjk::new(models, cfg);
        self.polyglot = crate::polyglot::Polyglot::new(models, cfg, night, runtime);
        self.asr_cfg = cfg.clone();
    }

    /// The line the daemon logs once at start-up about the other languages
    /// (0.11.6), `None` when there is nothing worth saying.
    pub fn polyglot_note(&self) -> Option<String> {
        self.polyglot.startup_note(&self.asr_cfg)
    }

    /// The line the daemon logs once at start-up about Japanese, `None` when
    /// there is nothing worth saying.
    pub fn japanese_note(&self) -> Option<String> {
        self.cjk.startup_note()
    }
    // ---- end Japanese ----------------------------------------------------

    /// Which languages this install can actually re-decode into. Logged once at
    /// start-up so "the flip was only flagged" has a visible cause.
    pub fn arbiters_installed(&self) -> Vec<&'static str> {
        self.arbiters.installed()
    }

    /// The operating point this analyzer was loaded with. The archive resplit
    /// reads it to say whether a turn was too short to cut or merely unchanged.
    pub fn identity_config(&self) -> &IdentityConfig {
        &self.cfg
    }

    pub fn attach_performance(
        &mut self,
        performance: std::sync::Arc<crate::performance::Performance>,
    ) {
        self.performance = performance;
    }

    pub fn embed_model_id(&self) -> &str {
        self.embedder.model_id()
    }

    /// The transducer's stable identity, stamped on every row it decodes
    /// (`Prepared::asr_model_id`). What `crate::light` compares against to
    /// tell whether the light decoder is already the one loaded.
    pub fn asr_model_id(&self) -> &str {
        self.asr.model_id()
    }

    // ---- light mode (0.13.x, `crate::light`) -------------------------------
    /// Swap the live decoder between the default multilingual export and the
    /// smaller English-only one, or leave it alone if it is already the one
    /// asked for.
    ///
    /// A real model load — the same `Decoder::load` start-up already pays for
    /// — so it belongs on the inference thread between turns, never mid-turn
    /// and never called from a socket handler. `models` is the resolved set
    /// the daemon started with; only its ASR leg is repointed, at
    /// [`crate::models::ModelSet::with_asr`].
    ///
    /// Returns `Ok(true)` when it actually reloaded, `Ok(false)` when the
    /// requested decoder was already live, and `Err` when `light` was asked
    /// for and the 110m export is not installed — the caller keeps whatever
    /// was already loaded rather than losing transcription over a missing
    /// optional download.
    pub fn set_light(&mut self, models: &crate::models::ModelSet, light: bool) -> Result<bool> {
        let export = if light {
            &crate::models::FALLBACK_ASR
        } else {
            &crate::models::DEFAULT_ASR
        };
        if self.asr.model_id().starts_with(export.dir) {
            return Ok(false);
        }
        if !models.has_asr_export(export) {
            anyhow::bail!(
                "light mode wants {} but it is not installed under {} — \
                 `recalld models fetch --fallback-asr` (also `--light`) installs it",
                export.dir,
                models.root.display()
            );
        }
        let pointed = models.with_asr(export);
        self.asr = crate::asr::Decoder::load(&pointed, self.cfg.split_turns)?;
        Ok(true)
    }
    // ---- end light mode -----------------------------------------------

    // ---- 0.11.0, partial turns: begin --------------------------------------
    /// Decode an OPEN turn for a provisional caption (`crate::partial`).
    ///
    /// The ASR leg and nothing else: no overlap gate, no embedding, no store.
    /// That is the whole point — a partial is words on a screen for a second
    /// and a half, and everything the analysis leg does besides transcribe is
    /// about writing something down, which a partial never does. It is the same
    /// recogniser instance the finished turn will go through, on the same
    /// thread, so the feature adds no model and no scheduling surface.
    pub fn transcribe_partial(&mut self, samples: &[f32]) -> String {
        let _timer = self.performance.partial_recognition.measure();
        self.asr.transcribe(samples)
    }
    // ---- 0.11.0, partial turns: end ----------------------------------------

    /// Final whole-turn re-read after live slicing; counted separately from captions.
    pub fn transcribe_final(&mut self, samples: &[f32]) -> String {
        let _timer = self.performance.recognition.measure();
        self.asr.transcribe(samples)
    }

    // ---- 0.12.5, sliced turns: begin ---------------------------------------
    /// Decode ONE SLICE of an open turn, or the remainder after the last slice
    /// (`crate::slice`).
    ///
    /// The same recogniser, the same thread and the same contract as
    /// [`Self::transcribe_partial`] beside it — and a different bargain. A
    /// partial re-reads the WHOLE open turn every time, so its words are thrown
    /// away by the next partial and paid for again; a slice reads its own audio
    /// and nobody else's, exactly once, and the words it produces are the words
    /// that go on the row. That is the difference between O(N²) and O(N) over a
    /// turn, and it is the whole claim this feature makes about its cost.
    ///
    /// The caller is responsible for the boundary being one the VAD scored as
    /// not-speech. Handing this half a word is not a worse reading of that
    /// word, it is a different word, and this method has no way to tell.
    pub fn transcribe_slice(&mut self, samples: &[f32]) -> String {
        let _timer = self.performance.partial_recognition.measure();
        self.asr.transcribe(samples)
    }
    // ---- 0.12.5, sliced turns: end -----------------------------------------

    /// All the inference for one turn. Touches no database.
    pub fn prepare(&mut self, samples: &[f32]) -> Result<Prepared> {
        self.prepare_with(samples, Gate::Full)
    }

    // ---- 0.12.4, cutting a turn where the speaker changes ------------------

    /// Where this turn should be cut, and what was said in each piece.
    ///
    /// **Only called when `[identity].split_turns` is on.** With the switch off
    /// the pipeline never reaches here and decodes exactly where it always did,
    /// so an install that has not asked for this feature does not pay a
    /// reordering for it either.
    ///
    /// On, it costs one ERes2Net pass per hop, plus moving the turn's decode
    /// ahead of the row insert. That is what the switch buys its recall with,
    /// and the reason it is measured in FINDINGS §39 rather than assumed.
    ///
    /// `bank` is every prototype in this install's voicebank, `(speaker_id,
    /// source_segment_id, embedding)`, at the moment the caller looked — the
    /// same shape `Store::prototypes_with_source` returns. `exclude_segment`
    /// drops any prototype minted from the very segment being planned, so a
    /// turn already in the bank cannot vote on its own cut (the archive
    /// resplit path; the live path has no segment id yet and passes `None`).
    ///
    /// The bank is not a second detector (FINDINGS §52). `adjacent` still
    /// finds every candidate boundary on its own distance; the bank is asked
    /// one yes/no question per candidate — does its own top-1 voice actually
    /// change here — and a boundary it disagrees with is dropped before
    /// [`crate::turnsplit::cuts`] ever sees it. An empty bank agrees with
    /// nothing, so a fresh install with nobody enrolled cuts nothing yet,
    /// which is the measured shape and not a bug to route around.
    pub fn plan_split(
        &mut self,
        samples: &[f32],
        bank: &[(i64, Option<i64>, crate::embed::Embedding)],
        exclude_segment: Option<i64>,
    ) -> Result<crate::turnsplit::Plan> {
        let (whole, words) = {
            let _timer = self.performance.recognition.measure();
            self.asr.transcribe_timed(samples)
        };
        let shape = crate::turnsplit::Shape::from_config(&self.cfg, SAMPLE_RATE);
        let cuts = if self.cfg.split_turns && shape.cuttable(samples.len()) {
            let windows = shape.windows_of(samples.len());
            let mut vectors = Vec::with_capacity(windows.len());
            for w in &windows {
                let _timer = self.performance.speaker_embedding.measure();
                vectors.push(self.embedder.embed(&samples[w.from..w.to], SAMPLE_RATE)?);
            }
            let filtered: Vec<(i64, crate::embed::Embedding)> = bank
                .iter()
                .filter(|(_, src, _)| match exclude_segment {
                    Some(id) => *src != Some(id),
                    None => true,
                })
                .map(|(sp, _, e)| (*sp, e.clone()))
                .collect();
            let curve = crate::turnsplit::curve(&shape, &windows, &vectors)?;
            let veto = crate::turnsplit::proto_veto(&shape, &vectors, &filtered)?;
            let curve = crate::turnsplit::vetoed(curve, &veto);
            crate::turnsplit::cuts(&shape, samples.len(), &curve)
        } else {
            Vec::new()
        };
        // Not a special case for its own sake: an uncut turn keeps the
        // decoder's own string, punctuation and all, rather than one rebuilt
        // from the word list.
        let uncut = |wordless| crate::turnsplit::Plan {
            pieces: vec![(
                crate::turnsplit::Piece {
                    from: 0,
                    to: samples.len(),
                },
                whole.clone(),
            )],
            wordless,
        };
        let pieces = crate::turnsplit::pieces(samples.len(), &cuts);
        if pieces.len() == 1 {
            return Ok(uncut(false));
        }
        let split: Vec<(crate::turnsplit::Piece, String)> =
            crate::turnsplit::spans(&pieces, SAMPLE_RATE)
                .into_iter()
                .map(|(p, from, to)| (p, crate::asr::words_in_span(&words, from, to)))
                .collect();
        if !crate::turnsplit::every_piece_speaks(&split) {
            return Ok(uncut(true));
        }
        Ok(crate::turnsplit::Plan {
            pieces: split,
            wordless: false,
        })
    }
    // ---- end 0.12.4 --------------------------------------------------------

    /// [`Self::prepare`], with a say in which gate the embedding is behind.
    ///
    /// The overlap detector still RUNS under every gate and its reading is
    /// still stored: it is information about the audio whatever the source, the
    /// correction UI reads it, and `enroll_max_overlap` reads it too. What
    /// [`Gate::SingleSpeaker`] changes is only whether a positive reading is
    /// allowed to throw the embedding away.
    pub fn prepare_with(&mut self, samples: &[f32], gate: Gate) -> Result<Prepared> {
        self.prepare_maybe_said(samples, gate, None)
    }

    /// [`Self::prepare_with`], decoding the audio unless the words are already
    /// in hand. `None` is the path every caller took before 0.12.4 and takes
    /// still while `[identity].split_turns` is off — same call, same cost, same
    /// order.
    pub fn prepare_maybe_said(
        &mut self,
        samples: &[f32],
        gate: Gate,
        said: Option<String>,
    ) -> Result<Prepared> {
        let raw = match said {
            Some(text) => text,
            None => {
                let _timer = self.performance.recognition.measure();
                self.asr.transcribe(samples)
            }
        };
        self.prepare_said(samples, gate, raw)
    }

    /// [`Self::prepare_with`] for audio whose words are already known.
    ///
    /// Two callers, and they arrive from opposite directions.
    ///
    /// A piece of a CUT turn (0.12.4, `[identity].split_turns`): the whole turn
    /// was decoded once, with times, and this piece's words are the ones that
    /// *start* inside it ([`crate::asr::words_in_span`]). Handing them in
    /// rather than decoding the piece is what makes the split lossless — every
    /// word of the turn belongs to exactly one piece by construction, where two
    /// independent decodes could drop a word straddling the cut or spell it
    /// twice — and it is also the cheaper of the two, one decode instead of N.
    ///
    /// A SLICED turn (0.12.5, `crate::slice`): the opposite arrangement, and
    /// the same saving. Its audio was decoded piece by piece while the person
    /// was still speaking and the pieces joined, so decoding it again here
    /// would spend the turn's whole cost twice and throw away the reading that
    /// is already on somebody's screen.
    ///
    /// Either way the rest is identical, deliberately: the overlap detector
    /// still runs, the gate still decides, and the embedding is still taken
    /// over the WHOLE audio handed in — which is why a sliced turn is joined
    /// into one row rather than left as several, since six embeddings of six
    /// fragments are six weaker claims about the same person.
    pub fn prepare_said(&mut self, samples: &[f32], gate: Gate, raw: String) -> Result<Prepared> {
        let duration_s = samples.len() as f32 / SAMPLE_RATE as f32;
        let overlap_frac = {
            let _timer = self.performance.overlap.measure();
            self.overlap.overlap_frac(samples)?
        };

        // An empty transcript is stored as NULL rather than "": it keeps the
        // full-text index free of empty documents and makes "has a transcript"
        // a single IS NOT NULL.
        let text = (!normalise_words(&raw).is_empty()).then_some(raw);

        let refusal = match gate {
            Gate::Full => identity::gate(&self.cfg, overlap_frac, duration_s),
            Gate::SingleSpeaker => {
                (duration_s < self.cfg.min_duration_s).then_some(identity::Refusal::TooShort)
            }
        };
        let embedding = match refusal {
            Some(_) => None,
            None => {
                let _timer = self.performance.speaker_embedding.measure();
                Some(self.embedder.embed(samples, SAMPLE_RATE)?)
            }
        };
        Ok(Prepared {
            overlap_frac,
            duration_s,
            text,
            asr_model_id: self.asr.model_id().to_string(),
            model_lang: self.asr.lang(),
            embedding,
            refusal,
        })
    }

    /// Write what `prepare` learned onto the row and resolve identity against
    /// the voicebank. Cheap: no model runs here.
    pub fn commit(
        &self,
        store: &Store,
        segment_id: i64,
        prepared: Prepared,
        now_utc_ns: i64,
    ) -> Result<Outcome> {
        let _timer = self.performance.transcript_commit.measure();
        let Prepared {
            overlap_frac,
            duration_s,
            text,
            asr_model_id,
            model_lang,
            embedding,
            refusal,
        } = prepared;

        let (lang, lang_via) = language_of(text.as_deref(), model_lang);
        store.set_segment_analysis(
            segment_id,
            &SegmentAnalysis {
                lang,
                lang_via,
                text: text.clone(),
                asr_model_id: Some(asr_model_id),
                overlap_frac: Some(overlap_frac),
            },
        )?;

        let embedding = match embedding {
            Some(e) => e,
            None => {
                let refusal = refusal.unwrap_or(Refusal::TooShort);
                debug!(
                    segment_id,
                    overlap_frac,
                    duration_s,
                    "no speaker: {}",
                    refusal.as_str()
                );
                store.set_segment_speaker(segment_id, None, None)?;
                return Ok(Outcome {
                    lid_checked: false,
                    routed_cjk: None,
                    routed_other: None,
                    overlap_frac,
                    text,
                    decision: Decision::Refused(refusal),
                    speaker_id: None,
                    match_score: None,
                    enrolled: false,
                    golden: false,
                    language_fix: None,
                    prior: None,
                    also_changed: Vec::new(),
                });
            }
        };

        store.store_embedding(segment_id, &embedding)?;
        let bank = store.prototypes(&embedding.model_id)?;
        // ---- 0.11.0: learned identity ---------------------------------
        // The learned space, if one is installed, goes in front of the
        // cosine — both sides of it, which is why the probe and the bank are
        // mapped together and never separately. A failure costs the learned
        // space and never the label: the ladder then sees exactly what 0.10.2
        // would have shown it. Nothing is installed until a nightly
        // calibration has beaten the raw space on held-out ground truth, so on
        // a fresh install this is a NULL lookup and a branch.
        let (probe, bank) = match learned_space(store, &self.cfg, &embedding, &bank) {
            Ok(Some(mapped)) => mapped,
            Ok(None) => (embedding.clone(), bank),
            Err(e) => {
                warn!(segment_id, "the learned space could not be applied: {e:#}");
                (embedding.clone(), bank)
            }
        };
        let thresholds = learned_thresholds(store, &self.cfg);
        // ---- end 0.11.0 -----------------------------------------------
        // 0.12.0: how a voice's several prototypes become the one score the
        // ladder compares. A read failure is not a reason to stop labelling —
        // the fallback is the rule every version before 0.12.0 used.
        let aggregate = learned_aggregate(store, &self.cfg);
        let ranked = {
            let _timer = self.performance.speaker_matching.measure();
            identity::rank_with(&probe, &bank, aggregate)?
        };
        // The source-aware prior (0.11.0), between ranking and deciding —
        // which is the only place it can be: it needs the scores to weigh a
        // foreign candidate against a native one, and it has to be able to
        // remove a candidate before the mint rule asks whether the bank was
        // empty. A failure here costs the prior, never the label: the ladder
        // then sees the list it would have seen in 0.10.0.
        let prior = match crate::identity_prior::for_segment(
            store,
            &self.cfg,
            self.prior_sources(),
            segment_id,
            &ranked,
        ) {
            Ok(p) => p,
            Err(e) => {
                warn!(segment_id, "the source prior could not be applied: {e:#}");
                crate::identity_prior::Applied::untouched(&ranked)
            }
        };
        if let Some(note) = prior.note() {
            info!(segment_id, "source prior: ignored {note}");
        }
        let ranked = prior.kept.clone();
        // The word count is the mint bar's second half (0.6.1): a new identity
        // needs seconds *and* words. Matching an existing one never asks.
        let words = text.as_deref().map(lang::word_count).unwrap_or(0);
        let decision = identity::decide_with(
            &self.cfg,
            &thresholds,
            overlap_frac,
            duration_s,
            words,
            &ranked,
        );

        let (speaker_id, match_score, enrolled) = match &decision {
            // `gate` already ran, so this arm is unreachable in practice; it
            // exists so a future gate change cannot silently label anyway.
            Decision::Refused(_) => (None, None, false),
            // `decide` never returns this: pinning is what `commit_mic` does
            // instead of asking. The arm exists so the match stays total.
            Decision::Pinned { speaker_id } => (Some(*speaker_id), None, false),
            Decision::Matched {
                speaker_id,
                score,
                enroll,
            } => {
                if *enroll {
                    self.enroll(store, *speaker_id, &embedding, segment_id, now_utc_ns)?;
                }
                (Some(*speaker_id), Some(*score), *enroll)
            }
            Decision::Mint { best_score } => {
                let id = store.mint_speaker(now_utc_ns)?;
                // The turn seeds the new speaker. It already passed the overlap
                // and duration gates, and without a seed no voice could ever be
                // recognised a second time.
                self.enroll(store, id, &embedding, segment_id, now_utc_ns)?;
                (Some(id), *best_score, true)
            }
            // Below the mint bar: nothing matched and this turn is too slight
            // to be an identity. The embedding is already stored, so a later
            // reassignment or split still has the evidence — only the voicebank
            // is spared a row nobody could ever name (0.6.1).
            // 0.12.2: the top candidate cleared the global operating point
            // and failed only its own learned bar. No name, and no phantom.
            Decision::Declined { best_score, bar } => {
                debug!(
                    segment_id,
                    best_score, bar, "a fitted bar declined the turn: no new voice"
                );
                (None, None, false)
            }
            Decision::TooSlight {
                duration_s, words, ..
            } => {
                debug!(
                    segment_id,
                    duration_s, words, "below the mint bar: no new voice"
                );
                (None, None, false)
            }
        };
        store.set_segment_speaker(segment_id, speaker_id, match_score)?;

        Ok(Outcome {
            overlap_frac,
            text,
            decision,
            speaker_id,
            match_score,
            enrolled,
            golden: false,
            language_fix: None,
            prior: Some(prior),
            also_changed: Vec::new(),
            lid_checked: false,
            routed_cjk: None,
            routed_other: None,
        })
    }

    /// The microphone leg: write what `prepare` learned, then label the turn
    /// with the pinned "You" speaker **without consulting the voicebank**.
    ///
    /// The design note this implements (DESIGN §5) is that a mic tap is a free
    /// perfect label, and the corollary is that it must not be laundered into
    /// looking like a match: `match_score` stays NULL, because there was no
    /// comparison to score. `overlap_frac` is still stored — when the user runs
    /// loudspeakers the mic hears the room talking back, and the correction UI
    /// has to be able to see that even though the name is certain.
    ///
    /// Enrolment keeps every gate the matching leg has, minus the two that are
    /// about *identifying* (threshold and margin): overlap ≤ `enroll_max_overlap`
    /// and duration ≥ `enroll_min_duration_s`. That is the payoff — prototypes
    /// for the one voice the daemon can be certain about, plus up to
    /// `max_goldens` kept clips for a future model migration.
    pub fn commit_mic(
        &self,
        store: &Store,
        segment_id: i64,
        prepared: Prepared,
        mic: &MicEnroll<'_>,
        now_utc_ns: i64,
    ) -> Result<Outcome> {
        self.commit_pinned(
            store,
            segment_id,
            prepared,
            &PinnedLeg::mic(mic),
            now_utc_ns,
        )
    }

    /// The general form of [`Self::commit_mic`] (0.12.1): a turn whose speaker
    /// is known from where the audio came from rather than from what is in it.
    ///
    /// Everything the doc comment above says about the microphone holds here
    /// word for word — the label is provenance, `match_score` stays NULL
    /// because nothing was compared, and enrolment keeps the two gates that are
    /// about the *recording* while dropping the two that are about
    /// *identifying*. The only judgements the caller gets to make are whose
    /// voice it is, what to stamp, whether a clip may be kept, and whether the
    /// voicebank may be written to at all.
    pub fn commit_pinned(
        &self,
        store: &Store,
        segment_id: i64,
        prepared: Prepared,
        pin: &PinnedLeg<'_>,
        now_utc_ns: i64,
    ) -> Result<Outcome> {
        let _timer = self.performance.transcript_commit.measure();
        let Prepared {
            overlap_frac,
            duration_s,
            text,
            asr_model_id,
            model_lang,
            embedding,
            refusal,
        } = prepared;

        let (lang, lang_via) = language_of(text.as_deref(), model_lang);
        store.set_segment_analysis(
            segment_id,
            &SegmentAnalysis {
                lang,
                lang_via,
                text: text.clone(),
                asr_model_id: Some(asr_model_id),
                overlap_frac: Some(overlap_frac),
            },
        )?;
        // Provenance, before anything else can fail: the label does not depend
        // on the embedder having produced a vector. It is recorded as such —
        // `label_via = "mic"` or `"discord-stream"` — so nothing downstream has
        // to infer it from a NULL score, which is a thing three other paths
        // also produce.
        store.set_segment_speaker_via(
            segment_id,
            Some(pin.speaker_id),
            None,
            Some(pin.label_via),
        )?;

        let mut enrolled = false;
        let mut golden = false;
        if let Some(embedding) = embedding {
            store.store_embedding(segment_id, &embedding)?;
            if pin.enrol
                && overlap_frac <= self.cfg.enroll_max_overlap
                && duration_s >= self.cfg.enroll_min_duration_s
            {
                // `enrolled` follows what the store actually did. Every slot
                // being golden means the vector is dropped — hand-enrolled
                // audio outranks anything inferred — and reporting that as an
                // enrolment made `mic_enrolled` count turns that added nothing
                // to the bank (audit finding #25).
                enrolled = store
                    .add_prototype(
                        pin.speaker_id,
                        &embedding,
                        Some(segment_id),
                        false,
                        self.cfg.max_prototypes,
                        now_utc_ns,
                    )?
                    .is_some();
                // The golden is kept on the strength of the audio, not of the
                // enrolment: a turn good enough to enrol from is good enough to
                // keep whether or not the bank had room for its vector.
                //
                // Only for a source that is allowed to keep one. A golden
                // outlives retention by design, and the case for that is "this
                // is the user's own voice and a future embedding model will
                // need it" — which is not a case about anybody else.
                if let Some((data_dir, max_goldens)) = pin.goldens {
                    golden = keep_golden(
                        store,
                        &MicEnroll {
                            speaker_id: pin.speaker_id,
                            data_dir,
                            max_goldens,
                        },
                        segment_id,
                        duration_s,
                    )?;
                }
            }
        } else {
            debug!(
                segment_id,
                overlap_frac,
                duration_s,
                via = pin.label_via,
                "pinned: labelled but not enrolled ({})",
                refusal.unwrap_or(Refusal::TooShort).as_str()
            );
        }

        Ok(Outcome {
            overlap_frac,
            text,
            decision: Decision::Pinned {
                speaker_id: pin.speaker_id,
            },
            speaker_id: Some(pin.speaker_id),
            match_score: None,
            enrolled,
            golden,
            language_fix: None,
            // A pinned leg never consults the voicebank, so there was no
            // candidate list for the prior to have an opinion about.
            prior: None,
            also_changed: Vec::new(),
            lid_checked: false,
            routed_cjk: None,
            routed_other: None,
        })
    }

    /// The two corrections that can only run once the row exists and its
    /// speaker is known: the wrong-language re-decode, and giving the *previous*
    /// turn a name now that this one has proved what came after it.
    ///
    /// Both are best-effort. Neither may cost the segment that was just
    /// analysed: a failure is logged and the row stands as committed.
    fn after_commit(
        &mut self,
        store: &Store,
        segment_id: i64,
        outcome: &mut Outcome,
        samples: &[f32],
    ) {
        let performance = std::sync::Arc::clone(&self.performance);
        let _timer = performance.refinement.measure();
        // ---- Japanese, Korean, Chinese (`crate::asr_cjk`) ----------------
        //
        // FIRST, before the declared-language correction, because the two
        // answer different questions and only one of them can be right about a
        // Japanese turn. `correct_language` asks "do the words disagree with
        // what this voice was declared to speak"; a transliterated Japanese
        // turn's words are `Unclear` and disagree with nothing, so that check
        // passes it silently — and if it did fire it would hand German audio
        // to a German arbiter over a Japanese sentence. When the router
        // settles a row there is nothing left for the declaration check to
        // decide, so it is skipped.
        //
        // Best-effort like every other correction here: a language nobody
        // could settle never costs a recording.
        let mut settled_cjk = false;
        // The identifier's answer, kept for the polyglot route below (0.11.8).
        let mut lid_heard = None;
        match crate::asr_cjk::route_segment(
            &mut self.cjk,
            store,
            crate::asr_cjk::Turn {
                segment_id,
                declared: outcome
                    .speaker_id
                    .and_then(|id| store.speaker_languages(id).ok().flatten())
                    .as_ref(),
                text: outcome.text.as_deref(),
                samples,
                lang_cfg: &self.lang_cfg,
                asr_cfg: &self.asr_cfg,
            },
            crate::clock::utc_now_ns(),
        ) {
            Ok(checked) => {
                outcome.lid_checked = checked.lid_checked;
                lid_heard = checked.heard;
                if let Some(routed) = checked.routed {
                    outcome.text = Some(routed.text.clone());
                    outcome.routed_cjk = Some(routed);
                    settled_cjk = true;
                }
            }
            Err(e) => warn!(segment_id, "the CJK route failed: {e:#}"),
        }
        // ---- end Japanese, Korean, Chinese --------------------------------

        // ---- the other languages (0.11.8, `crate::polyglot`) --------------
        //
        // SECOND, and only on a turn the CJK route left alone. It reuses
        // that route's LID reading rather than asking again — the identifier
        // is the only cost either feature has — which is why this reads
        // `heard` off the `Checked` above instead of holding an identifier of
        // its own. `post_route` refuses `ja`, so the two cannot both fire.
        //
        // Best-effort, like everything else in this function: a language
        // nobody could settle never costs a recording.
        if !settled_cjk && let Some(heard) = lid_heard.as_ref() {
            match crate::polyglot::route_segment(
                &mut self.polyglot,
                store,
                crate::polyglot::Turn {
                    segment_id,
                    text: outcome.text.as_deref(),
                    samples,
                    heard: Some(heard),
                    lang_cfg: &self.lang_cfg,
                    asr_cfg: &self.asr_cfg,
                },
                crate::clock::utc_now_ns(),
            ) {
                Ok(Some(routed)) => {
                    outcome.text = Some(routed.text.clone());
                    outcome.routed_other = Some(routed);
                    // `settled_cjk` is misnamed as of this branch and is left
                    // alone deliberately: what it actually gates is "this row's
                    // language was settled by ear, so the declaration check
                    // must not decide it again", which is exactly as true here.
                    settled_cjk = true;
                }
                Ok(None) => {}
                Err(e) => warn!(segment_id, "the polyglot route failed: {e:#}"),
            }
        }
        // ---- end the other languages ---------------------------------------

        if let Some(speaker_id) = outcome.speaker_id.filter(|_| !settled_cjk) {
            match self.correct_language(
                store,
                segment_id,
                speaker_id,
                outcome.text.as_deref(),
                samples,
            ) {
                Ok(fix) => {
                    if let Some(LanguageFix::Redecoded { text, .. }) = &fix {
                        outcome.text = Some(text.clone());
                    }
                    outcome.language_fix = fix;
                }
                Err(e) => warn!(segment_id, "language correction failed: {e:#}"),
            }
        }
        match crate::proximity::apply(store, &self.cfg, segment_id) {
            Ok(Some(id)) => outcome.also_changed.push(id),
            Ok(None) => {}
            Err(e) => warn!(segment_id, "proximity inheritance failed: {e:#}"),
        }
    }

    /// `prepare` then `commit`, for callers that hold the store exclusively.
    pub fn process(
        &mut self,
        store: &Store,
        segment_id: i64,
        samples: &[f32],
        now_utc_ns: i64,
    ) -> Result<Outcome> {
        let prepared = self.prepare(samples)?;
        let mut outcome = self.commit(store, segment_id, prepared, now_utc_ns)?;
        self.after_commit(store, segment_id, &mut outcome, samples);
        Ok(outcome)
    }

    /// `prepare` then `commit_mic`, for callers that hold the store exclusively.
    pub fn process_mic(
        &mut self,
        store: &Store,
        segment_id: i64,
        samples: &[f32],
        mic: &MicEnroll<'_>,
        now_utc_ns: i64,
    ) -> Result<Outcome> {
        let prepared = self.prepare(samples)?;
        let mut outcome = self.commit_mic(store, segment_id, prepared, mic, now_utc_ns)?;
        self.after_commit(store, segment_id, &mut outcome, samples);
        Ok(outcome)
    }

    /// `prepare_with(SingleSpeaker)` then `commit_pinned` (0.12.1), for callers
    /// that hold the store exclusively — the offline leg, and the tests.
    pub fn process_pinned(
        &mut self,
        store: &Store,
        segment_id: i64,
        samples: &[f32],
        pin: &PinnedLeg<'_>,
        now_utc_ns: i64,
    ) -> Result<Outcome> {
        let prepared = self.prepare_with(samples, Gate::SingleSpeaker)?;
        let mut outcome = self.commit_pinned(store, segment_id, prepared, pin, now_utc_ns)?;
        self.after_commit(store, segment_id, &mut outcome, samples);
        Ok(outcome)
    }

    /// Act on a transcript that disagrees with its speaker's declared language.
    ///
    /// This is the correction the 0.6.1 measurement asks for. `spike/lang_flip.py`
    /// found the multilingual export decoding German fragments *as English* on
    /// 12% of 1 s windows and 5% of 2 s ones, against a median real turn of
    /// 2.4 s — so on a lobby of short turns a German speaker's transcript is
    /// wrong several times an hour, silently, in a way full-utterance benchmarks
    /// never show. Knowing which languages a voice actually speaks turns that
    /// from an unfixable annoyance into a decidable question.
    ///
    /// Only a speaker pinned to **exactly one** language can be corrected: a
    /// bilingual voice speaking German is not a mistake, and neither is a voice
    /// nobody has said anything about (the default).
    ///
    /// The two directions used to be asymmetric, because the catalogue was:
    ///
    /// * **English speaker, German-looking transcript** → decode the audio
    ///   again with the English-only export, whose language is a hard property
    ///   of the model rather than a hint.
    /// * **German speaker, English-looking transcript** → nothing to re-decode
    ///   *with*. Flag only.
    ///
    /// 0.7.7 closes that gap: `crate::arbiter` adds Whisper forced to German,
    /// so both directions now go through the same [`Arbiters::arbitrate`] with
    /// the same measured guards, and the second bullet is only reached when the
    /// optional arbiter is not installed. What has *not* changed is what the
    /// failure looks like: the words are kept (they are the only record of what
    /// was said) and the row is marked, with `lang` NULL, because the
    /// classifier and the declaration cannot both be right and this daemon
    /// cannot tell which is wrong.
    ///
    /// Identity is untouched either way: the label came from the voice, and a
    /// voice does not become less recognisable by switching language.
    pub fn correct_language(
        &mut self,
        store: &Store,
        segment_id: i64,
        speaker_id: i64,
        text: Option<&str>,
        samples: &[f32],
    ) -> Result<Option<LanguageFix>> {
        let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
            return Ok(None);
        };
        let declared = store.speaker_languages(speaker_id)?;
        let Some(want) = lang::sole_language(declared.as_ref()) else {
            return Ok(None);
        };
        let read = lang::classify(text);
        // "Unclear" and "empty" disagree with nothing: a name, a number and a
        // grunt are not evidence that the wrong language was decoded.
        let Some(got) = read.tag() else {
            return Ok(None);
        };
        if got == want {
            return Ok(None);
        }
        // 0.11.x: a tag with no arbiter behind it is NOT English. See
        // `arbiter::target_for` — the catch-all this replaced sent a voice
        // declared to speak Japanese to the English decoder and stored the
        // result as `lang = "ja"`. `None` here falls through to the marking
        // branch below, which is what "the classifier and the declaration
        // cannot both be right and this daemon cannot tell which is wrong"
        // already means everywhere else in this function.
        let target = crate::arbiter::target_for(want);
        // Already decoding under that exact constraint: the text is what this
        // model says, and running it twice would say it again.
        let already_constrained = self.asr.lang() == Some(want);
        let outcome = match target {
            Some(target) if !already_constrained => {
                self.arbiters.arbitrate(target, samples, &self.lang_cfg)
            }
            Some(_) => Arbitration::Rejected {
                read_as: got,
                words: 0,
            },
            None => Arbitration::Unavailable,
        };

        if let Arbitration::Replaced { text, model_id } = outcome {
            store.set_segment_text_from_redecode(segment_id, &text, want, &model_id)?;
            info!(
                segment_id,
                speaker = speaker_id,
                model = %model_id,
                read_as = got,
                "re-decoded a transcript that disagreed with the voice's declared language"
            );
            return Ok(Some(LanguageFix::Redecoded {
                text,
                asr_model_id: model_id,
                lang: want.to_string(),
            }));
        }

        // Unsettled in either direction: mark it and keep the words.
        store.mark_segment_language_mismatch(segment_id)?;
        debug!(
            segment_id,
            speaker = speaker_id,
            declared = want,
            reads_as = got,
            outcome = ?outcome,
            "transcript disagrees with the speaker's declared language; marked, not changed"
        );
        Ok(Some(LanguageFix::Marked { read_as: got }))
    }

    /// The conversational language prior (0.7.7, `crate::langctx`).
    ///
    /// Runs **after** threading, because the thread is the thing it reads —
    /// which is also why it is not part of `after_commit`: at that point the
    /// turn has a speaker but no conversation yet.
    ///
    /// Best-effort like every other correction here. A failure is logged by the
    /// caller and the row stands as committed.
    pub fn apply_language_context(
        &mut self,
        store: &Store,
        segment_id: i64,
        samples: &[f32],
    ) -> Result<Option<ContextFix>> {
        // ---- Japanese (0.11.0) -------------------------------------------
        //
        // A row the Japanese router settled is not the thread prior's
        // business, and this guard is load-bearing rather than tidy:
        // `langctx::decide` treats `re-decode` and `mismatch` as settled and
        // has never heard of `lid`, and `lang::classify` on kana returns
        // `Unclear` — which is the *inherit* branch. Without this line a
        // correctly re-decoded Japanese turn would be stamped "de" by the
        // German conversation around it, seconds after being fixed.
        if crate::asr_cjk::settled_by_lid(store, segment_id)? {
            return Ok(None);
        }
        // ---- end Japanese ------------------------------------------------
        let (intent, _context) = langctx::intent_for(store, &self.lang_cfg, segment_id)?;
        match intent {
            Intent::Nothing => Ok(None),
            Intent::Inherit(lang) => langctx::commit_inheritance(store, segment_id, lang),
            Intent::Arbitrate { want, read_as } => {
                let outcome = self.arbiters.arbitrate(want, samples, &self.lang_cfg);
                langctx::commit_arbitration(store, segment_id, want, read_as, outcome)
            }
        }
    }

    fn enroll(
        &self,
        store: &Store,
        speaker_id: i64,
        embedding: &Embedding,
        segment_id: i64,
        now_utc_ns: i64,
    ) -> Result<()> {
        store.add_prototype(
            speaker_id,
            embedding,
            Some(segment_id),
            false,
            self.cfg.max_prototypes,
            now_utc_ns,
        )?;
        Ok(())
    }
}

/// Keep this turn's audio as a golden sample, if it earns a slot.
///
/// "Up to N, longest": under the cap anything qualifying is kept; at the cap a
/// longer clip replaces the shortest one, because a golden exists to re-enrol a
/// future model and three seconds of speech does that better than one. Written
/// at most once per segment — the path carries the segment id, so a re-analysis
/// of the same turn finds its own file already there and does nothing.
fn keep_golden(
    store: &Store,
    mic: &MicEnroll<'_>,
    segment_id: i64,
    duration_s: f32,
) -> Result<bool> {
    if mic.max_goldens == 0 {
        return Ok(false);
    }
    let rel = golden_path(mic.speaker_id, segment_id);
    let rel_str = rel.to_string_lossy().to_string();

    let existing = store.golden_samples_for(mic.speaker_id)?;
    if existing.iter().any(|g| g.audio_path == rel_str) {
        return Ok(false);
    }
    let mut evict = None;
    if existing.len() >= mic.max_goldens {
        // `golden_samples_for` is longest-first, so the last row is the one to
        // beat. Not beating it is the common case and costs nothing.
        let Some(shortest) = existing.last() else {
            return Ok(false);
        };
        if shortest.duration_s >= duration_s {
            return Ok(false);
        }
        evict = Some(shortest.clone());
    }

    // Copy rather than move: the segment's own WAV still belongs to the
    // transcript and to `segments.audio`, and retention still owns its life.
    let Some((source_rel, _, _)) = store.segment_audio(segment_id)? else {
        return Ok(false);
    };
    if source_rel.is_empty() {
        return Ok(false);
    }
    let dst = mic.data_dir.join(&rel);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::copy(mic.data_dir.join(&source_rel), &dst)
        .with_context(|| format!("copying {source_rel} to {}", dst.display()))?;
    store.add_golden_sample(mic.speaker_id, &rel_str, duration_s)?;

    if let Some(old) = evict {
        // Row first, then the file: a file with no row is residue the
        // reconciliation sweep understands; a row with no file is a lie.
        if let Some(path) = store.delete_golden_sample(old.id)?
            && !path.is_empty()
        {
            let _ = std::fs::remove_file(mic.data_dir.join(path));
        }
    }
    info!(
        segment_id,
        speaker = mic.speaker_id,
        duration_s,
        path = %rel_str,
        "microphone: kept a golden sample"
    );
    Ok(true)
}

/// Counters the daemon logs on shutdown.
#[derive(Default)]
pub struct AnalysisStats {
    pub analysed: std::sync::atomic::AtomicU64,
    pub labelled: std::sync::atomic::AtomicU64,
    pub refused_overlap: std::sync::atomic::AtomicU64,
    /// Turns labelled from the microphone's provenance rather than a match.
    pub mic_segments: std::sync::atomic::AtomicU64,
    pub mic_enrolled: std::sync::atomic::AtomicU64,
    pub mic_goldens: std::sync::atomic::AtomicU64,
    /// Turns that matched nobody and were too slight to mint a voice (0.6.1).
    pub too_slight: std::sync::atomic::AtomicU64,
    /// Turns a voice's own **fitted** bar turned down, which under 0.12.2 is a
    /// decline rather than a new identity (FINDINGS §46).
    pub declined_fitted: std::sync::atomic::AtomicU64,
    /// Turns that took their name from the turns around them.
    pub proximity_labelled: std::sync::atomic::AtomicU64,
    /// Transcripts re-decoded under a language constraint, either direction.
    pub redecoded: std::sync::atomic::AtomicU64,
    /// Transcripts that disagree with their speaker's declared language and
    /// could not be corrected.
    pub lang_mismatch: std::sync::atomic::AtomicU64,

    // ---- the conversational language prior (0.7.7) -----------------------
    /// Turns nothing could be read out of that took their conversation's
    /// language instead (`lang_via = "context"`). Text unchanged.
    pub context_stamped: std::sync::atomic::AtomicU64,
    /// Turns that read as the opposite of a strong thread context and were
    /// therefore handed to an arbiter — whatever the arbiter then said.
    pub flips_suspected: std::sync::atomic::AtomicU64,
    /// The two directions of a successful re-decode, split out because they run
    /// on different models with different measured behaviour: `de` is Whisper
    /// forced to German, `en` is the Parakeet 110m.
    pub redecoded_de: std::sync::atomic::AtomicU64,
    pub redecoded_en: std::sync::atomic::AtomicU64,
    /// Rows `recalld lang repair` rewrote out of the mismatch backlog.
    pub repairs: std::sync::atomic::AtomicU64,

    // ---- the source-aware prior (0.11.0) ---------------------------------
    /// Candidates removed because they are foreign to the segment's source and
    /// did not clear the raised bar. Counted per candidate, not per turn: one
    /// turn can drop several.
    pub prior_foreign: std::sync::atomic::AtomicU64,
    /// Candidates removed because Discord said the linked account was not
    /// speaking anywhere near the turn.
    pub prior_absent: std::sync::atomic::AtomicU64,
    /// Labels that WERE given to a foreign voice, over the raised bar. The
    /// other half of the story, and the one worth watching: if this number is
    /// large the margin is too low.
    pub prior_foreign_kept: std::sync::atomic::AtomicU64,

    // ---- Japanese, Korean, Chinese (0.11.0/0.11.6, `crate::asr_cjk`) -----
    /// Turns handed to the spoken-language identifier — the ones whose
    /// transcript nobody could read. This is what the feature COSTS, and the
    /// two counters are reported side by side on purpose: a `lid_checked` that
    /// climbs while `routed_ja` stays at zero means the daemon is paying 20 ms
    /// a turn to be told "German", and the answer is to look at why so many
    /// transcripts are unreadable rather than to look at Japanese.
    pub lid_checked: std::sync::atomic::AtomicU64,
    /// Turns a CJK decoder re-read, whose words it replaced, and whose
    /// language is now `ja`, `ko` or `zh` via `lid`. Counted per language
    /// rather than as one total, because the three arms have different
    /// decoders, different downloads and different accuracies (FINDINGS §27),
    /// and a number that mixed them could not answer "is Korean working".
    pub routed_ja: std::sync::atomic::AtomicU64,
    pub routed_ko: std::sync::atomic::AtomicU64,
    pub routed_zh: std::sync::atomic::AtomicU64,
    // ---- the other languages (0.11.8, `crate::polyglot`) -------------------
    /// Turns re-decoded by a decoder forced to the language the identifier
    /// named, over all of fr/es/it/pt/nl/pl.
    pub routed_other: std::sync::atomic::AtomicU64,
    /// The same, split by language and **positionally aligned with
    /// [`crate::polyglot::ROUTABLE`]** — slot `i` counts
    /// `ROUTABLE[i]`.
    ///
    /// An array rather than a map behind a lock: the set is known at compile
    /// time, and this is incremented on the inference thread where a mutex for
    /// six counters would be the most expensive thing about the feature. Read
    /// it through [`AnalysisStats::routed_other_counts`], which pairs the slots
    /// back up with their tags so no caller has to know the ordering.
    pub routed_other_by_lang: [std::sync::atomic::AtomicU64; crate::polyglot::ROUTABLE.len()],
}

impl AnalysisStats {
    /// Count what the language prior did to one row (0.7.7). Called from the
    /// pipeline, after threading — `record` cannot do it, because the prior
    /// runs later than the outcome it would have to hang off.
    pub fn record_context(&self, fix: Option<&crate::langctx::ContextFix>) {
        use crate::langctx::ContextFix;
        match fix {
            Some(ContextFix::Inherited { .. }) => {
                self.context_stamped.fetch_add(1, Ordering::Relaxed);
            }
            Some(ContextFix::Redecoded { lang, .. }) => {
                self.flips_suspected.fetch_add(1, Ordering::Relaxed);
                self.redecoded.fetch_add(1, Ordering::Relaxed);
                self.count_direction(lang);
            }
            Some(ContextFix::Marked { .. }) => {
                self.flips_suspected.fetch_add(1, Ordering::Relaxed);
                self.lang_mismatch.fetch_add(1, Ordering::Relaxed);
            }
            None => {}
        }
    }

    fn count_direction(&self, lang: &str) {
        match lang {
            "de" => self.redecoded_de.fetch_add(1, Ordering::Relaxed),
            _ => self.redecoded_en.fetch_add(1, Ordering::Relaxed),
        };
    }

    pub fn record(&self, outcome: &Outcome) {
        self.analysed.fetch_add(1, Ordering::Relaxed);
        match &outcome.decision {
            Decision::Refused(Refusal::Overlapped) => {
                self.refused_overlap.fetch_add(1, Ordering::Relaxed);
            }
            Decision::Matched { .. } | Decision::Mint { .. } => {
                self.labelled.fetch_add(1, Ordering::Relaxed);
            }
            Decision::TooSlight { .. } => {
                self.too_slight.fetch_add(1, Ordering::Relaxed);
            }
            Decision::Declined { .. } => {
                self.declined_fitted.fetch_add(1, Ordering::Relaxed);
            }
            Decision::Pinned { .. } => {
                self.labelled.fetch_add(1, Ordering::Relaxed);
                self.mic_segments.fetch_add(1, Ordering::Relaxed);
                if outcome.enrolled {
                    self.mic_enrolled.fetch_add(1, Ordering::Relaxed);
                }
                if outcome.golden {
                    self.mic_goldens.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ => {}
        }
        match &outcome.language_fix {
            Some(LanguageFix::Redecoded { lang, .. }) => {
                self.redecoded.fetch_add(1, Ordering::Relaxed);
                self.count_direction(lang);
            }
            Some(LanguageFix::Marked { .. }) => {
                self.lang_mismatch.fetch_add(1, Ordering::Relaxed);
            }
            None => {}
        }
        self.proximity_labelled
            .fetch_add(outcome.also_changed.len() as u64, Ordering::Relaxed);
        // What the source prior did to this turn's candidate list (0.11.0).
        if let Some(prior) = &outcome.prior {
            use crate::identity_prior::Dropped;
            for (_, _, why) in &prior.dropped {
                match why {
                    Dropped::HardAbsent => self.prior_absent.fetch_add(1, Ordering::Relaxed),
                    Dropped::ForeignBelowBar | Dropped::ForeignBehindNative => {
                        self.prior_foreign.fetch_add(1, Ordering::Relaxed)
                    }
                };
            }
            self.prior_foreign_kept
                .fetch_add(prior.foreign_kept.len() as u64, Ordering::Relaxed);
        }
        // What the Japanese router did to this turn (0.11.0).
        if outcome.lid_checked {
            self.lid_checked.fetch_add(1, Ordering::Relaxed);
        }
        match outcome.routed_cjk.as_ref().map(|r| r.lang) {
            Some(crate::asr_cjk::JA) => self.routed_ja.fetch_add(1, Ordering::Relaxed),
            Some(crate::asr_cjk::KO) => self.routed_ko.fetch_add(1, Ordering::Relaxed),
            Some(crate::asr_cjk::ZH) => self.routed_zh.fetch_add(1, Ordering::Relaxed),
            _ => 0,
        };
        // …and what the other half of the same route did (0.11.8).
        if let Some(routed) = outcome.routed_other.as_ref() {
            self.routed_other.fetch_add(1, Ordering::Relaxed);
            if let Some(i) = crate::polyglot::ROUTABLE
                .iter()
                .position(|t| *t == routed.lang)
            {
                self.routed_other_by_lang[i].fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// The per-language counts, paired back up with their tags.
    ///
    /// The one way anything outside this module should read
    /// `routed_other_by_lang`: the array is positional, and a caller that
    /// indexed it itself would silently report French numbers under Spanish
    /// the first time a language is added to the catalogue.
    pub fn routed_other_counts(&self) -> Vec<(&'static str, u64)> {
        crate::polyglot::ROUTABLE
            .iter()
            .zip(self.routed_other_by_lang.iter())
            .map(|(t, n)| (*t, n.load(Ordering::Relaxed)))
            .filter(|(_, n)| *n > 0)
            .collect()
    }
}

/// The language to store for a transcript, and where it came from.
///
/// Three answers, in order of how much they can be trusted:
///
/// 1. **The model said so.** An English-only export cannot produce German, so
///    its tag is a property of the decoder rather than a reading of the words.
/// 2. **The classifier read it.** The multilingual export returns text and no
///    language at all, so somebody has to look at the words — and that is worth
///    doing precisely because the model is *wrong* about language often enough
///    to matter on short turns (FINDINGS §10 / `spike/lang_flip.py`).
/// 3. **Nobody knows.** A tie, a name, a number, or no words: NULL, and no
///    provenance either. "I could not tell" is a real answer and is not a
///    language.
fn language_of(
    text: Option<&str>,
    model_lang: Option<&'static str>,
) -> (Option<String>, Option<String>) {
    let Some(text) = text else {
        return (None, None);
    };
    if let Some(l) = model_lang {
        return (Some(l.to_string()), Some(lang_via::MODEL.to_string()));
    }
    match lang::classify(text).tag() {
        Some(tag) => (
            Some(tag.to_string()),
            Some(lang_via::CLASSIFIED.to_string()),
        ),
        None => (None, None),
    }
}

/// Inference outside the lock, then the write inside it, logging rather than
/// propagating a model failure: one bad segment must not stop capture.
///
/// Returns the ids of **other** segments this call changed (proximity
/// inheritance labels the previous turn), so the caller can announce them.
pub fn analyse_or_log(
    analyzer: &mut Analyzer,
    store: &std::sync::Mutex<Store>,
    stats: &AnalysisStats,
    segment_id: i64,
    samples: &[f32],
    said: Option<String>,
    now_utc_ns: i64,
) -> Vec<i64> {
    let prepared = match analyzer.prepare_maybe_said(samples, Gate::Full, said) {
        Ok(p) => p,
        Err(e) => {
            warn!(segment_id, "analysis failed: {e:#}");
            return Vec::new();
        }
    };
    let Ok(store) = store.lock() else {
        warn!(segment_id, "store mutex poisoned; analysis discarded");
        return Vec::new();
    };
    match analyzer.commit(&store, segment_id, prepared, now_utc_ns) {
        Ok(mut outcome) => {
            analyzer.after_commit(&store, segment_id, &mut outcome, samples);
            stats.record(&outcome);
            outcome.also_changed
        }
        Err(e) => {
            warn!(segment_id, "storing analysis failed: {e:#}");
            Vec::new()
        }
    }
}

/// `analyse_or_log` for a turn that came off the user's own microphone.
///
/// Same shape, same lock discipline; the only difference is which `commit` runs
/// — and that difference is the whole point, because a mic turn must never fall
/// through to the voicebank.
#[allow(clippy::too_many_arguments)]
pub fn analyse_mic_or_log(
    analyzer: &mut Analyzer,
    store: &std::sync::Mutex<Store>,
    stats: &AnalysisStats,
    segment_id: i64,
    samples: &[f32],
    mic: &MicEnroll<'_>,
    said: Option<String>,
    now_utc_ns: i64,
) -> Vec<i64> {
    let prepared = match analyzer.prepare_maybe_said(samples, Gate::Full, said) {
        Ok(p) => p,
        Err(e) => {
            warn!(segment_id, "analysis failed: {e:#}");
            return Vec::new();
        }
    };
    let Ok(store) = store.lock() else {
        warn!(segment_id, "store mutex poisoned; analysis discarded");
        return Vec::new();
    };
    match analyzer.commit_mic(&store, segment_id, prepared, mic, now_utc_ns) {
        Ok(mut outcome) => {
            analyzer.after_commit(&store, segment_id, &mut outcome, samples);
            stats.record(&outcome);
            outcome.also_changed
        }
        Err(e) => {
            warn!(segment_id, "storing microphone analysis failed: {e:#}");
            Vec::new()
        }
    }
}

/// `analyse_or_log` for a turn whose speaker is known from the wire it arrived
/// on (0.12.1) — a per-user Discord stream.
///
/// Same shape and the same lock discipline as the two above it. The two
/// differences are both consequences of the audio being one person by
/// construction: [`Gate::SingleSpeaker`], so an overlap reading cannot cost the
/// embedding, and [`Analyzer::commit_pinned`], so the voicebank is never asked
/// a question it cannot answer better than the wire already did.
#[allow(clippy::too_many_arguments)]
pub fn analyse_pinned_or_log(
    analyzer: &mut Analyzer,
    store: &std::sync::Mutex<Store>,
    stats: &AnalysisStats,
    segment_id: i64,
    samples: &[f32],
    pin: &PinnedLeg<'_>,
    said: Option<String>,
    now_utc_ns: i64,
) -> Vec<i64> {
    let prepared = match analyzer.prepare_maybe_said(samples, Gate::SingleSpeaker, said) {
        Ok(p) => p,
        Err(e) => {
            warn!(segment_id, "analysis failed: {e:#}");
            return Vec::new();
        }
    };
    let Ok(store) = store.lock() else {
        warn!(segment_id, "store mutex poisoned; analysis discarded");
        return Vec::new();
    };
    match analyzer.commit_pinned(&store, segment_id, prepared, pin, now_utc_ns) {
        Ok(mut outcome) => {
            analyzer.after_commit(&store, segment_id, &mut outcome, samples);
            stats.record(&outcome);
            outcome.also_changed
        }
        Err(e) => {
            warn!(
                segment_id,
                "storing per-user Discord analysis failed: {e:#}"
            );
            Vec::new()
        }
    }
}

// ---- 0.11.0: learned identity ----------------------------------------------

/// A probe and the bank it will be compared against, in the same space.
///
/// Named rather than written out because the pairing is the invariant: a
/// projected probe against raw prototypes is not a worse score, it is a
/// meaningless one.
type SameSpace = (Embedding, Vec<(i64, Embedding)>);

/// The learned space, if one is installed and `[identity].learn` is on.
///
/// Returns the probe and the bank **both** mapped, or `None` when nothing is
/// installed. Returning them as a pair is the point: a projected probe
/// compared against raw prototypes is not a worse score, it is a meaningless
/// one, and the type is what makes forgetting one half impossible.
fn learned_space(
    store: &Store,
    cfg: &IdentityConfig,
    probe: &Embedding,
    bank: &[(i64, Embedding)],
) -> Result<Option<SameSpace>> {
    if !cfg.learn {
        return Ok(None);
    }
    let Some((projection, _, _)) = store.installed_projection()? else {
        return Ok(None);
    };
    if projection.model_id != probe.model_id {
        // A projection is as model-specific as an embedding. A new extractor
        // invalidates the old map rather than reinterpreting it.
        return Ok(None);
    }
    Ok(Some((
        projection.apply(probe)?,
        crate::calib::project_bank(&projection, bank)?,
    )))
}

/// The per-voice label thresholds, or the globals.
///
/// A read failure is not a reason to stop labelling: the table is an
/// improvement on the globals, not a prerequisite for them, so the fallback is
/// the operating point 0.10.2 shipped with.
fn learned_thresholds(store: &Store, cfg: &IdentityConfig) -> crate::calib::Thresholds {
    let globals = crate::calib::Thresholds::global(cfg.label_threshold, 0.0);
    if !cfg.learn {
        return globals;
    }
    match store.threshold_table((cfg.label_threshold, 0.0)) {
        Ok(t) => t,
        Err(e) => {
            warn!("the learned thresholds could not be read: {e:#}");
            globals
        }
    }
}

/// How a voice's several prototypes become one score, as learned (0.12.0).
///
/// Same shape and same reasoning as [`learned_thresholds`]: it is an
/// improvement on the shipped rule, not a prerequisite for it, so an
/// unreadable value costs the improvement and never the label.
fn learned_aggregate(store: &Store, cfg: &IdentityConfig) -> crate::calib::Aggregate {
    if !cfg.learn {
        return crate::calib::Aggregate::Max;
    }
    match store.learned_aggregate() {
        Ok(a) => a,
        Err(e) => {
            warn!("the learned prototype aggregate could not be read: {e:#}");
            crate::calib::Aggregate::Max
        }
    }
}

// ---- end 0.11.0 -----------------------------------------------------------
