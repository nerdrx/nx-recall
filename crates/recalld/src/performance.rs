//! Bounded, process-local measurements. No transcript or query text is kept.
use serde_json::{Value, json};
use std::{collections::VecDeque, sync::Mutex};
pub const WINDOW: usize = 256;
#[derive(Default)]
pub struct Latency {
    samples: Mutex<VecDeque<f64>>,
}
impl Latency {
    pub fn record_ms(&self, ms: f64) {
        if !ms.is_finite() || ms < 0.0 {
            return;
        }
        let mut samples = self.samples.lock().unwrap_or_else(|p| p.into_inner());
        if samples.len() == WINDOW {
            samples.pop_front();
        }
        samples.push_back(ms);
    }
    pub fn measure(&self) -> Measurement<'_> {
        Measurement {
            latency: self,
            started: std::time::Instant::now(),
        }
    }
    pub fn snapshot(&self) -> Value {
        let samples = self.samples.lock().unwrap_or_else(|p| p.into_inner());
        let latest = samples.back().copied();
        let mut sorted: Vec<_> = samples.iter().copied().collect();
        drop(samples);
        sorted.sort_by(f64::total_cmp);
        let percentile = |percent: usize| -> Option<f64> {
            if sorted.is_empty() {
                None
            } else {
                Some(sorted[(sorted.len() * percent).div_ceil(100) - 1])
            }
        };
        json!({"samples": sorted.len(), "window": WINDOW, "latest_ms": latest,
            "p50_ms": percentile(50), "p95_ms": percentile(95), "max_ms": sorted.last()})
    }
}
/// Records elapsed time even on early returns; never records text or identifiers.
pub struct Measurement<'a> {
    latency: &'a Latency,
    started: std::time::Instant,
}
impl Drop for Measurement<'_> {
    fn drop(&mut self) {
        self.latency
            .record_ms(self.started.elapsed().as_secs_f64() * 1000.0);
    }
}
#[derive(Default)]
pub struct Performance {
    pub capture: Latency,
    pub search: Latency,
    pub queue_wait: Latency,
    pub recognition: Latency,
    pub partial_recognition: Latency,
    pub overlap: Latency,
    pub speaker_embedding: Latency,
    pub speaker_matching: Latency,
    pub transcript_commit: Latency,
    pub refinement: Latency,
    pub audio_write: Latency,
    pub semantic: Latency,
}
impl Performance {
    pub fn stages(&self) -> Value {
        json!({"queue_wait":self.queue_wait.snapshot(), "recognition":self.recognition.snapshot(),
            "partial_recognition":self.partial_recognition.snapshot(), "overlap":self.overlap.snapshot(),
            "speaker_embedding":self.speaker_embedding.snapshot(), "speaker_matching":self.speaker_matching.snapshot(),
            "transcript_commit":self.transcript_commit.snapshot(), "audio_write":self.audio_write.snapshot(),
            "refinement":self.refinement.snapshot(), "semantic":self.semantic.snapshot()})
    }
    pub fn record_queue_wait(&self, first_ns: u64, samples: usize, now_ns: u64) {
        let end =
            first_ns as u128 + samples as u128 * 1_000_000_000 / crate::config::SAMPLE_RATE as u128;
        if end <= now_ns as u128 {
            self.queue_wait
                .record_ms((now_ns as u128 - end) as f64 / 1_000_000.0);
        }
    }
}
/// Resident memory of this daemon, not Electron or separately spawned models.
pub fn resident_bytes() -> Option<u64> {
    parse_resident(&std::fs::read_to_string("/proc/self/status").ok()?)
}
fn parse_resident(status: &str) -> Option<u64> {
    let mut parts = status
        .lines()
        .find(|line| line.starts_with("VmRSS:"))?
        .split_whitespace();
    parts.next()?;
    let kib: u64 = parts.next()?.parse().ok()?;
    if parts.next()? != "kB" {
        return None;
    }
    kib.checked_mul(1024)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn queue_measurement_uses_last_sample_and_rejects_future_clock() {
        let p = Performance::default();
        p.record_queue_wait(
            1_000_000_000,
            crate::config::SAMPLE_RATE as usize,
            2_050_000_000,
        );
        p.record_queue_wait(3_000_000_000, 1, 2_000_000_000);
        assert_eq!(p.queue_wait.snapshot()["samples"], 1);
        assert_eq!(p.queue_wait.snapshot()["latest_ms"], 50.0);
    }
    #[test]
    fn scoped_measurements_record_early_returns_without_other_stage_samples() {
        let p = Performance::default();
        let work = || -> Result<(), ()> {
            let _timer = p.recognition.measure();
            Err(())
        };
        assert!(work().is_err());
        assert_eq!(p.stages()["recognition"]["samples"], 1);
        assert_eq!(p.stages()["speaker_matching"]["samples"], 0);
    }
    #[test]
    fn empty_is_unknown() {
        let s = Latency::default().snapshot();
        assert_eq!(s["samples"], 0);
        assert!(s["p95_ms"].is_null());
    }
    #[test]
    fn percentiles_and_latest_are_bounded() {
        let l = Latency::default();
        for n in 0..300 {
            l.record_ms(n as f64);
        }
        let s = l.snapshot();
        assert_eq!(s["samples"], 256);
        assert_eq!(s["latest_ms"], 299.0);
        assert_eq!(s["p50_ms"], 171.0);
        assert_eq!(s["p95_ms"], 287.0);
        for v in [f64::NAN, f64::INFINITY, -1.0] {
            l.record_ms(v);
        }
        assert_eq!(l.snapshot(), s);
    }
    #[test]
    fn rss_requires_valid_units_and_no_overflow() {
        assert_eq!(parse_resident("Name: test\nVmRSS: 123 kB\n"), Some(125952));
        assert_eq!(parse_resident("VmRSS: 123 MB"), None);
        assert_eq!(parse_resident("VmRSS: 18446744073709551615 kB"), None);
        assert_eq!(parse_resident("Name: test"), None);
    }
}
