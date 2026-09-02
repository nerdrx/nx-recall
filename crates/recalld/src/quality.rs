//! The accuracy round's idle worker (0.8.0): two passes over stored turns.
//!
//! Both of them are things that cannot be done live and are worth doing late.
//!
//! ## 1. Context re-decode — the biggest measured gain in the round
//!
//! A short turn decoded alone is decoded badly. §11 of `spike/FINDINGS.md`
//! found the cliff and every model in the bake-off fell off it: 1.5 s slices
//! score 85-90% "WER" against the same audio's full-sentence decode at 5%. The
//! decoder has no way to know that "wurde" is not "wurde ich" when the two
//! seconds either side of the word are missing.
//!
//! So: decode the turn again inside a window of the session's own audio, and
//! keep the words whose token timestamps land inside the turn. Measured
//! (`spike/context_redecode_bench.py`, FLEURS German + LibriSpeech through
//! Opus 24k, 125 turns cut at word boundaries):
//!
//! | turn | decoded alone | decoded in a ±3 s window | relative |
//! |---|---:|---:|---:|
//! | 1.5 s | 56.7% WER | 20.4% WER | **64%** |
//! | 2.5 s | 34.3% WER | 17.1% WER | **50%** |
//!
//! ### The window is not a recording, and that is a real limitation
//!
//! There is no continuous capture: the daemon stores one WAV per turn and the
//! silences between them are not on disk. The window is therefore rebuilt by
//! butting the neighbouring clips together with their real silences restored as
//! zeros — and where the silence is longer than `[asr].context_max_gap_s` the
//! window is **truncated at the gap** rather than spanning it. Past a second of
//! silence the audio on the other side is different speech, and a window that
//! spans it would hand the decoder a false continuity. The consequence to be
//! honest about: a turn in the middle of a long pause gets no context at all
//! and is re-decoded exactly as it was decoded live, which is why the worker
//! counts `redecode_skipped_no_audio`.
//!
//! ## 2. Confidence — a second opinion, never a second transcript
//!
//! Canary 180m (`crate::canary`) decodes the same clip and the two transcripts
//! are compared word by word. At or above `[asr].confidence_tau` the row is
//! `solid`, below it `shaky`. Measured (`spike/confidence_bench.py`): shaky
//! turns carry 4.2× the word error of solid ones. **The text is never
//! replaced** — canary returns nothing at all on 11% of real turns, which
//! disqualifies it as a transcriber and costs nothing as a witness.
//!
//! ## The lock discipline, which is not negotiable
//!
//! Gather under the store lock, decode without it, commit under it again. The
//! rule is [`crate::enrich`]'s and it was written in blood on 2026-09-02: a
//! worker that held the store mutex across model calls blocked the capture
//! pipeline's segment inserts, the queue overflowed, and the evening has 128
//! dropped-buffer warnings to show for it. **Model time and store-lock time
//! never overlap.**

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::asr::{TimedAsr, Word, normalise_words, words_in_span};
use crate::bus::Bus;
use crate::canary::{Canary, Confidence, agreement};
use crate::clock::utc_now_ns;
use crate::config::{AsrConfig, SAMPLE_RATE};
use crate::control::Control;
use crate::models::{ConfidenceModel, ModelSet};
use crate::store::{Clip, RedecodeCandidate, Store};

/// Why the worker may not run right now, or `None` for "go ahead".
///
/// Deliberately the same two rules as [`crate::enrich::gate`], and deliberately
/// its own function rather than a call into it: this worker has its own queue
/// ceiling, and a shared gate would tie two features' budgets together for no
/// better reason than that they were written in the same month.
pub fn gate(control: &Control, cfg: &AsrConfig) -> Option<String> {
    if control.is_paused() {
        return Some("capture is paused — nothing is written down, including this".to_string());
    }
    let queued = control
        .queue
        .as_ref()
        .map(|q| (q.queued_samples() as f64 / SAMPLE_RATE as f64).round() as i64)
        .unwrap_or(0);
    if queued > cfg.max_queue_seconds {
        return Some(format!(
            "{queued}s of audio is still waiting to be transcribed — capture comes first"
        ));
    }
    None
}

#[derive(Default)]
pub struct QualityStop(AtomicBool);

impl QualityStop {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// the window
// ---------------------------------------------------------------------------

/// One piece of a rebuilt window: where it sits on the session's timeline and
/// where in the window's own samples it landed.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub samples: Vec<f32>,
    /// The UTC nanosecond the window's first sample corresponds to.
    pub start_ns: i64,
    /// How many clips went into it, the turn included.
    pub clips: usize,
}

impl Window {
    /// Seconds from the window's start to a moment on the session timeline.
    pub fn offset_s(&self, t_ns: i64) -> f32 {
        (t_ns - self.start_ns) as f32 / 1e9
    }
}

/// Rebuild the audio around a turn out of the clips stored either side of it.
///
/// `clips` must be time-ordered and must contain the turn itself. Gaps up to
/// `max_gap_s` are restored as silence so the window's timeline matches the
/// session's; a longer gap **ends** the window on that side, because the audio
/// beyond it is not this turn's context.
///
/// Pure, and takes the samples as a loader so the test suite can drive it
/// without a filesystem: the gap arithmetic is the part that is easy to get
/// wrong and it is the part worth a test.
pub fn build_window(
    clips: &[Clip],
    turn_id: i64,
    max_gap_s: f32,
    mut load: impl FnMut(&Clip) -> Option<Vec<f32>>,
) -> Option<Window> {
    let here = clips.iter().position(|c| c.id == turn_id)?;
    let max_gap_ns = (max_gap_s.max(0.0) as f64 * 1e9) as i64;

    // The turn itself first, then outwards in both directions, stopping at the
    // first gap too wide to be a pause in the same conversation.
    let mut kept: Vec<(Clip, Vec<f32>)> = Vec::new();
    let turn = load(&clips[here])?;
    kept.push((clips[here].clone(), turn));

    let mut left = here;
    while left > 0 {
        let prev = &clips[left - 1];
        if kept[0].0.t_start_ns - prev.t_end_ns > max_gap_ns {
            break;
        }
        match load(prev) {
            Some(samples) => kept.insert(0, (prev.clone(), samples)),
            None => break,
        }
        left -= 1;
    }
    let mut right = here;
    while right + 1 < clips.len() {
        let next = &clips[right + 1];
        if next.t_start_ns - kept[kept.len() - 1].0.t_end_ns > max_gap_ns {
            break;
        }
        match load(next) {
            Some(samples) => kept.push((next.clone(), samples)),
            None => break,
        }
        right += 1;
    }

    let start_ns = kept[0].0.t_start_ns;
    let mut samples: Vec<f32> = Vec::new();
    let mut cursor_ns = start_ns;
    for (clip, clip_samples) in &kept {
        // The clip's own start, in samples from the window's start. Silence is
        // written for whatever is missing in front of it, so a word's position
        // in the window is its position in the evening.
        let want = (((clip.t_start_ns - start_ns) as f64 / 1e9) * SAMPLE_RATE as f64) as i64;
        let have = samples.len() as i64;
        if want > have {
            samples.extend(std::iter::repeat_n(0.0, (want - have) as usize));
        }
        samples.extend_from_slice(clip_samples);
        cursor_ns = clip.t_end_ns;
    }
    let _ = cursor_ns;
    Some(Window {
        samples,
        start_ns,
        clips: kept.len(),
    })
}

// ---------------------------------------------------------------------------
// the two decisions, each without a store lock in scope
// ---------------------------------------------------------------------------

/// What a context re-decode concluded about one turn.
#[derive(Debug, Clone, PartialEq)]
pub enum Redecode {
    /// These words replace the row's.
    Replaced { text: String, model_id: String },
    /// The window decode produced nothing usable inside the turn's span, or
    /// nothing that differs from what is already there. The row is marked
    /// considered and its words stand.
    Kept,
    /// Not enough audio around the turn to be worth a pass — there was no
    /// neighbouring clip inside the gap limit, so the "window" would have been
    /// the turn itself.
    NoContext,
}

/// Decide what a window decode says about the turn inside it.
///
/// Pure, so the guards are testable without a model: the words of the window
/// that start inside the turn win, provided there are any and provided they say
/// something different from what is stored.
pub fn judge_redecode(
    words: &[Word],
    window: &Window,
    turn_start_ns: i64,
    turn_end_ns: i64,
    current: &str,
    model_id: &str,
) -> Redecode {
    let from = window.offset_s(turn_start_ns);
    let to = window.offset_s(turn_end_ns);
    let text = words_in_span(words, from, to);
    if normalise_words(&text).is_empty() {
        // A re-decode that found no words in the turn's own span has not
        // discovered silence — it has failed to line up. The stored words are
        // the only record of what was said and they stay.
        return Redecode::Kept;
    }
    if normalise_words(&text) == normalise_words(current) {
        return Redecode::Kept;
    }
    Redecode::Replaced {
        text,
        model_id: model_id.to_string(),
    }
}

/// What the cross-check concluded, and the agreement it concluded it from.
#[derive(Debug, Clone, PartialEq)]
pub struct CrossCheck {
    pub confidence: Option<Confidence>,
    pub agreement: f32,
    /// The source language the winning opinion was decoded under, for the log.
    pub lang: Option<&'static str>,
}

/// Compare a stored transcript with the cross-check decoder's, over one or both
/// source languages.
///
/// With a language in hand only that one is run. Without one, both are, and the
/// **higher** agreement wins — a decoder asked for the wrong language disagrees
/// for a reason that has nothing to do with whether the first decoder was
/// right, so taking the worse of the two would flag the language, not the
/// words. It is measurably weaker (4.2× → 3.0×) and it is still a signal.
pub fn cross_check(
    canaries: &mut [Canary],
    samples: &[f32],
    current: &str,
    lang: Option<&str>,
    tau: f32,
) -> CrossCheck {
    let mut best = CrossCheck {
        confidence: None,
        agreement: 0.0,
        lang: None,
    };
    for canary in canaries.iter_mut() {
        if let Some(want) = lang
            && canary.lang() != want
        {
            continue;
        }
        let other = canary.transcribe(samples);
        if normalise_words(&other).is_empty() {
            // Canary returned nothing. That is its own failure mode (§11: 11%
            // of real turns) and says nothing about the words it was checking,
            // so the row is marked checked with no verdict rather than shaky.
            continue;
        }
        let score = agreement(current, &other);
        if best.confidence.is_none() || score > best.agreement {
            best = CrossCheck {
                confidence: Some(Confidence::from_agreement(score, tau)),
                agreement: score,
                lang: Some(canary.lang()),
            };
        }
    }
    best
}

// ---------------------------------------------------------------------------
// the worker
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Phase {
    /// Both passes are switched off.
    #[default]
    Off,
    /// On, but nothing it needs is installed.
    Unavailable,
    /// On and installed, but a gate is closed.
    Blocked,
    /// Nothing left to do.
    Idle,
    Running,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Off => "off",
            Phase::Unavailable => "unavailable",
            Phase::Blocked => "blocked",
            Phase::Idle => "idle",
            Phase::Running => "running",
        }
    }
}

/// The worker's counters, read by `status` and pushed with it.
#[derive(Debug, Default)]
pub struct QualityStats {
    pub redecoded_context: std::sync::atomic::AtomicU64,
    pub redecode_skipped_no_audio: std::sync::atomic::AtomicU64,
    pub confidence_solid: std::sync::atomic::AtomicU64,
    pub confidence_shaky: std::sync::atomic::AtomicU64,
}

impl QualityStats {
    pub fn to_json(&self) -> Value {
        json!({
            "redecoded_context": self.redecoded_context.load(Ordering::Relaxed),
            "redecode_skipped_no_audio": self.redecode_skipped_no_audio.load(Ordering::Relaxed),
            "confidence_solid": self.confidence_solid.load(Ordering::Relaxed),
            "confidence_shaky": self.confidence_shaky.load(Ordering::Relaxed),
        })
    }
}

/// One pass over a batch of segments needing a context re-decode.
///
/// Returns whether there was any work. Every model call in here happens with no
/// store guard in scope; the three phases are gather, decode, commit.
///
/// Public so a test can run exactly one pass against a real model instead of
/// starting the thread and waiting for it: a background loop that can only be
/// observed by sleeping is a background loop nobody tests.
#[allow(clippy::too_many_arguments)]
pub fn redecode_batch(
    store: &Arc<std::sync::Mutex<Store>>,
    control: &Arc<Control>,
    bus: &Bus,
    asr: &mut TimedAsr,
    cfg: &AsrConfig,
    data_dir: &Path,
    stats: &QualityStats,
    stop: &QualityStop,
) -> Result<bool> {
    let candidates: Vec<RedecodeCandidate> = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        guard.segments_for_context_redecode(cfg.context_redecode_below_s, cfg.batch_segments)?
    };
    if candidates.is_empty() {
        return Ok(false);
    }
    let pad_ns = (cfg.context_pad_s.max(0.0) as f64 * 1e9) as i64;

    for candidate in candidates {
        if stop.stopped() || gate(control, cfg).is_some() {
            break;
        }
        // ---- gather (lock held, no model) ----
        let clips = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            guard.session_clips_between(
                candidate.session_id,
                candidate.t_start_ns - pad_ns,
                candidate.t_end_ns + pad_ns,
            )?
        };
        let at = utc_now_ns();

        // ---- decode (no lock) ----
        let dir = data_dir.to_path_buf();
        let window = build_window(&clips, candidate.id, cfg.context_max_gap_s, |clip| {
            read_clip(&dir, &clip.audio_path)
        });
        let outcome = match window {
            None => Redecode::NoContext,
            Some(window) if window.clips < 2 => {
                let _ = window;
                Redecode::NoContext
            }
            Some(window) => {
                let (_, words) = asr.transcribe_timed(&window.samples);
                judge_redecode(
                    &words,
                    &window,
                    candidate.t_start_ns,
                    candidate.t_end_ns,
                    candidate.text.as_deref().unwrap_or(""),
                    asr.model_id(),
                )
            }
        };

        // ---- commit (lock held, no model) ----
        let changed = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            match &outcome {
                Redecode::Replaced { text, model_id } => {
                    guard.set_segment_text_from_context(candidate.id, text, model_id, at)?;
                    stats.redecoded_context.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Redecode::Kept => {
                    guard.mark_redecode_considered(candidate.id, at)?;
                    false
                }
                Redecode::NoContext => {
                    guard.mark_redecode_considered(candidate.id, at)?;
                    stats
                        .redecode_skipped_no_audio
                        .fetch_add(1, Ordering::Relaxed);
                    false
                }
            }
        };
        if changed {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            crate::pipeline::publish_segment(bus, &guard, candidate.id);
        }
    }
    Ok(true)
}

/// One pass over a batch of segments waiting for a cross-check. Public for the
/// same reason [`redecode_batch`] is.
#[allow(clippy::too_many_arguments)]
pub fn confidence_batch(
    store: &Arc<std::sync::Mutex<Store>>,
    control: &Arc<Control>,
    bus: &Bus,
    canaries: &mut [Canary],
    cfg: &AsrConfig,
    data_dir: &Path,
    stats: &QualityStats,
    stop: &QualityStop,
) -> Result<bool> {
    let candidates: Vec<RedecodeCandidate> = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        guard.segments_for_confidence(cfg.batch_segments)?
    };
    if candidates.is_empty() {
        return Ok(false);
    }

    for candidate in candidates {
        if stop.stopped() || gate(control, cfg).is_some() {
            break;
        }
        // ---- gather ----
        let (lang, _thread) = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            guard.segment_lang_hint(candidate.id)?
        };
        let at = utc_now_ns();
        let Some(samples) = read_clip(data_dir, &candidate.audio_path) else {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            guard.set_segment_confidence(candidate.id, None, at)?;
            continue;
        };

        // ---- decode (no lock) ----
        let current = candidate.text.clone().unwrap_or_default();
        let checked = cross_check(
            canaries,
            &samples,
            &current,
            lang.as_deref(),
            cfg.confidence_tau,
        );

        // ---- commit ----
        {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            guard.set_segment_confidence(
                candidate.id,
                checked.confidence.map(Confidence::as_str),
                at,
            )?;
        }
        match checked.confidence {
            Some(Confidence::Solid) => {
                stats.confidence_solid.fetch_add(1, Ordering::Relaxed);
            }
            Some(Confidence::Shaky) => {
                stats.confidence_shaky.fetch_add(1, Ordering::Relaxed);
            }
            None => continue,
        }
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        crate::pipeline::publish_segment(bus, &guard, candidate.id);
    }
    Ok(true)
}

/// Read one stored clip, or `None` if retention has already taken it.
fn read_clip(data_dir: &Path, relative: &str) -> Option<Vec<f32>> {
    if relative.is_empty() {
        return None;
    }
    crate::ingest::read_wav(&data_dir.join(relative)).ok()
}

/// The background thread. Started whether or not either pass is on: both
/// switches are live, so something has to be watching them.
#[allow(clippy::too_many_arguments)]
pub fn run(
    store: Arc<std::sync::Mutex<Store>>,
    control: Arc<Control>,
    bus: Arc<Bus>,
    models: Option<ModelSet>,
    models_root: Option<PathBuf>,
    data_dir: PathBuf,
    runtime: crate::config::RuntimeConfig,
    stats: Arc<QualityStats>,
    stop: Arc<QualityStop>,
) {
    // The project's rule, applied to this thread too: analysis never wins a
    // scheduling contest against a VR frame. Same nice value and same pinned
    // cores as the inference thread — this worker runs the same models.
    crate::pipeline::deprioritise_current_thread(runtime.inference_nice, &runtime.inference_cpus);
    // Both decoders are loaded on demand and dropped the moment there is
    // nothing to do: they are a second copy of an encoder in memory, worth
    // paying for while there is a backlog and not worth paying for overnight.
    let mut asr: Option<TimedAsr> = None;
    let mut canaries: Vec<Canary> = Vec::new();
    let mut said_unavailable = false;

    loop {
        if stop.stopped() {
            debug!("the transcript quality worker stopped");
            return;
        }
        let cfg = control.asr();
        let mut worked = false;

        if (cfg.context_redecode || cfg.confidence)
            && let Some(reason) = gate(&control, &cfg)
        {
            debug!("quality worker standing down: {reason}");
        } else {
            if cfg.context_redecode
                && let Some(models) = models.as_ref()
            {
                if asr.is_none() {
                    match TimedAsr::load(models) {
                        Ok(loaded) => asr = Some(loaded),
                        Err(e) => warn!("the re-decode worker could not load the ASR model: {e:#}"),
                    }
                }
                if let Some(asr) = asr.as_mut() {
                    match redecode_batch(
                        &store, &control, &bus, asr, &cfg, &data_dir, &stats, &stop,
                    ) {
                        Ok(did) => worked |= did,
                        Err(e) => warn!("a context re-decode batch failed: {e:#}"),
                    }
                }
            }
            if cfg.confidence
                && let Some(root) = models_root.as_ref()
            {
                let model = ConfidenceModel::resolve_at(root.clone(), 1);
                if !model.present() {
                    if !said_unavailable {
                        info!("{}", ConfidenceModel::how_to_get_it());
                        said_unavailable = true;
                    }
                } else {
                    if canaries.is_empty() {
                        for lang in crate::canary::LANGS {
                            match Canary::load(&model, lang) {
                                Ok(c) => canaries.push(c),
                                Err(e) => warn!("could not load the {lang} cross-check: {e:#}"),
                            }
                        }
                    }
                    if !canaries.is_empty() {
                        match confidence_batch(
                            &store,
                            &control,
                            &bus,
                            &mut canaries,
                            &cfg,
                            &data_dir,
                            &stats,
                            &stop,
                        ) {
                            Ok(did) => worked |= did,
                            Err(e) => warn!("a confidence batch failed: {e:#}"),
                        }
                    }
                }
            }
        }

        if !worked {
            // Nothing to do: give the memory back until there is.
            asr = None;
            canaries.clear();
        }

        let pause = Duration::from_secs(cfg.batch_pause_s.max(1));
        let step = Duration::from_millis(200);
        let mut slept = Duration::ZERO;
        while slept < pause {
            if stop.stopped() {
                break;
            }
            std::thread::sleep(step);
            slept += step;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;

    fn clip(id: i64, start_s: i64, end_s: i64) -> Clip {
        Clip {
            id,
            t_start_ns: start_s * S,
            t_end_ns: end_s * S,
            audio_path: format!("seg-{id}.wav"),
        }
    }

    fn tone(seconds: i64, value: f32) -> Vec<f32> {
        vec![value; (seconds * SAMPLE_RATE as i64) as usize]
    }

    #[test]
    fn a_window_restores_the_silence_between_two_clips() {
        // 0-1 s, then 2-3 s: one second of silence in between, which the window
        // has to contain or every timestamp after it is a second early.
        let clips = vec![clip(1, 0, 1), clip(2, 2, 3)];
        let window = build_window(&clips, 2, 1.0, |c| Some(tone(1, c.id as f32))).unwrap();
        assert_eq!(window.clips, 2);
        assert_eq!(window.start_ns, 0);
        assert_eq!(window.samples.len(), 3 * SAMPLE_RATE as usize);
        assert_eq!(window.samples[SAMPLE_RATE as usize + 100], 0.0);
        assert_eq!(window.samples[2 * SAMPLE_RATE as usize + 100], 2.0);
        // The turn starts two seconds into the window, and that is what the
        // span selection will be given.
        assert!((window.offset_s(2 * S) - 2.0).abs() < 1e-6);
    }

    #[test]
    fn a_gap_wider_than_the_limit_truncates_the_window() {
        // The clip before the turn is five seconds earlier. Whatever was said
        // then is not this turn's context, and splicing it on would hand the
        // decoder a continuity that never existed.
        let clips = vec![clip(1, 0, 1), clip(2, 6, 7), clip(3, 7, 8)];
        let window = build_window(&clips, 2, 1.0, |c| Some(tone(1, c.id as f32))).unwrap();
        assert_eq!(window.clips, 2, "the far clip must not be spliced on");
        assert_eq!(window.start_ns, 6 * S);
    }

    #[test]
    fn a_turn_alone_in_its_session_yields_a_one_clip_window() {
        let clips = vec![clip(7, 0, 1)];
        let window = build_window(&clips, 7, 1.0, |c| Some(tone(1, c.id as f32))).unwrap();
        assert_eq!(window.clips, 1, "the caller treats this as no context");
    }

    #[test]
    fn a_clip_retention_has_taken_stops_the_window_rather_than_shifting_it() {
        let clips = vec![clip(1, 0, 1), clip(2, 1, 2), clip(3, 2, 3)];
        let window = build_window(&clips, 2, 1.0, |c| {
            (c.id != 1).then(|| tone(1, c.id as f32))
        })
        .unwrap();
        assert_eq!(window.clips, 2);
        assert_eq!(window.start_ns, S, "the window starts at the turn");
    }

    #[test]
    fn a_turn_whose_own_audio_is_gone_has_no_window_at_all() {
        let clips = vec![clip(1, 0, 1), clip(2, 1, 2)];
        assert!(build_window(&clips, 2, 1.0, |c| (c.id == 1).then(|| tone(1, 1.0))).is_none());
    }

    fn word(text: &str, at: f32) -> Word {
        Word {
            text: text.into(),
            start_s: at,
        }
    }

    fn window_at(start_ns: i64) -> Window {
        Window {
            samples: Vec::new(),
            start_ns,
            clips: 3,
        }
    }

    #[test]
    fn only_the_words_inside_the_turn_are_kept() {
        // The window starts a second before the turn, which runs 1.0-2.5 s into
        // it. "vorher" and "danach" belong to the neighbours.
        let words = [
            word("vorher", 0.4),
            word("das", 1.1),
            word("war", 1.6),
            word("gut", 2.2),
            word("danach", 2.8),
        ];
        let out = judge_redecode(
            &words,
            &window_at(0),
            1_000_000_000,
            2_500_000_000,
            "das war",
            "model-x",
        );
        assert_eq!(
            out,
            Redecode::Replaced {
                text: "das war gut".into(),
                model_id: "model-x".into()
            }
        );
    }

    #[test]
    fn a_re_decode_that_agrees_with_the_row_changes_nothing() {
        let words = [word("das", 1.1), word("war", 1.6)];
        let out = judge_redecode(
            &words,
            &window_at(0),
            1_000_000_000,
            2_500_000_000,
            "Das war.",
            "model-x",
        );
        assert_eq!(out, Redecode::Kept, "punctuation is not a new transcript");
    }

    #[test]
    fn a_window_decode_with_nothing_in_the_span_keeps_the_stored_words() {
        // Everything the decoder found sits outside the turn. That is a failure
        // to line up, not a discovery of silence, and the stored words are the
        // only record of what was said.
        let words = [word("vorher", 0.2), word("danach", 3.4)];
        let out = judge_redecode(
            &words,
            &window_at(0),
            1_000_000_000,
            2_500_000_000,
            "das war",
            "model-x",
        );
        assert_eq!(out, Redecode::Kept);
    }

    #[test]
    fn the_span_is_measured_from_the_window_not_from_the_session() {
        // A window that starts at 100 s: a turn at 101-102 s is at 1.0-2.0 s in
        // the decoder's own numbers, and reading the session clock straight into
        // the span would keep nothing at all.
        let w = window_at(100 * S);
        assert!((w.offset_s(101 * S) - 1.0).abs() < 1e-6);
        let words = [word("hier", 1.2)];
        assert_eq!(
            judge_redecode(&words, &w, 101 * S, 102 * S, "", "m"),
            Redecode::Replaced {
                text: "hier".into(),
                model_id: "m".into()
            }
        );
    }

    #[test]
    fn the_gate_stands_down_for_a_pause() {
        // Pause means nothing is written down, and a background pass rewriting
        // transcripts through a pause would make that sentence false.
        let control = Control::new(
            PathBuf::from("/nonexistent"),
            None,
            &crate::allowlist::Allowlist::from_rules([("VRChat.exe", true)]),
        );
        let cfg = AsrConfig::default();
        assert!(gate(&control, &cfg).is_none());
        control.pause();
        assert!(gate(&control, &cfg).unwrap().contains("paused"));
        control.resume();
        assert!(gate(&control, &cfg).is_none());
    }
}
