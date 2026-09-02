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
pub fn bottom_margin(
    bounds: Option<Bounds>,
    height: u32,
    output: (u32, u32),
    origin: (i32, i32),
) -> i32 {
    let oh = output.1 as i64;
    let Some(b) = bounds else {
        return DEFAULT_BOTTOM_MARGIN;
    };
    // `bounds` is in the same global desktop coordinates the Electron window
    // reports; the margin is relative to this output. The origin is the whole
    // difference, and on a single-monitor desktop it is (0, 0).
    let local_y = b.y as i64 - origin.1 as i64;
    let margin = oh - (local_y + height as i64);
    if margin < 0 || margin > oh - height as i64 {
        return DEFAULT_BOTTOM_MARGIN;
    }
    margin as i32
}

/// How far from the LEFT edge of the output the bar sits.
///
/// Since 0.10.1 the surface is anchored `BOTTOM | LEFT` rather than `BOTTOM`
/// alone. Bottom alone centres a fixed-width surface for free, which was fine
/// while the bar could not be moved; the moment it can, "centred" is a position
/// the client cannot name and therefore cannot nudge. So the centring is done
/// here, in arithmetic, and a remembered `x` simply replaces it.
pub fn left_margin(
    bounds: Option<Bounds>,
    width: u32,
    output: (u32, u32),
    origin: (i32, i32),
) -> i32 {
    let ow = output.0 as i64;
    let w = width as i64;
    let centred = ((ow - w) / 2).max(0) as i32;
    let Some(b) = bounds else {
        return centred;
    };
    let local_x = b.x as i64 - origin.0 as i64;
    // Same rule as `bottom_margin`: a coordinate that does not land on THIS
    // output belongs to a monitor that is not here today.
    if local_x < 0 || local_x > ow - w {
        return centred;
    }
    local_x as i32
}

/// Keep the bar on the output. A caption bar dragged three quarters of the way
/// off the screen is a caption bar with most of the sentence missing, and — on
/// a layer surface, which has no title bar and no window menu — no way back.
pub fn clamp_margins(left: i32, bottom: i32, size: (u32, u32), output: (u32, u32)) -> (i32, i32) {
    let max_left = (output.0 as i64 - size.0 as i64).max(0) as i32;
    let max_bottom = (output.1 as i64 - size.1 as i64).max(0) as i32;
    (left.clamp(0, max_left), bottom.clamp(0, max_bottom))
}

/// Where a drag has got to.
///
/// `press` and `now` are surface-local pointer positions, which is all a Wayland
/// client is given — there are no global pointer coordinates on this protocol.
/// That sounds like it should not work, and it does: the delta is applied to the
/// margins, the surface moves under the pointer by exactly that delta, and the
/// next motion event's surface-local position is back at `press` plus however
/// far the hand has moved since. The loop is self-correcting rather than
/// accumulating, which is why `press` is the ORIGINAL press point and not the
/// previous motion.
///
/// Note the sign on the vertical: surface coordinates grow downward and the
/// bottom margin grows upward.
pub fn drag_margins(
    press: (f64, f64),
    now: (f64, f64),
    from: (i32, i32),
    size: (u32, u32),
    output: (u32, u32),
) -> (i32, i32) {
    let dx = (now.0 - press.0).round() as i32;
    let dy = (now.1 - press.1).round() as i32;
    clamp_margins(from.0 + dx, from.1 - dy, size, output)
}

/// The rectangle to remember, in the same global desktop coordinates the
/// Electron window reports — so `bounds` means one thing whichever surface
/// wrote it, and either side can open the bar where the other left it.
///
/// The output's own origin is included because a layer surface's margins are
/// relative to ITS output, and `bounds` is not.
pub fn bounds_from_margins(
    margins: (i32, i32),
    size: (u32, u32),
    output: (u32, u32),
    output_origin: (i32, i32),
) -> Bounds {
    Bounds {
        x: output_origin.0 + margins.0,
        y: output_origin.1 + output.1 as i32 - margins.1 - size.1 as i32,
        width: size.0,
        height: size.1,
    }
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
            bottom_margin(Some(bounds(1004, 1100, 340)), 340, (2560, 1440), (0, 0)),
            96
        );
        assert_eq!(
            bottom_margin(Some(bounds(600, 1100, 340)), 340, (2560, 1440), (0, 0)),
            500
        );
    }

    /// The monitor that is not there today. A `y` off this output must not pin
    /// the bar to an edge it was never at.
    #[test]
    fn a_position_on_some_other_output_falls_back_to_the_default() {
        assert_eq!(
            bottom_margin(Some(bounds(3000, 1100, 340)), 340, (2560, 1440), (0, 0)),
            DEFAULT_BOTTOM_MARGIN
        );
        assert_eq!(
            bottom_margin(Some(bounds(-500, 1100, 340)), 340, (2560, 1440), (0, 0)),
            DEFAULT_BOTTOM_MARGIN
        );
        assert_eq!(
            bottom_margin(None, 340, (2560, 1440), (0, 0)),
            DEFAULT_BOTTOM_MARGIN
        );
    }

    fn at(x: i32, y: i32, w: u32, h: u32) -> Bounds {
        Bounds {
            x,
            y,
            width: w,
            height: h,
        }
    }

    /// With nothing remembered the bar is centred — the same place anchoring to
    /// BOTTOM alone used to put it, now said in arithmetic so a drag can move it.
    #[test]
    fn an_unplaced_bar_is_centred_and_a_remembered_x_replaces_that() {
        assert_eq!(left_margin(None, 1100, (2560, 1440), (0, 0)), 730);
        assert_eq!(
            left_margin(Some(at(120, 0, 1100, 340)), 1100, (2560, 1440), (0, 0)),
            120
        );
        // Off this output: back to centred, never pinned to an edge it was
        // never at.
        assert_eq!(
            left_margin(Some(at(9000, 0, 1100, 340)), 1100, (2560, 1440), (0, 0)),
            730
        );
        assert_eq!(
            left_margin(Some(at(-40, 0, 1100, 340)), 1100, (2560, 1440), (0, 0)),
            730
        );
    }

    /// `bounds` is in global desktop coordinates and margins are relative to one
    /// output. On the second monitor of a stacked pair those are 1440 apart, and
    /// getting it wrong puts the bar on the wrong screen.
    #[test]
    fn a_second_output_is_addressed_through_its_own_origin() {
        let origin = (0, 1440);
        let out = (5120, 1440);
        // A bar 96 off the bottom of the LOWER screen: global y = 1440 + 1440 - 340 - 96.
        let b = at(200, 1440 + 1004, 1100, 340);
        assert_eq!(bottom_margin(Some(b), 340, out, origin), 96);
        assert_eq!(left_margin(Some(b), 1100, out, origin), 200);
        // …and the round trip back out.
        assert_eq!(bounds_from_margins((200, 96), (1100, 340), out, origin), b);
    }

    /// Margins in, bounds out, bounds in, the same margins. If this drifts, the
    /// bar walks up (or down) the screen a few pixels on every launch.
    #[test]
    fn a_dragged_position_survives_the_round_trip_through_the_file() {
        let out = (2560, 1440);
        for margins in [(0, 0), (730, 96), (1460, 1100), (12, 7)] {
            let size = (1100, 340);
            let b = bounds_from_margins(margins, size, out, (0, 0));
            assert_eq!(
                (
                    left_margin(Some(b), size.0, out, (0, 0)),
                    bottom_margin(Some(b), size.1, out, (0, 0))
                ),
                margins,
                "margins {margins:?} did not survive"
            );
        }
    }

    /// The drag itself. Surface-local coordinates, a delta from the press point,
    /// and the vertical sign that is easy to get backwards.
    #[test]
    fn a_drag_moves_the_bar_by_exactly_the_hand() {
        let (size, out) = ((1100u32, 340u32), (2560u32, 1440u32));
        let from = (730, 96);
        // Right and up.
        assert_eq!(
            drag_margins((50.0, 20.0), (90.0, 5.0), from, size, out),
            (770, 111)
        );
        // Left and down.
        assert_eq!(
            drag_margins((50.0, 20.0), (30.0, 60.0), from, size, out),
            (710, 56)
        );
        // Not moved at all.
        assert_eq!(
            drag_margins((50.0, 20.0), (50.0, 20.0), from, size, out),
            from
        );
    }

    /// A layer surface has no title bar and no window menu, so a bar dragged off
    /// the screen is a bar with no way back. It stops at the edge.
    #[test]
    fn the_bar_cannot_be_dragged_off_the_output() {
        let (size, out) = ((1100u32, 340u32), (2560u32, 1440u32));
        assert_eq!(
            drag_margins((0.0, 0.0), (-99999.0, 99999.0), (730, 96), size, out),
            (0, 0)
        );
        assert_eq!(
            drag_margins((0.0, 0.0), (99999.0, -99999.0), (730, 96), size, out),
            (2560 - 1100, 1440 - 340)
        );
        // A surface bigger than the output pins to the corner rather than going
        // negative.
        assert_eq!(clamp_margins(50, 50, (4000, 2000), (2560, 1440)), (0, 0));
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
