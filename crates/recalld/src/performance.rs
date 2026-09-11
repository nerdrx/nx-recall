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
#[derive(Default)]
pub struct Performance {
    pub capture: Latency,
    pub search: Latency,
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
