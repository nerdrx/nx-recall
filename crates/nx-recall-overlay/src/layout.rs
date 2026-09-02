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

// ---------------------------------------------------------------------------
// crossing between screens
//
// A layer surface belongs to ONE wl_output. That is not a limitation of this
// code, it is the protocol: `zwlr_layer_shell_v1.get_layer_surface` takes an
// output and the surface lives on it until it is destroyed. So a bar dragged
// to the edge of DP-2 stops there, which is exactly what "I CANT MOVE THE LIVE
// CAPTION BETWEEN SCREENS" describes.
//
// The way across is to notice that the bar's centre has entered a DIFFERENT
// output's rectangle and re-create the surface there at the same place on the
// desk. Everything needed to decide that is arithmetic over the outputs'
// logical geometry, so it lives here where a machine with no compositor can
// check it.
// ---------------------------------------------------------------------------

/// One output, as the placement math needs it: a name and a rectangle in the
/// global desktop coordinates `bounds` is written in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    pub name: String,
    pub origin: (i32, i32),
    pub size: (u32, u32),
    pub scale: i32,
}

impl Screen {
    pub fn contains(&self, p: (i32, i32)) -> bool {
        p.0 >= self.origin.0
            && p.1 >= self.origin.1
            && p.0 < self.origin.0 + self.size.0 as i32
            && p.1 < self.origin.1 + self.size.1 as i32
    }

    /// `(left, bottom)` margins that put a global rectangle here.
    ///
    /// Not clamped. While a bar is being carried across a seam it genuinely
    /// hangs off the edge of one screen, and clamping mid-drag is what would
    /// stop it ever reaching the other one.
    pub fn margins_for(&self, r: Bounds) -> (i32, i32) {
        (
            r.x - self.origin.0,
            self.origin.1 + self.size.1 as i32 - r.y - r.height as i32,
        )
    }

    /// The global rectangle a pair of margins describes here — the inverse.
    pub fn bounds_for(&self, margins: (i32, i32), size: (u32, u32)) -> Bounds {
        Bounds {
            x: self.origin.0 + margins.0,
            y: self.origin.1 + self.size.1 as i32 - margins.1 - size.1 as i32,
            width: size.0,
            height: size.1,
        }
    }
}

/// Which screen a global point is on, if any.
pub fn screen_at(screens: &[Screen], p: (i32, i32)) -> Option<usize> {
    screens.iter().position(|s| s.contains(p))
}

/// Which screen a name belongs to.
pub fn screen_named(screens: &[Screen], name: &str) -> Option<usize> {
    screens.iter().position(|s| s.name == name)
}

/// What a drag has just asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DragStep {
    /// Nowhere valid. The bar keeps the margins it had — a caption bar dropped
    /// in the gap between two monitors of different heights would be on no
    /// screen at all, and there is no way back to it.
    Nowhere,
    /// Still this screen. Move the margins.
    Stay { margins: (i32, i32) },
    /// Another screen. The surface has to be destroyed and made again there,
    /// because a layer surface cannot change output.
    Hop { screen: usize, margins: (i32, i32) },
}

/// Where a dragged bar has got to, across the whole desk.
///
/// The bar's CENTRE decides which screen it is on, not its corner: dragging by
/// the left edge would otherwise hop the moment one pixel crossed the seam,
/// while the thing you are looking at is still entirely on the old screen.
pub fn drag_step(screens: &[Screen], current: usize, rect: Bounds) -> DragStep {
    let centre = (
        rect.x + rect.width as i32 / 2,
        rect.y + rect.height as i32 / 2,
    );
    match screen_at(screens, centre) {
        None => DragStep::Nowhere,
        Some(i) if i == current => DragStep::Stay {
            margins: screens[i].margins_for(rect),
        },
        Some(i) => DragStep::Hop {
            screen: i,
            margins: screens[i].margins_for(rect),
        },
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
        assert_eq!(
            Screen {
                name: "DP-1".into(),
                origin,
                size: out,
                scale: 1
            }
            .bounds_for((200, 96), (1100, 340)),
            b
        );
    }

    // -- crossing between screens ------------------------------------------

    fn screen(name: &str, x: i32, y: i32, w: u32, h: u32) -> Screen {
        Screen {
            name: name.into(),
            origin: (x, y),
            size: (w, h),
            scale: 1,
        }
    }

    /// The user's own desk: two 5120x1440 ultrawides, stacked.
    fn stacked() -> Vec<Screen> {
        vec![
            screen("DP-2", 0, 0, 5120, 1440),
            screen("DP-1", 0, 1440, 5120, 1440),
        ]
    }

    /// The other common shape, and the one where an x offset rather than a y
    /// offset is what the margin math has to get right.
    fn side_by_side() -> Vec<Screen> {
        vec![
            screen("HDMI-A-1", 0, 0, 1920, 1080),
            screen("DP-3", 1920, 0, 2560, 1440),
        ]
    }

    #[test]
    fn a_global_point_finds_its_screen_and_the_gaps_find_none() {
        let s = stacked();
        assert_eq!(screen_at(&s, (10, 10)), Some(0));
        assert_eq!(
            screen_at(&s, (10, 1440)),
            Some(1),
            "the seam belongs to the lower screen"
        );
        assert_eq!(screen_at(&s, (10, 1439)), Some(0));
        assert_eq!(screen_at(&s, (10, 2879)), Some(1));
        assert_eq!(
            screen_at(&s, (10, 2880)),
            None,
            "past the bottom of the desk"
        );
        assert_eq!(screen_at(&s, (-1, 10)), None);
        // Side by side, with a taller screen on the right: the region beside
        // the short one is desk that belongs to nobody.
        let t = side_by_side();
        assert_eq!(screen_at(&t, (100, 100)), Some(0));
        assert_eq!(screen_at(&t, (2000, 100)), Some(1));
        assert_eq!(screen_at(&t, (100, 1200)), None, "below the short screen");
        assert_eq!(screen_named(&t, "DP-3"), Some(1));
        assert_eq!(screen_named(&t, "DP-9"), None);
    }

    /// Margins and global rectangles are the same fact said two ways, and the
    /// conversion has to survive a round trip on every screen — otherwise the
    /// bar walks a little further off every time it is picked up.
    #[test]
    fn margins_and_bounds_round_trip_on_every_screen() {
        for screens in [stacked(), side_by_side()] {
            for s in &screens {
                for margins in [(0, 0), (40, 96), (200, 500)] {
                    let size = (900, 260);
                    let r = s.bounds_for(margins, size);
                    assert_eq!(s.margins_for(r), margins, "{} {margins:?}", s.name);
                }
            }
        }
    }

    /// The hop itself, on the desk the report came from. A bar near the bottom
    /// of the upper screen, dragged down, lands on the lower one at the SAME
    /// place on the desk — which is the whole point: it must not jump.
    #[test]
    fn dragging_past_the_seam_moves_the_bar_to_the_other_screen() {
        let s = stacked();
        let size = (1100u32, 340u32);
        // Sitting on DP-2, 96 up from its bottom: global y = 1440-340-96 = 1004.
        let here = s[0].bounds_for((2010, 96), size);
        assert_eq!(here.y, 1004);
        assert_eq!(
            drag_step(&s, 0, here),
            DragStep::Stay {
                margins: (2010, 96)
            }
        );

        // Dragged 300 px down. The centre (y = 1304 + 170 = 1474) is now on
        // DP-1, so the surface has to be re-made there.
        let moved = Bounds {
            y: here.y + 300,
            ..here
        };
        let step = drag_step(&s, 0, moved);
        let DragStep::Hop { screen, margins } = step else {
            panic!("no hop: {step:?}");
        };
        assert_eq!(screen, 1);
        // Same place on the desk, expressed against the new screen: it is
        // 1304 - 1440 = -136 into DP-1, so 1440 - (-136) - 340 = 1236 up from
        // DP-1's bottom, and it straddles the seam exactly as it looks.
        assert_eq!(margins, (2010, 1236));
        assert_eq!(
            s[1].bounds_for(margins, size),
            moved,
            "the bar moved when it hopped"
        );
    }

    /// Side by side, where the x offset is what has to be undone.
    #[test]
    fn the_hop_works_sideways_too() {
        let s = side_by_side();
        let size = (900u32, 260u32);
        let here = s[0].bounds_for((100, 96), size);
        let moved = Bounds {
            x: here.x + 1600,
            ..here
        };
        let step = drag_step(&s, 0, moved);
        let DragStep::Hop { screen, margins } = step else {
            panic!("no hop: {step:?}");
        };
        assert_eq!(screen, 1);
        assert_eq!(margins.0, moved.x - 1920);
        assert_eq!(s[1].bounds_for(margins, size), moved);
    }

    /// A centre on no screen at all — the dead region beside a shorter monitor,
    /// or past the edge of the desk. The bar stays where it was: there is no
    /// title bar to drag it back by.
    #[test]
    fn a_bar_carried_into_nothing_stays_where_it_was() {
        let s = side_by_side();
        let size = (900u32, 260u32);
        let here = s[0].bounds_for((100, 96), size);
        let into_the_void = Bounds {
            y: here.y + 900,
            ..here
        };
        assert_eq!(drag_step(&s, 0, into_the_void), DragStep::Nowhere);
        // …and off the left of the desk entirely.
        assert_eq!(
            drag_step(&s, 0, Bounds { x: -5000, ..here }),
            DragStep::Nowhere
        );
    }

    /// One screen is the ordinary case and must never produce a hop.
    #[test]
    fn a_single_screen_desk_never_hops() {
        let s = vec![screen("eDP-1", 0, 0, 1920, 1080)];
        let size = (900u32, 260u32);
        let here = s[0].bounds_for((100, 96), size);
        for dx in [-400, 0, 400] {
            let step = drag_step(
                &s,
                0,
                Bounds {
                    x: here.x + dx,
                    ..here
                },
            );
            assert!(
                matches!(step, DragStep::Stay { .. } | DragStep::Nowhere),
                "{step:?}"
            );
        }
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
