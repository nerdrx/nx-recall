//! Fallback format conversion.
//!
//! The capture stream *asks* PipeWire for 16 kHz mono f32 and its adapter
//! normally does the work with a proper resampler. These routines exist for the
//! case where the graph refuses and hands us the native rate anyway — a naive
//! linear resample is worse than PipeWire's, but it keeps the pipeline running
//! rather than dropping the source.

/// Average interleaved channels down to mono, in place of a proper downmix.
pub fn downmix(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    let frames = interleaved.len() / channels;
    let mut out = Vec::with_capacity(frames);
    let scale = 1.0 / channels as f32;
    for f in 0..frames {
        let base = f * channels;
        let sum: f32 = interleaved[base..base + channels].iter().sum();
        out.push(sum * scale);
    }
    out
}

/// Linear-interpolating resampler with carry-over between calls.
///
/// The carried tail is what keeps successive buffers from clicking: without it
/// each buffer would restart interpolation from its own first sample.
#[derive(Debug, Default)]
pub struct LinearResampler {
    /// Fractional read position within `[prev, input]`.
    phase: f64,
    prev: Option<f32>,
}

impl LinearResampler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.phase = 0.0;
        self.prev = None;
    }

    pub fn process(&mut self, input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
        if input.is_empty() {
            return Vec::new();
        }
        if from_rate == to_rate || from_rate == 0 || to_rate == 0 {
            self.prev = input.last().copied();
            return input.to_vec();
        }
        let step = from_rate as f64 / to_rate as f64;

        // Splice the carried sample onto the front so an output point that
        // straddles the buffer boundary interpolates across it instead of
        // restarting from zero.
        let mut ext = Vec::with_capacity(input.len() + 1);
        if let Some(p) = self.prev {
            ext.push(p);
        }
        ext.extend_from_slice(input);

        let last = (ext.len() - 1) as f64;
        let mut pos = self.phase;
        let mut out = Vec::with_capacity((ext.len() as f64 / step) as usize + 2);
        while pos < last {
            let base = pos.floor();
            let idx = base as usize;
            let frac = (pos - base) as f32;
            let a = ext[idx];
            let b = ext[idx + 1];
            out.push(a + (b - a) * frac);
            pos += step;
        }

        // Carry the leftover fraction relative to the new trailing sample.
        self.phase = pos - last;
        self.prev = ext.last().copied();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_input_passes_through_downmix() {
        let x = vec![1.0, 2.0, 3.0];
        assert_eq!(downmix(&x, 1), x);
        assert_eq!(downmix(&x, 0), x);
    }

    #[test]
    fn stereo_downmixes_to_the_average() {
        let x = vec![1.0, 3.0, -1.0, 1.0, 0.5, 0.5];
        assert_eq!(downmix(&x, 2), vec![2.0, 0.0, 0.5]);
    }

    #[test]
    fn a_ragged_tail_is_discarded_rather_than_read_out_of_bounds() {
        let x = vec![1.0, 3.0, 5.0];
        assert_eq!(downmix(&x, 2), vec![2.0]);
    }

    #[test]
    fn matching_rates_are_a_passthrough() {
        let mut r = LinearResampler::new();
        let x: Vec<f32> = (0..10).map(|i| i as f32).collect();
        assert_eq!(r.process(&x, 16_000, 16_000), x);
    }

    #[test]
    fn downsampling_produces_roughly_the_right_count() {
        let mut r = LinearResampler::new();
        let x = vec![0.0f32; 48_000];
        let out = r.process(&x, 48_000, 16_000);
        assert!(
            (out.len() as i64 - 16_000).abs() <= 1,
            "got {} samples",
            out.len()
        );
    }

    #[test]
    fn upsampling_produces_roughly_the_right_count() {
        let mut r = LinearResampler::new();
        let x = vec![0.0f32; 8_000];
        let out = r.process(&x, 8_000, 16_000);
        assert!(
            (out.len() as i64 - 16_000).abs() <= 2,
            "got {} samples",
            out.len()
        );
    }

    #[test]
    fn a_ramp_stays_a_ramp() {
        let mut r = LinearResampler::new();
        let x: Vec<f32> = (0..1000).map(|i| i as f32 / 1000.0).collect();
        let out = r.process(&x, 2000, 1000);
        // Linear interpolation of a linear signal is exact.
        for (i, v) in out.iter().enumerate().skip(1) {
            let want = (i as f32 * 2.0) / 1000.0;
            assert!((v - want).abs() < 1e-3, "sample {i}: {v} vs {want}");
        }
    }

    #[test]
    fn successive_buffers_stay_continuous() {
        // The same ramp fed in one go and in chunks must resample identically.
        let x: Vec<f32> = (0..2400).map(|i| i as f32 / 2400.0).collect();

        let mut whole = LinearResampler::new();
        let a = whole.process(&x, 48_000, 16_000);

        let mut chunked = LinearResampler::new();
        let mut b = Vec::new();
        for c in x.chunks(240) {
            b.extend(chunked.process(c, 48_000, 16_000));
        }

        assert_eq!(a.len(), b.len());
        for (i, (p, q)) in a.iter().zip(&b).enumerate() {
            assert!((p - q).abs() < 1e-4, "sample {i}: whole {p} vs chunked {q}");
        }
    }

    #[test]
    fn an_empty_buffer_is_harmless() {
        let mut r = LinearResampler::new();
        assert!(r.process(&[], 48_000, 16_000).is_empty());
        r.reset();
        assert!(r.process(&[], 48_000, 16_000).is_empty());
    }
}
