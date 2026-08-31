//! Time handling.
//!
//! Two clocks, deliberately kept apart:
//!
//! * `CLOCK_MONOTONIC` stamps every captured buffer. It never jumps, so sample
//!   counts and elapsed durations derived from it stay honest across NTP steps,
//!   suspend/resume and DST changes.
//! * UTC is read *once* per anchor and everything else is `anchor_utc +
//!   (monotonic delta)`. Reading the wall clock again at processing time would
//!   attribute audio to whenever the VAD thread happened to get around to it,
//!   which under load can be seconds late.
//!
//! Local time is never stored anywhere; rows carry UTC nanoseconds.

use std::time::{SystemTime, UNIX_EPOCH};

/// Nanoseconds since an arbitrary boot-relative origin, from `CLOCK_MONOTONIC`.
pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, correctly aligned timespec we own for the call.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        // clock_gettime(CLOCK_MONOTONIC) cannot fail on Linux for a valid
        // pointer; fall back to zero rather than panicking on a capture path.
        return 0;
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

/// Nanoseconds since the Unix epoch, UTC.
pub fn utc_now_ns() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(e) => -(e.duration().as_nanos() as i64),
    }
}

/// Ties one monotonic reading to one UTC reading.
///
/// Every wall-clock timestamp downstream is computed from an anchor plus a
/// monotonic delta, so a mid-session clock step cannot stretch or reorder
/// segments that were captured contiguously.
#[derive(Debug, Clone, Copy)]
pub struct Anchor {
    pub mono_ns: u64,
    pub utc_ns: i64,
}

impl Anchor {
    /// Take a fresh pair of readings.
    pub fn now() -> Self {
        Self {
            mono_ns: monotonic_ns(),
            utc_ns: utc_now_ns(),
        }
    }

    /// Anchor a known monotonic instant to the current wall clock.
    pub fn at(mono_ns: u64) -> Self {
        let now = Self::now();
        Self {
            mono_ns,
            utc_ns: now.utc_ns + (mono_ns as i64 - now.mono_ns as i64),
        }
    }

    /// UTC nanoseconds for a monotonic instant, via this anchor.
    pub fn utc_of(&self, mono_ns: u64) -> i64 {
        self.utc_ns + (mono_ns as i64 - self.mono_ns as i64)
    }
}

/// Nanoseconds occupied by `samples` at `rate` Hz.
pub fn samples_to_ns(samples: u64, rate: u32) -> i64 {
    ((samples as i128 * 1_000_000_000i128) / rate as i128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_maps_monotonic_to_utc_linearly() {
        let a = Anchor {
            mono_ns: 1_000_000_000,
            utc_ns: 1_700_000_000_000_000_000,
        };
        assert_eq!(a.utc_of(1_000_000_000), 1_700_000_000_000_000_000);
        assert_eq!(a.utc_of(2_500_000_000), 1_700_000_001_500_000_000);
        // Instants before the anchor extrapolate backwards.
        assert_eq!(a.utc_of(500_000_000), 1_699_999_999_500_000_000);
    }

    #[test]
    fn sample_arithmetic_is_exact_at_16k() {
        assert_eq!(samples_to_ns(16_000, 16_000), 1_000_000_000);
        assert_eq!(samples_to_ns(512, 16_000), 32_000_000);
        assert_eq!(samples_to_ns(0, 16_000), 0);
    }

    #[test]
    fn monotonic_does_not_go_backwards() {
        let a = monotonic_ns();
        let b = monotonic_ns();
        assert!(b >= a);
        assert!(a > 0);
    }
}
