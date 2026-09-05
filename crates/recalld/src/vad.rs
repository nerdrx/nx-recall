//! Silero voice-activity detection and speech segmentation.
//!
//! The segmenter is a pure state machine over per-frame probabilities so it can
//! be tested without the model; `SileroVad` is the thin ONNX Runtime wrapper
//! that produces those probabilities.

use anyhow::{Context, Result, bail};
use ort::session::Session;
use ort::value::Tensor;

/// Silero's 16 kHz path is fixed at 512-sample (32 ms) frames. It is not a
/// tunable: the model was exported with this window baked in.
pub const FRAME_SAMPLES: usize = 512;

/// Which state contract the loaded ONNX export uses.
///
/// Silero shipped two incompatible signatures and the ecosystem has both:
///
/// * `HC` (v4-era 16 kHz export): inputs `x [1,512]`, `h [2,1,64]`, `c [2,1,64]`;
///   outputs `prob [1,1]`, `new_h`, `new_c`. No sample-rate input — the rate is
///   fixed by the export.
/// * `Combined` (v5): inputs `input [1,512]`, `state [2,1,128]`, `sr` (int64
///   scalar); outputs `output [1,1]`, `stateN`.
///
/// The model bundled with this crate is the `HC` export (verified against the
/// Step 0 spike, which is where its numbers came from). We detect the contract
/// from the session's input names rather than assuming, so swapping in a v5
/// file later needs no code change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Contract {
    Hc,
    Combined,
}

/// Per-source recurrent state. One instance per capture session: the LSTM
/// carries context across frames, so interleaving two sources through one state
/// would let one app's audio bias the other's decisions.
#[derive(Debug, Clone)]
pub struct VadState {
    a: Vec<f32>,
    b: Vec<f32>,
}

impl VadState {
    fn hc() -> Self {
        Self {
            a: vec![0.0; 2 * 64],
            b: vec![0.0; 2 * 64],
        }
    }

    fn combined() -> Self {
        Self {
            a: vec![0.0; 2 * 128],
            b: Vec::new(),
        }
    }
}

pub struct SileroVad {
    session: Session,
    contract: Contract,
}

impl SileroVad {
    pub fn from_bytes(model: &[u8]) -> Result<Self> {
        // One thread each: the model is tiny and the whole point is to stay out
        // of the way of the game. ORT's default pools would spin up a thread
        // per core for no measurable gain.
        // `ort`'s builder errors carry the builder itself, so they are neither
        // Send nor Sync and cannot go into anyhow directly.
        let ort_err = |what: &'static str| move |e: ort::Error<_>| anyhow::anyhow!("{what}: {e}");
        let session = Session::builder()
            .map_err(|e| anyhow::anyhow!("creating ONNX session builder: {e}"))?
            .with_intra_threads(1)
            .map_err(ort_err("configuring intra-op threads"))?
            .with_inter_threads(1)
            .map_err(ort_err("configuring inter-op threads"))?
            .commit_from_memory(model)
            .context("loading the Silero VAD model")?;

        let names: Vec<&str> = session.inputs().iter().map(|i| i.name()).collect();
        let contract = if names.contains(&"state") {
            Contract::Combined
        } else if names.contains(&"h") && names.contains(&"c") {
            Contract::Hc
        } else {
            bail!("unrecognised Silero VAD export; inputs are {names:?}");
        };

        Ok(Self { session, contract })
    }

    pub fn new_state(&self) -> VadState {
        match self.contract {
            Contract::Hc => VadState::hc(),
            Contract::Combined => VadState::combined(),
        }
    }

    /// Speech probability for one 512-sample frame, advancing `state`.
    pub fn frame(&mut self, samples: &[f32], state: &mut VadState) -> Result<f32> {
        if samples.len() != FRAME_SAMPLES {
            bail!(
                "Silero expects {FRAME_SAMPLES}-sample frames, got {}",
                samples.len()
            );
        }
        let x = Tensor::from_array((vec![1i64, FRAME_SAMPLES as i64], samples.to_vec()))?;

        let (prob, next_a, next_b) = match self.contract {
            Contract::Hc => {
                let h = Tensor::from_array((vec![2i64, 1, 64], state.a.clone()))?;
                let c = Tensor::from_array((vec![2i64, 1, 64], state.b.clone()))?;
                let out = self
                    .session
                    .run(ort::inputs!["x" => x, "h" => h, "c" => c])?;
                let (_, prob) = out["prob"].try_extract_tensor::<f32>()?;
                let p = *prob.first().context("empty VAD probability output")?;
                let (_, nh) = out["new_h"].try_extract_tensor::<f32>()?;
                let (_, nc) = out["new_c"].try_extract_tensor::<f32>()?;
                (p, nh.to_vec(), nc.to_vec())
            }
            Contract::Combined => {
                let st = Tensor::from_array((vec![2i64, 1, 128], state.a.clone()))?;
                let sr = Tensor::from_array((vec![1i64], vec![16_000i64]))?;
                let out = self
                    .session
                    .run(ort::inputs!["input" => x, "state" => st, "sr" => sr])?;
                let (_, prob) = out["output"].try_extract_tensor::<f32>()?;
                let p = *prob.first().context("empty VAD probability output")?;
                let (_, ns) = out["stateN"].try_extract_tensor::<f32>()?;
                (p, ns.to_vec(), Vec::new())
            }
        };

        state.a = next_a;
        state.b = next_b;
        Ok(prob)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SegmenterConfig {
    pub threshold: f32,
    pub min_speech_samples: u64,
    pub min_silence_samples: u64,
    pub pad_samples: u64,
    pub max_segment_samples: u64,
}

impl SegmenterConfig {
    pub fn from_ms(
        threshold: f32,
        min_speech_ms: u32,
        min_silence_ms: u32,
        pad_ms: u32,
        max_segment_ms: u32,
        rate: u32,
    ) -> Self {
        let ms = |v: u32| (v as u64 * rate as u64) / 1000;
        Self {
            threshold,
            min_speech_samples: ms(min_speech_ms),
            min_silence_samples: ms(min_silence_ms),
            pad_samples: ms(pad_ms),
            max_segment_samples: ms(max_segment_ms).max(ms(min_speech_ms).max(1)),
        }
    }
}

/// Half-open sample range `[start, end)` of an emitted segment, padded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentSpan {
    pub start: u64,
    pub end: u64,
    /// Unpadded speech extent, kept for diagnostics and for tests that want to
    /// check detection rather than padding arithmetic.
    pub voiced_start: u64,
    pub voiced_end: u64,
}

/// Turns a stream of frame probabilities into speech segments.
///
/// Open on the first frame over threshold; close after `min_silence` of
/// sub-threshold frames; discard anything shorter than `min_speech`; pad both
/// edges. The close delay is longer than the pad, so the trailing pad is always
/// audio we have already buffered.
#[derive(Debug)]
pub struct Segmenter {
    cfg: SegmenterConfig,
    in_speech: bool,
    speech_start: u64,
    last_voiced_end: u64,
    /// Absolute sample index just past the last frame fed in.
    cursor: u64,
}

impl Segmenter {
    pub fn new(cfg: SegmenterConfig) -> Self {
        Self {
            cfg,
            in_speech: false,
            speech_start: 0,
            last_voiced_end: 0,
            cursor: 0,
        }
    }

    /// A fresh segmenter that picks up at `cursor` instead of at sample zero.
    ///
    /// For the one case where a session throws its buffered audio away without
    /// ending: a queue drop or an xrun leaves a hole, and the half-built
    /// segment on the near side of it is discarded rather than spliced onto the
    /// far side (`pipeline::SessionPipeline::discard_across_gap`). The session's
    /// sample counter is continuous across that, so the segmenter that replaces
    /// this one has to agree with it about where "now" is.
    pub fn resuming_at(cfg: SegmenterConfig, cursor: u64) -> Self {
        Self {
            cursor,
            ..Self::new(cfg)
        }
    }

    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    // ---- 0.11.0, partial turns: begin --------------------------------------
    /// Where the speech currently being detected began, padded, or `None` when
    /// the segmenter is idle.
    ///
    /// The turn merger only learns about a span once the segmenter has CLOSED
    /// it, which needs `min_silence` (500 ms) of quiet. So while somebody is
    /// mid-sentence — the entire case partial captions exist for —
    /// `TurnMerger::pending_start` is `None` and this is the only thing that
    /// knows a turn is open. It is exactly the lower bound
    /// [`Self::retain_from`] already promises to keep buffered, said out loud.
    pub fn speech_open(&self) -> Option<u64> {
        self.in_speech
            .then(|| self.speech_start.saturating_sub(self.cfg.pad_samples))
    }
    // ---- 0.11.0, partial turns: end ----------------------------------------

    // ---- 0.12.5, sliced turns: begin ---------------------------------------
    /// How long the current sub-threshold run is, in samples, or `None` when
    /// the segmenter is not in speech at all.
    ///
    /// This is the segmenter's own `min_silence` countdown read out loud before
    /// it finishes. A span is only CLOSED after 500 ms of quiet, but the daemon
    /// knows about every shorter gap on the way there, and a gap of 120 ms is
    /// already a guarantee of the one thing [`crate::slice`] needs: the
    /// boundary is not inside a word, because the model scored it as
    /// not-speech.
    ///
    /// Zero while somebody is actually speaking — the last frame was voiced, so
    /// there is no dip — which is exactly the reading that refuses a cut.
    pub fn dip_len(&self) -> Option<u64> {
        self.in_speech
            .then(|| self.cursor.saturating_sub(self.last_voiced_end))
    }
    // ---- 0.12.5, sliced turns: end -----------------------------------------

    /// Sample index below which buffered audio can never be needed again.
    pub fn retain_from(&self) -> u64 {
        if self.in_speech {
            self.speech_start.saturating_sub(self.cfg.pad_samples)
        } else {
            // Idle: only enough history for a future segment's leading pad.
            self.cursor
                .saturating_sub(self.cfg.pad_samples + FRAME_SAMPLES as u64)
        }
    }

    /// Feed one frame's probability. `frame_start` must be the absolute sample
    /// index of the frame; frames are expected in order and contiguous.
    pub fn push_frame(
        &mut self,
        prob: f32,
        frame_start: u64,
        frame_len: u64,
    ) -> Option<SegmentSpan> {
        let frame_end = frame_start + frame_len;
        self.cursor = frame_end;
        let voiced = prob >= self.cfg.threshold;

        if !self.in_speech {
            if voiced {
                self.in_speech = true;
                self.speech_start = frame_start;
                self.last_voiced_end = frame_end;
            }
            return None;
        }

        if voiced {
            self.last_voiced_end = frame_end;
        }

        // Hard cap first: a monologue must still be cut into storable pieces.
        if frame_end.saturating_sub(self.speech_start) >= self.cfg.max_segment_samples {
            let voiced_end = self.last_voiced_end.max(frame_end);
            let span = self.emit(self.speech_start, voiced_end);
            if voiced {
                // Still talking — start the next segment where this one ended.
                self.speech_start = voiced_end;
                self.last_voiced_end = voiced_end;
            } else {
                self.in_speech = false;
            }
            return span;
        }

        if !voiced && frame_end.saturating_sub(self.last_voiced_end) >= self.cfg.min_silence_samples
        {
            let span = self.emit(self.speech_start, self.last_voiced_end);
            self.in_speech = false;
            return span;
        }

        None
    }

    /// Close an open segment at end of stream (source went away).
    pub fn flush(&mut self) -> Option<SegmentSpan> {
        if !self.in_speech {
            return None;
        }
        let span = self.emit(self.speech_start, self.last_voiced_end);
        self.in_speech = false;
        span
    }

    fn emit(&self, voiced_start: u64, voiced_end: u64) -> Option<SegmentSpan> {
        if voiced_end.saturating_sub(voiced_start) < self.cfg.min_speech_samples {
            return None;
        }
        Some(SegmentSpan {
            start: voiced_start.saturating_sub(self.cfg.pad_samples),
            end: (voiced_end + self.cfg.pad_samples).min(self.cursor),
            voiced_start,
            voiced_end,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 16_000;

    fn cfg() -> SegmenterConfig {
        SegmenterConfig::from_ms(0.5, 250, 500, 200, 30_000, RATE)
    }

    /// Drive the segmenter with a probability track expressed in frames.
    fn run(cfg: SegmenterConfig, track: &[f32]) -> Vec<SegmentSpan> {
        let mut seg = Segmenter::new(cfg);
        let mut out = Vec::new();
        for (i, p) in track.iter().enumerate() {
            if let Some(s) = seg.push_frame(*p, (i * FRAME_SAMPLES) as u64, FRAME_SAMPLES as u64) {
                out.push(s);
            }
        }
        out.extend(seg.flush());
        out
    }

    fn track(spec: &[(f32, usize)]) -> Vec<f32> {
        spec.iter()
            .flat_map(|(p, n)| std::iter::repeat_n(*p, *n))
            .collect()
    }

    #[test]
    fn silence_alone_produces_nothing() {
        assert!(run(cfg(), &track(&[(0.0, 200)])).is_empty());
    }

    #[test]
    fn one_burst_becomes_one_padded_segment() {
        // 32 speech frames = 1024 ms, then 40 silent frames = 1280 ms > 500 ms.
        let segs = run(cfg(), &track(&[(0.0, 20), (0.9, 32), (0.0, 40)]));
        assert_eq!(segs.len(), 1);
        let s = segs[0];
        assert_eq!(s.voiced_start, 20 * 512);
        assert_eq!(s.voiced_end, 52 * 512);
        assert_eq!(s.start, 20 * 512 - 3200); // 200 ms of pad at 16 kHz
        assert_eq!(s.end, 52 * 512 + 3200);
    }

    #[test]
    fn two_bursts_separated_by_long_silence_are_two_segments() {
        let segs = run(
            cfg(),
            &track(&[(0.0, 10), (0.9, 30), (0.0, 40), (0.9, 30), (0.0, 40)]),
        );
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].voiced_start, 10 * 512);
        assert_eq!(segs[1].voiced_start, 80 * 512);
        assert!(segs[0].voiced_end < segs[1].voiced_start);
    }

    #[test]
    fn a_short_gap_does_not_split_a_segment() {
        // 10 silent frames = 320 ms, under the 500 ms close threshold.
        let segs = run(cfg(), &track(&[(0.9, 20), (0.0, 10), (0.9, 20), (0.0, 40)]));
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].voiced_start, 0);
        assert_eq!(segs[0].voiced_end, 50 * 512);
    }

    #[test]
    fn a_burst_shorter_than_min_speech_is_discarded() {
        // 5 frames = 160 ms < 250 ms.
        assert!(run(cfg(), &track(&[(0.9, 5), (0.0, 40)])).is_empty());
        // 9 frames = 288 ms > 250 ms: kept.
        assert_eq!(run(cfg(), &track(&[(0.9, 9), (0.0, 40)])).len(), 1);
    }

    #[test]
    fn the_threshold_is_inclusive_and_sub_threshold_frames_do_not_open() {
        let segs = run(cfg(), &track(&[(0.5, 20), (0.0, 40)]));
        assert_eq!(segs.len(), 1);
        assert!(run(cfg(), &track(&[(0.49, 20), (0.0, 40)])).is_empty());
    }

    #[test]
    fn continuous_speech_is_cut_at_the_hard_cap() {
        // 30 s cap; feed 70 s of unbroken speech.
        let frames = (70 * RATE as usize) / FRAME_SAMPLES;
        let segs = run(cfg(), &track(&[(0.9, frames)]));
        assert!(
            segs.len() >= 2,
            "expected the cap to split, got {} segment(s)",
            segs.len()
        );
        for s in &segs[..segs.len() - 1] {
            let secs = (s.voiced_end - s.voiced_start) as f32 / RATE as f32;
            assert!(secs <= 30.05, "segment ran {secs} s past the 30 s cap");
        }
        // The cut is a partition: no gaps, no overlap in the voiced extents.
        for w in segs.windows(2) {
            assert_eq!(w[0].voiced_end, w[1].voiced_start);
        }
    }

    #[test]
    fn flush_closes_an_open_segment_when_the_source_disappears() {
        let mut seg = Segmenter::new(cfg());
        for i in 0..30 {
            assert!(
                seg.push_frame(0.9, (i * FRAME_SAMPLES) as u64, FRAME_SAMPLES as u64)
                    .is_none()
            );
        }
        let s = seg
            .flush()
            .expect("open speech must be flushed, not dropped");
        assert_eq!(s.voiced_start, 0);
        assert_eq!(s.voiced_end, 30 * 512);
        assert!(seg.flush().is_none(), "flush must be idempotent");
    }

    #[test]
    fn padding_is_clamped_to_available_audio() {
        // Speech starts at sample 0, so there is no room for a leading pad.
        let segs = run(cfg(), &track(&[(0.9, 20), (0.0, 40)]));
        assert_eq!(segs[0].start, 0);
    }

    #[test]
    fn retain_from_tracks_what_the_ring_still_needs() {
        let mut seg = Segmenter::new(cfg());
        for i in 0..100 {
            seg.push_frame(0.0, (i * FRAME_SAMPLES) as u64, FRAME_SAMPLES as u64);
        }
        // Idle: only the leading-pad window is worth keeping.
        assert!(seg.retain_from() >= 100 * 512 - 3200 - 512);

        for i in 100..140 {
            seg.push_frame(0.9, (i * FRAME_SAMPLES) as u64, FRAME_SAMPLES as u64);
        }
        // In speech: everything from the padded start must survive.
        assert_eq!(seg.retain_from(), 100 * 512 - 3200);
    }

    // ---- End-to-end against the real model ------------------------------

    /// Silero is speech-specific: a sine tone scores ~0.01 and would make a
    /// useless fixture. This synthesises a voiced source instead — a glottal
    /// pulse train shaped by three formant resonators, with the formants
    /// switching every 180 ms to imitate syllables. Measured against the
    /// bundled model it scores >0.5 on ~80% of frames.
    fn synth_speech(seconds: f32) -> Vec<f32> {
        const VOWELS: [[(f32, f32); 3]; 4] = [
            [(730.0, 90.0), (1090.0, 110.0), (2440.0, 180.0)],
            [(270.0, 60.0), (2290.0, 90.0), (3010.0, 180.0)],
            [(300.0, 60.0), (870.0, 90.0), (2240.0, 180.0)],
            [(640.0, 80.0), (1190.0, 90.0), (2390.0, 180.0)],
        ];
        let total = (seconds * RATE as f32) as usize;
        let mut out = Vec::with_capacity(total);
        let syllable = (0.18 * RATE as f32) as usize;
        let mut i = 0usize;
        while out.len() < total {
            let f0 = 110.0 + 10.0 * (i % 3) as f32;
            let period = (RATE as f32 / f0) as usize;
            let vowel = VOWELS[i % VOWELS.len()];

            let mut buf = vec![0.0f32; syllable];
            for (fc, bw) in vowel {
                let r = (-std::f32::consts::PI * bw / RATE as f32).exp();
                let th = 2.0 * std::f32::consts::PI * fc / RATE as f32;
                let a1 = -2.0 * r * th.cos();
                let a2 = r * r;
                let (mut y1, mut y2) = (0.0f32, 0.0f32);
                for (n, slot) in buf.iter_mut().enumerate() {
                    let x = if n % period == 0 { 1.0 } else { 0.0 };
                    let y = x - a1 * y1 - a2 * y2;
                    y2 = y1;
                    y1 = y;
                    *slot += y;
                }
            }
            let peak = buf.iter().fold(0.0f32, |m, v| m.max(v.abs())).max(1e-9);
            let n = buf.len() as f32;
            for (k, v) in buf.iter_mut().enumerate() {
                // Hann envelope so syllables have onsets rather than clicks.
                let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * k as f32 / n).cos();
                *v = 0.5 * (*v / peak) * w;
            }
            out.extend_from_slice(&buf);
            i += 1;
        }
        out.truncate(total);
        out
    }

    fn silence(seconds: f32) -> Vec<f32> {
        vec![0.0; (seconds * RATE as f32) as usize]
    }

    fn write_wav(path: &std::path::Path, samples: &[f32]) {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: RATE,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(path, spec).unwrap();
        for s in samples {
            w.write_sample((s.clamp(-1.0, 1.0) * 32767.0) as i16)
                .unwrap();
        }
        w.finalize().unwrap();
    }

    fn read_wav(path: &std::path::Path) -> Vec<f32> {
        let mut r = hound::WavReader::open(path).unwrap();
        assert_eq!(r.spec().sample_rate, RATE);
        assert_eq!(r.spec().channels, 1);
        r.samples::<i16>()
            .map(|s| s.unwrap() as f32 / 32767.0)
            .collect()
    }

    #[test]
    fn segments_a_generated_wav_at_the_expected_boundaries() {
        // Fixture: 1 s silence, 1.5 s speech, 1 s silence, 2 s speech, 1 s silence.
        let mut audio = Vec::new();
        audio.extend(silence(1.0));
        audio.extend(synth_speech(1.5));
        audio.extend(silence(1.0));
        audio.extend(synth_speech(2.0));
        audio.extend(silence(1.0));

        let dir = std::env::temp_dir().join(format!("nx-recall-vad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("fixture.wav");
        write_wav(&path, &audio);
        let audio = read_wav(&path);
        let _ = std::fs::remove_dir_all(&dir);

        let mut vad = SileroVad::from_bytes(crate::VAD_MODEL).expect("bundled model must load");
        let mut state = vad.new_state();
        let mut seg = Segmenter::new(cfg());
        let mut segs = Vec::new();
        for (i, frame) in audio.as_chunks::<FRAME_SAMPLES>().0.iter().enumerate() {
            let p = vad.frame(frame, &mut state).unwrap();
            if let Some(s) = seg.push_frame(p, (i * FRAME_SAMPLES) as u64, FRAME_SAMPLES as u64) {
                segs.push(s);
            }
        }
        segs.extend(seg.flush());

        assert_eq!(segs.len(), 2, "expected two speech segments, got {segs:?}");

        // 150 ms of slack absorbs the model's onset/offset latency.
        let tol = (0.15 * RATE as f32) as i64;
        let close = |got: u64, want: f32| {
            let want = (want * RATE as f32) as i64;
            let d = (got as i64 - want).abs();
            assert!(
                d <= tol,
                "boundary {got} is {d} samples from the expected {want}"
            );
        };
        close(segs[0].voiced_start, 1.0);
        close(segs[0].voiced_end, 2.5);
        close(segs[1].voiced_start, 3.5);
        close(segs[1].voiced_end, 5.5);
    }

    #[test]
    fn the_bundled_model_reports_silence_as_silence() {
        let mut vad = SileroVad::from_bytes(crate::VAD_MODEL).unwrap();
        let mut state = vad.new_state();
        for frame in silence(1.0).as_chunks::<FRAME_SAMPLES>().0 {
            let p = vad.frame(frame, &mut state).unwrap();
            assert!(p < 0.5, "silence scored {p} as speech");
        }
    }

    #[test]
    fn a_wrong_sized_frame_is_an_error_not_a_panic() {
        let mut vad = SileroVad::from_bytes(crate::VAD_MODEL).unwrap();
        let mut state = vad.new_state();
        assert!(vad.frame(&[0.0; 256], &mut state).is_err());
    }

    #[test]
    fn per_source_states_are_independent() {
        let mut vad = SileroVad::from_bytes(crate::VAD_MODEL).unwrap();
        let speech = synth_speech(1.0);
        let mut a = vad.new_state();
        let mut b = vad.new_state();

        // Feed A speech and B silence through the same model instance; B must
        // not inherit A's activation.
        let mut last_b = 1.0;
        for frame in speech.as_chunks::<FRAME_SAMPLES>().0 {
            vad.frame(frame, &mut a).unwrap();
            last_b = vad.frame(&[0.0; FRAME_SAMPLES], &mut b).unwrap();
        }
        assert!(
            last_b < 0.5,
            "silent source leaked to {last_b} from the other session"
        );
    }
}
