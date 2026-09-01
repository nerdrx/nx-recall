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

use crate::asr::{Asr, normalise_words};
use crate::config::{IdentityConfig, SAMPLE_RATE};
use crate::embed::{Embedder, Embedding};
use crate::identity::{self, Decision, Refusal};
use crate::lang::{self, Lang};
use crate::models::{FALLBACK_ASR, ModelSet};
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
    /// **Other** segments this turn's arrival changed — proximity inheritance
    /// labels the turn *before* this one, once this one proves what came after
    /// it. The caller publishes them, so a GUI sees the row change without
    /// re-querying.
    pub also_changed: Vec<i64>,
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
    /// new transcript won. `text` is what the row says now.
    Redecoded { text: String, asr_model_id: String },
    /// The transcript and the speaker's declared language disagree and nothing
    /// in the catalogue can settle it. The words are kept and the row is
    /// marked; `lang` goes to NULL rather than to a guess.
    Marked { read_as: &'static str },
}

pub struct Analyzer {
    overlap: OverlapDetector,
    asr: Asr,
    embedder: Embedder,
    cfg: IdentityConfig,
    /// Where the models live, kept so a constrained decoder can be loaded from
    /// the same root later without re-resolving the config.
    models: ModelSet,
    /// The English-only export, loaded the first time a wrong-language decode
    /// needs it and resident from then on. `None` means "not tried yet";
    /// `en_unavailable` means "tried, not installed" — the difference is what
    /// keeps the warning to one line rather than one per segment.
    en_asr: Option<Asr>,
    en_unavailable: bool,
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
            overlap: OverlapDetector::load(&models.segmentation)?,
            asr: Asr::load(models)?,
            embedder: Embedder::load(models)?,
            cfg: cfg.clone(),
            models: models.clone(),
            en_asr: None,
            en_unavailable: false,
        })
    }

    pub fn embed_model_id(&self) -> &str {
        self.embedder.model_id()
    }

    /// All the inference for one turn. Touches no database.
    pub fn prepare(&mut self, samples: &[f32]) -> Result<Prepared> {
        let duration_s = samples.len() as f32 / SAMPLE_RATE as f32;
        let overlap_frac = self.overlap.overlap_frac(samples)?;

        let raw = self.asr.transcribe(samples);
        // An empty transcript is stored as NULL rather than "": it keeps the
        // full-text index free of empty documents and makes "has a transcript"
        // a single IS NOT NULL.
        let text = (!normalise_words(&raw).is_empty()).then_some(raw);

        let refusal = identity::gate(&self.cfg, overlap_frac, duration_s);
        let embedding = match refusal {
            Some(_) => None,
            None => Some(self.embedder.embed(samples, SAMPLE_RATE)?),
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
                    overlap_frac,
                    text,
                    decision: Decision::Refused(refusal),
                    speaker_id: None,
                    match_score: None,
                    enrolled: false,
                    golden: false,
                    language_fix: None,
                    also_changed: Vec::new(),
                });
            }
        };

        store.store_embedding(segment_id, &embedding)?;
        let bank = store.prototypes(&embedding.model_id)?;
        let ranked = identity::rank(&embedding, &bank)?;
        // The word count is the mint bar's second half (0.6.1): a new identity
        // needs seconds *and* words. Matching an existing one never asks.
        let words = text.as_deref().map(lang::word_count).unwrap_or(0);
        let decision = identity::decide(&self.cfg, overlap_frac, duration_s, words, &ranked);

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
            also_changed: Vec::new(),
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
        // `label_via = "mic"` — so nothing downstream has to infer it from a
        // NULL score, which is a thing three other paths also produce.
        store.set_segment_speaker_via(
            segment_id,
            Some(mic.speaker_id),
            None,
            Some(crate::store::label_via::MIC),
        )?;

        let mut enrolled = false;
        let mut golden = false;
        if let Some(embedding) = embedding {
            store.store_embedding(segment_id, &embedding)?;
            if overlap_frac <= self.cfg.enroll_max_overlap
                && duration_s >= self.cfg.enroll_min_duration_s
            {
                // `enrolled` follows what the store actually did. Every slot
                // being golden means the vector is dropped — hand-enrolled
                // audio outranks anything inferred — and reporting that as an
                // enrolment made `mic_enrolled` count turns that added nothing
                // to the bank (audit finding #25).
                enrolled = store
                    .add_prototype(
                        mic.speaker_id,
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
                golden = keep_golden(store, mic, segment_id, duration_s)?;
            }
        } else {
            debug!(
                segment_id,
                overlap_frac,
                duration_s,
                "microphone: labelled but not enrolled ({})",
                refusal.unwrap_or(Refusal::TooShort).as_str()
            );
        }

        Ok(Outcome {
            overlap_frac,
            text,
            decision: Decision::Pinned {
                speaker_id: mic.speaker_id,
            },
            speaker_id: Some(mic.speaker_id),
            match_score: None,
            enrolled,
            golden,
            language_fix: None,
            also_changed: Vec::new(),
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
        if let Some(speaker_id) = outcome.speaker_id {
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

    /// The English-only decoder, loaded on demand and kept.
    ///
    /// It is not part of the default model set any more (DESIGN §4 — the
    /// multilingual export beats it at English too), so the honest answer here
    /// is often "not installed", and that is said once rather than per segment.
    fn english(&mut self) -> Option<&mut Asr> {
        if self.en_asr.is_none() && !self.en_unavailable {
            if !self.models.has_asr_export(&FALLBACK_ASR) {
                self.en_unavailable = true;
                warn!(
                    "a transcript disagrees with its speaker's declared language, but the \
                     English-only export is not installed under {} — nothing to re-decode with. \
                     `recalld models fetch --fallback-asr` installs it ({}).",
                    self.models.root.display(),
                    FALLBACK_ASR.note
                );
            } else {
                match Asr::load(&self.models.with_asr(&FALLBACK_ASR)) {
                    Ok(asr) => {
                        info!(
                            model = asr.model_id(),
                            "loaded the English-only decoder for wrong-language correction"
                        );
                        self.en_asr = Some(asr);
                    }
                    Err(e) => {
                        self.en_unavailable = true;
                        warn!("could not load the English-only decoder: {e:#}");
                    }
                }
            }
        }
        self.en_asr.as_mut()
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
    /// The two directions are not symmetric, and pretending otherwise would be
    /// the bug here:
    ///
    /// * **English speaker, German-looking transcript** → decode the audio
    ///   again with the English-only export, whose language is a hard property
    ///   of the model rather than a hint. The result replaces the text *only*
    ///   if it is non-empty and reads as English; `asr_model_id` moves with it,
    ///   because the row must say which model produced the words it holds.
    /// * **German speaker, English-looking transcript** → there is no German
    ///   constrained decoder in the catalogue, so there is nothing to re-decode
    ///   *with*. The words are kept (they are the only record of what was said)
    ///   and the row is marked; `lang` goes to NULL, because the classifier and
    ///   the declaration cannot both be right and this daemon cannot tell which
    ///   is wrong. Identity is untouched: the label came from the voice, and a
    ///   voice does not become less recognisable by switching language.
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

        if want == "en" {
            // Already decoding under the English constraint: the text is what
            // this model says, and running it twice would say it again.
            let already_english = self.asr.lang() == Some("en");
            if !already_english && let Some(asr) = self.english() {
                let model_id = asr.model_id().to_string();
                let raw = asr.transcribe(samples);
                let redecoded = lang::classify(&raw);
                if redecoded == Lang::En && !normalise_words(&raw).is_empty() {
                    store.set_segment_text_from_redecode(segment_id, &raw, "en", &model_id)?;
                    info!(
                        segment_id,
                        speaker = speaker_id,
                        model = %model_id,
                        "re-decoded a transcript that read as German for an English-only voice"
                    );
                    return Ok(Some(LanguageFix::Redecoded {
                        text: raw,
                        asr_model_id: model_id,
                    }));
                }
                debug!(
                    segment_id,
                    speaker = speaker_id,
                    reads_as = redecoded.as_str(),
                    "the English re-decode did not come back as English; keeping the original"
                );
            }
        }

        // Unresolvable in either direction: mark it and keep the words.
        store.mark_segment_language_mismatch(segment_id)?;
        debug!(
            segment_id,
            speaker = speaker_id,
            declared = want,
            reads_as = got,
            "transcript disagrees with the speaker's declared language; marked, not changed"
        );
        Ok(Some(LanguageFix::Marked { read_as: got }))
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
    /// Turns that took their name from the turns around them.
    pub proximity_labelled: std::sync::atomic::AtomicU64,
    /// Transcripts re-decoded under a language constraint.
    pub redecoded: std::sync::atomic::AtomicU64,
    /// Transcripts that disagree with their speaker's declared language and
    /// could not be corrected.
    pub lang_mismatch: std::sync::atomic::AtomicU64,
}

impl AnalysisStats {
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
            Some(LanguageFix::Redecoded { .. }) => {
                self.redecoded.fetch_add(1, Ordering::Relaxed);
            }
            Some(LanguageFix::Marked { .. }) => {
                self.lang_mismatch.fetch_add(1, Ordering::Relaxed);
            }
            None => {}
        }
        self.proximity_labelled
            .fetch_add(outcome.also_changed.len() as u64, Ordering::Relaxed);
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
    now_utc_ns: i64,
) -> Vec<i64> {
    let prepared = match analyzer.prepare(samples) {
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
pub fn analyse_mic_or_log(
    analyzer: &mut Analyzer,
    store: &std::sync::Mutex<Store>,
    stats: &AnalysisStats,
    segment_id: i64,
    samples: &[f32],
    mic: &MicEnroll<'_>,
    now_utc_ns: i64,
) -> Vec<i64> {
    let prepared = match analyzer.prepare(samples) {
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
