//! Where the bar goes, how big it is, and how faded it is right now.
//!
//! Split out of `desktop.rs` on purpose: none of it needs a compositor, so all
//! of it can be tested on a machine with no Wayland session at all — which is
//! the only part of the layer-shell path that could be. See docs/OVERLAY.md.

use crate::settings::Bounds;

/// The Electron captions window's `defaultBounds()`, in one place, because the
/// two paths must open a bar of the same shape. A person who has never dragged
/// the window must not get a different caption bar depending on which of the
/// two surfaces their desktop can host.
const MAX_W: u32 = 1100;
const MAX_H: u32 = 340;
const WIDTH_FRAC: f64 = 0.62;
const HEIGHT_FRAC: f64 = 0.30;
/// Clear of a bottom panel, the same 96 the Electron window uses.
pub const DEFAULT_BOTTOM_MARGIN: i32 = 96;

/// A caption bar smaller than this is not a caption bar; `normalizeBounds`
/// refuses one and so does this.
const MIN_W: u32 = 240;
const MIN_H: u32 = 90;

/// The bar's size in LOGICAL pixels, from the remembered bounds if there are
/// any and from the output if there are not.
///
/// The remembered size is honoured; the remembered POSITION mostly is not — see
/// `bottom_margin`.
pub fn surface_size(bounds: Option<Bounds>, output: (u32, u32)) -> (u32, u32) {
    let (ow, oh) = (output.0.max(MIN_W), output.1.max(MIN_H));
    let (w, h) = match bounds {
        Some(b) => (b.width, b.height),
        None => (
            MAX_W.min((ow as f64 * WIDTH_FRAC).round() as u32),
            MAX_H.min((oh as f64 * HEIGHT_FRAC).round() as u32),
        ),
    };
    (w.clamp(MIN_W, ow), h.clamp(MIN_H, oh))
}

/// How far off the bottom of the output the bar sits.
///
/// A layer surface is placed by anchor and margin, not by a global desktop
/// coordinate, so the remembered `bounds.y` can only be honoured when it can be
/// read as an offset from the bottom of THIS output — which is exactly the
/// single-monitor case, and exactly the case where it means what the person
/// meant. A `y` that lands outside this output (they dragged it to the other
/// screen, or unplugged one) falls back to the default rather than pinning the
/// bar to an edge it was never at.
pub fn bottom_margin(bounds: Option<Bounds>, height: u32, output: (u32, u32)) -> i32 {
    let oh = output.1 as i64;
    let Some(b) = bounds else {
        return DEFAULT_BOTTOM_MARGIN;
    };
    let margin = oh - (b.y as i64 + height as i64);
    if margin < 0 || margin > oh - height as i64 {
        return DEFAULT_BOTTOM_MARGIN;
    }
    margin as i32
}

/// The fade, as a pure function of the clock — `visibleCaptions` in
/// gui/src/renderer/lib/captions.js, same arithmetic.
///
/// Per-STACK, not per-row: the bar is one thing you glance at, and a stack whose
/// top line has vanished while the one under it is still solid reads as a
/// rendering bug. One second of actual fading at the end of the hold, so it
/// leaves rather than blinks.
pub fn fade_at(age_s: f32, hold_s: f32) -> f32 {
    if age_s <= hold_s {
        return 1.0;
    }
    (1.0 - (age_s - hold_s)).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(y: i32, w: u32, h: u32) -> Bounds {
        Bounds {
            x: 0,
            y,
            width: w,
            height: h,
        }
    }

    /// The shape the Electron window opens at, for a person who has never
    /// dragged it. 2560x1440: 0.62 of the width is 1587, so the 1100 cap wins;
    /// 0.30 of the height is 432, so the 340 cap wins.
    #[test]
    fn an_unplaced_bar_is_the_same_shape_the_electron_window_opens_at() {
        assert_eq!(surface_size(None, (2560, 1440)), (1100, 340));
        // …and on a small output the fractions win instead.
        assert_eq!(surface_size(None, (1366, 768)), (847, 230));
    }

    #[test]
    fn a_remembered_size_is_honoured_and_a_silly_one_is_not() {
        assert_eq!(
            surface_size(Some(bounds(0, 900, 200)), (2560, 1440)),
            (900, 200)
        );
        // Wider than the output it landed on: clipped to the output rather than
        // asking the compositor for a surface it cannot place.
        assert_eq!(
            surface_size(Some(bounds(0, 9000, 200)), (1920, 1080)),
            (1920, 200)
        );
    }

    #[test]
    fn a_remembered_position_becomes_a_bottom_margin_when_it_can() {
        // 1440 tall, a 340-tall bar whose top was at 1004 → 96 off the bottom.
        assert_eq!(
            bottom_margin(Some(bounds(1004, 1100, 340)), 340, (2560, 1440)),
            96
        );
        assert_eq!(
            bottom_margin(Some(bounds(600, 1100, 340)), 340, (2560, 1440)),
            500
        );
    }

    /// The monitor that is not there today. A `y` off this output must not pin
    /// the bar to an edge it was never at.
    #[test]
    fn a_position_on_some_other_output_falls_back_to_the_default() {
        assert_eq!(
            bottom_margin(Some(bounds(3000, 1100, 340)), 340, (2560, 1440)),
            DEFAULT_BOTTOM_MARGIN
        );
        assert_eq!(
            bottom_margin(Some(bounds(-500, 1100, 340)), 340, (2560, 1440)),
            DEFAULT_BOTTOM_MARGIN
        );
        assert_eq!(
            bottom_margin(None, 340, (2560, 1440)),
            DEFAULT_BOTTOM_MARGIN
        );
    }

    /// The hold is flat, then one second of fading, then gone. Same numbers the
    /// renderer's `visibleCaptions` produces.
    #[test]
    fn the_stack_holds_then_leaves_over_one_second() {
        assert_eq!(fade_at(0.0, 12.0), 1.0);
        assert_eq!(fade_at(12.0, 12.0), 1.0);
        assert!((fade_at(12.5, 12.0) - 0.5).abs() < 1e-6);
        assert_eq!(fade_at(13.0, 12.0), 0.0);
        assert_eq!(fade_at(600.0, 12.0), 0.0);
    }
}
