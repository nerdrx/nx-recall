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

// ---- the wire's time formats -------------------------------------------
//
// Rows carry UTC nanoseconds, but a JSON number cannot: 1.8e18 exceeds 2^53 and
// would be silently rounded by every JavaScript client. So the socket says both
// (PROTOCOL "Field conventions"): `t_ms` as a number for display, `t_ns` as a
// string for fidelity. Dates that a human or a client writes — `first_seen`,
// a search window — travel as ISO-8601 UTC.

pub fn ns_to_ms(utc_ns: i64) -> i64 {
    utc_ns.div_euclid(1_000_000)
}

/// `1970-01-01T00:00:00Z`-style UTC, seconds resolution.
pub fn iso8601(utc_ns: i64) -> String {
    let secs = utc_ns.div_euclid(1_000_000_000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod / 60) % 60,
        tod % 60
    )
}

/// `YYYY-MM-DDTHH:MM:SS[.fff][Z]` as UTC nanoseconds. A bare `YYYY-MM-DD` is
/// midnight UTC. Anything else is `None` — a filter the daemon cannot read is
/// an error, never a silently different window.
pub fn parse_iso8601(text: &str) -> Option<i64> {
    let text = text.trim();
    let bytes = text.as_bytes();
    if bytes.len() < 10 {
        return None;
    }
    let num = |a: usize, b: usize| text.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    if bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut secs = days_from_civil(y, mo as u32, d as u32) * 86_400;
    let mut nanos = 0i64;
    if bytes.len() > 10 {
        if bytes[10] != b'T' && bytes[10] != b' ' {
            return None;
        }
        if bytes.len() < 19 || bytes[13] != b':' || bytes[16] != b':' {
            return None;
        }
        let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
        if h > 23 || mi > 59 || s > 60 {
            return None;
        }
        secs += h * 3600 + mi * 60 + s;
        if bytes.len() > 19 && bytes[19] == b'.' {
            let frac: String = text[20..]
                .chars()
                .take_while(char::is_ascii_digit)
                .chain(std::iter::repeat('0'))
                .take(9)
                .collect();
            nanos = frac.parse::<i64>().ok()?;
        }
    }
    Some(secs * 1_000_000_000 + nanos)
}

/// Howard Hinnant's `days_from_civil`: a calendar date to days since the epoch.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m as i64 - 3 } else { m as i64 + 9 }) + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Its inverse: days since the epoch to a calendar date.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
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
    fn iso8601_round_trips_through_the_parser() {
        assert_eq!(iso8601(0), "1970-01-01T00:00:00Z");
        // 2026-08-31T18:46:00Z
        let t = 1_788_201_960_000_000_000;
        assert_eq!(iso8601(t), "2026-08-31T18:46:00Z");
        assert_eq!(parse_iso8601(&iso8601(t)), Some(t));
        assert_eq!(parse_iso8601("2026-08-31"), Some(1_788_134_400_000_000_000));
        // What `new Date(...).toISOString()` produces, milliseconds and all.
        assert_eq!(
            parse_iso8601("2026-08-31T18:46:00.250Z"),
            Some(t + 250_000_000)
        );
        assert_eq!(ns_to_ms(t), 1_788_201_960_000);
    }

    #[test]
    fn an_unreadable_date_is_refused_rather_than_guessed() {
        for bad in [
            "",
            "today",
            "2026-13-01T00:00:00Z",
            "2026-08-31X18:46:00Z",
            "2026/08/31",
            "2026-08-31T18:46",
        ] {
            assert_eq!(parse_iso8601(bad), None, "{bad:?} must not parse");
        }
    }

    #[test]
    fn the_civil_date_conversions_are_inverses() {
        for days in [-25_000i64, -1, 0, 1, 19_000, 20_697, 100_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days, "round trip at {days}");
        }
        // Leap day, both directions.
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
    }

    #[test]
    fn monotonic_does_not_go_backwards() {
        let a = monotonic_ns();
        let b = monotonic_ns();
        assert!(b >= a);
        assert!(a > 0);
    }
}
