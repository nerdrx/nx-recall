//! Light mode (0.13.x): swap the live decoder to the smaller Parakeet-TDT
//! 110m export while a game is running, so the four cores the multilingual
//! 0.6b-v3 transducer spends (FINDINGS §40: 89.5% of the live path's 29.9
//! CPU s/audio-minute) are not competing with whatever is drawing frames.
//!
//! Three ways in, all resolved by [`decide`]:
//!
//! * **A captured source is a game** (`[asr].light_mode_games`, seeded with
//!   the same `"vrchat"` substring `[identity].vrchat_sources` already
//!   matches — VRChat is the one game this install has ever captured).
//! * **The GPU is sustained-busy** (`[asr].light_mode_gpu_busy_pct`), read
//!   through [`crate::night::gpu_busy_pct`] like the night shift's own gate,
//!   but smoothed: FINDINGS §12/13 measured that single reading swinging
//!   ±20 points at 1 Hz, so [`GpuBusyMonitor`] keeps a rolling window and
//!   [`decide`] is handed the window's *median*, not the last sample.
//! * **The manual switch** (`[asr].light_mode = "on"` or `"off"`), which
//!   overrides both.
//!
//! [`decide`] is a pure function on purpose: the two ways the world can say
//! "the machine is gaming" are both noisy and both worth testing without a
//! GPU or a captured source anywhere nearby.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::config::LightMode;

/// How long a window of `gpu_busy_percent` samples to keep before taking the
/// median. FINDINGS §12/13's number: ±20 points of noise per 1 Hz sample, so
/// thirty seconds is roughly the shortest window that outvotes it.
pub const GPU_WINDOW: Duration = Duration::from_secs(30);

/// A rolling window of `gpu_busy_percent` samples and their median.
///
/// Kept apart from the decision itself so the noisy half (reading a sysfs
/// file on a timer) and the pure half ([`decide`]) can be tested separately.
#[derive(Debug, Clone, Default)]
pub struct GpuBusyMonitor {
    samples: VecDeque<(Instant, u32)>,
}

impl GpuBusyMonitor {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::new(),
        }
    }

    /// Record one reading (or none, when the machine has no `gpu_busy_percent`
    /// file to read — `crate::night::gpu_busy_pct` already turns "no AMD DRM
    /// node" into `None` once) and drop everything older than [`GPU_WINDOW`].
    pub fn sample(&mut self, now: Instant, pct: Option<u32>) {
        if let Some(pct) = pct {
            self.samples.push_back((now, pct));
        }
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) > GPU_WINDOW)
        {
            self.samples.pop_front();
        }
    }

    /// The window's median, or `None` before the first sample (or on a
    /// machine with no readable GPU busy counter at all).
    pub fn median(&self) -> Option<u32> {
        if self.samples.is_empty() {
            return None;
        }
        let mut v: Vec<u32> = self.samples.iter().map(|(_, p)| *p).collect();
        v.sort_unstable();
        Some(v[v.len() / 2])
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }
}

/// Why the light decoder is, or is not, the one live right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reason {
    /// `[asr].light_mode = "on"`.
    ManualOn,
    /// `[asr].light_mode = "off"`.
    ManualOff,
    /// Auto, and a captured source matched `light_mode_games`.
    Game,
    /// Auto, and the GPU's 30 s median busy percent cleared the threshold.
    GpuBusy,
    /// Auto, and neither condition held.
    #[default]
    Clear,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::ManualOn => "manual: light_mode = on",
            Reason::ManualOff => "manual: light_mode = off",
            Reason::Game => "a captured source matches light_mode_games",
            Reason::GpuBusy => "gpu_busy_percent's 30s median cleared the threshold",
            Reason::Clear => "no game captured and the GPU is not sustained-busy",
        }
    }
}

/// Is `key` one of the configured game patterns? Matched the same way
/// `[identity].vrchat_sources` is: a lower-cased substring of the source's
/// match key. `patterns` are expected already lower-cased (that is what
/// `AsrConfig::light_mode_games` stores); `key` is lower-cased here so a
/// caller can pass a match key straight off the row.
pub fn is_game_source(patterns: &[String], key: &str) -> bool {
    let key = key.to_lowercase();
    patterns.iter().any(|p| !p.is_empty() && key.contains(p))
}

/// What the live path is doing right now: whether the light decoder is the
/// one loaded, and why. Read by `status.asr.devices` and published on
/// `Topic::Status` the same way `GraphState` is (`Control::set_light_state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LightState {
    pub light: bool,
    pub reason: Reason,
}

/// One evaluation of the switch: given the configured mode, whether any
/// captured source matched a game pattern, and the GPU's smoothed busy
/// percent, decide whether the light decoder should be live right now and
/// why.
pub fn decide(
    mode: LightMode,
    game_active: bool,
    gpu_busy_median: Option<u32>,
    gpu_busy_threshold_pct: u32,
) -> (bool, Reason) {
    match mode {
        LightMode::On => (true, Reason::ManualOn),
        LightMode::Off => (false, Reason::ManualOff),
        LightMode::Auto => {
            if game_active {
                (true, Reason::Game)
            } else if gpu_busy_median.is_some_and(|m| m >= gpu_busy_threshold_pct) {
                (true, Reason::GpuBusy)
            } else {
                (false, Reason::Clear)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_on_ignores_everything_else() {
        assert_eq!(
            decide(LightMode::On, false, None, 70),
            (true, Reason::ManualOn)
        );
        assert_eq!(
            decide(LightMode::On, false, Some(0), 70),
            (true, Reason::ManualOn)
        );
    }

    #[test]
    fn manual_off_ignores_everything_else() {
        assert_eq!(
            decide(LightMode::Off, true, Some(100), 70),
            (false, Reason::ManualOff)
        );
    }

    #[test]
    fn auto_follows_a_captured_game() {
        assert_eq!(
            decide(LightMode::Auto, true, None, 70),
            (true, Reason::Game)
        );
    }

    #[test]
    fn auto_follows_a_busy_gpu() {
        assert_eq!(
            decide(LightMode::Auto, false, Some(80), 70),
            (true, Reason::GpuBusy)
        );
        // At the threshold counts as busy, not just over it.
        assert_eq!(
            decide(LightMode::Auto, false, Some(70), 70),
            (true, Reason::GpuBusy)
        );
    }

    #[test]
    fn auto_clears_when_neither_holds() {
        assert_eq!(
            decide(LightMode::Auto, false, Some(69), 70),
            (false, Reason::Clear)
        );
        assert_eq!(
            decide(LightMode::Auto, false, None, 70),
            (false, Reason::Clear)
        );
    }

    #[test]
    fn game_beats_gpu_reading_for_the_reason_reported() {
        // Both conditions true: the report should still name a cause, and
        // it is deterministic which one it names.
        assert_eq!(
            decide(LightMode::Auto, true, Some(99), 70),
            (true, Reason::Game)
        );
    }

    #[test]
    fn game_source_matching_is_case_insensitive_substring() {
        let games = vec!["vrchat".to_string()];
        assert!(is_game_source(&games, "VRChat.exe"));
        assert!(is_game_source(&games, "vrchat.exe"));
        assert!(!is_game_source(&games, "Discord.exe"));
    }

    #[test]
    fn an_empty_pattern_matches_nothing() {
        // Same guard `identity_prior` carries: `"".contains("")` is true, so
        // an empty entry in the list would otherwise call every source a game.
        let games = vec![String::new()];
        assert!(!is_game_source(&games, "anything"));
    }

    // ---- GpuBusyMonitor -----------------------------------------------

    #[test]
    fn the_monitor_is_empty_before_any_sample() {
        assert_eq!(GpuBusyMonitor::new().median(), None);
    }

    #[test]
    fn a_none_reading_records_nothing_but_still_ages_the_window() {
        let mut m = GpuBusyMonitor::new();
        let t0 = Instant::now();
        m.sample(t0, None);
        assert_eq!(m.median(), None);
        assert_eq!(m.len(), 0);
    }

    #[test]
    fn the_median_outvotes_a_noisy_single_reading() {
        let mut m = GpuBusyMonitor::new();
        let t0 = Instant::now();
        // FINDINGS §12/13: ±20 points of noise around a true ~80% load.
        for (i, pct) in [60, 100, 60, 100, 80, 90, 70].into_iter().enumerate() {
            m.sample(t0 + Duration::from_secs(i as u64), Some(pct));
        }
        assert_eq!(m.median(), Some(80));
    }

    #[test]
    fn samples_older_than_the_window_are_dropped() {
        let mut m = GpuBusyMonitor::new();
        let t0 = Instant::now();
        m.sample(t0, Some(100));
        m.sample(t0 + GPU_WINDOW + Duration::from_secs(1), Some(0));
        // The stale 100 must be gone, leaving only the fresh 0.
        assert_eq!(m.median(), Some(0));
        assert_eq!(m.len(), 1);
    }
}
