//! `captions.json` — the same file, read by the other half of the feature.
//!
//! The Electron settings card is the single control surface for captions, and
//! it writes exactly one file: `captions.json` in the app's userData directory.
//! When the desktop captions are a layer surface rather than a BrowserWindow,
//! nothing about that changes — this process READS that file and re-reads it
//! whenever it is written, so a slider dragged in the Sources view still moves
//! the bar on screen. There is deliberately no second settings file, no CLI
//! flag that shadows one of these fields, and no way for this process to write
//! back: two writers on one file is how a settings file ends up disagreeing
//! with the UI that owns it.
//!
//! The clamping below is a transliteration of `normalizeCaptionSettings` in
//! gui/src/renderer/lib/captions.js, ranges included, and for the same reason
//! that function exists: the file lives in a profile a person can hand-edit, and
//! a `size` of 4000 in it must produce the default rather than one unreadable
//! word and no control able to get back out.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Everything the captions bar is, as the file states it.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptionSettings {
    pub turns: usize,
    pub size: f32,
    pub hold_s: f32,
    pub opacity: f32,
    pub show_you: bool,
    /// Read, kept, and — on this path — ignored: a layer surface with an empty
    /// input region is click-through and cannot be anything else. Kept in the
    /// struct so the value is never silently rewritten, and so the settings card
    /// on the Electron fallback keeps meaning what it says.
    pub click_through: bool,
    /// The remembered window rectangle. Only its SIZE is honoured here — see
    /// `docs/OVERLAY.md`, "Desktop: layer-shell".
    pub bounds: Option<Bounds>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Default for CaptionSettings {
    /// CAPTION_DEFAULTS, verbatim.
    fn default() -> Self {
        Self {
            turns: 5,
            size: 26.0,
            hold_s: 12.0,
            opacity: 0.6,
            show_you: true,
            click_through: true,
            bounds: None,
        }
    }
}

/// CAPTION_RANGES, verbatim: `(min, max, step)`.
const TURNS: (f64, f64, f64) = (3.0, 8.0, 1.0);
const SIZE: (f64, f64, f64) = (18.0, 40.0, 1.0);
const HOLD_S: (f64, f64, f64) = (4.0, 60.0, 1.0);
const OPACITY: (f64, f64, f64) = (0.3, 0.9, 0.05);

/// One number, clamped the way the renderer clamps it.
///
/// `null` and `""` are ABSENT, not zero — the same trap the JS guards: a field
/// an older build wrote as null would otherwise become the minimum rather than
/// the default.
fn clamp(v: Option<&Value>, (min, max, step): (f64, f64, f64), dflt: f64) -> f64 {
    let n = match v {
        None | Some(Value::Null) => return dflt,
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) if s.trim().is_empty() => return dflt,
        Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
        Some(Value::Bool(_)) | Some(Value::Array(_)) | Some(Value::Object(_)) => None,
    };
    let Some(n) = n.filter(|n| n.is_finite()) else {
        return dflt;
    };
    let stepped = (n / step).round() * step;
    let clamped = stepped.clamp(min, max);
    // Rounded back to the step's own precision, so 0.30000000000000004 and 0.3
    // are one opacity rather than two strings.
    let places = format!("{step}")
        .split_once('.')
        .map(|(_, frac)| frac.len())
        .unwrap_or(0) as i32;
    let scale = 10f64.powi(places);
    (clamped * scale).round() / scale
}

/// `src.x === undefined ? default : !!src.x`.
///
/// The asymmetry with `clamp` is the JS's, not a slip: for a NUMBER, `null`
/// means absent and takes the default; for a FLAG, `!!null` is `false` and a
/// file that says `null` is a file that says off. Matching it matters because
/// the two halves of this feature read the same file.
fn flag(v: Option<&Value>, dflt: bool) -> bool {
    match v {
        None => dflt,
        Some(present) => truthy(present),
    }
}

/// JavaScript's `!!x` for the shapes that reach a settings file.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

impl CaptionSettings {
    /// A settings block from anywhere, folded onto the defaults.
    pub fn normalize(raw: &Value) -> Self {
        let get = |k: &str| raw.get(k);
        let d = Self::default();
        Self {
            turns: clamp(get("turns"), TURNS, d.turns as f64) as usize,
            size: clamp(get("size"), SIZE, d.size as f64) as f32,
            hold_s: clamp(get("hold_s"), HOLD_S, d.hold_s as f64) as f32,
            opacity: clamp(get("opacity"), OPACITY, d.opacity as f64) as f32,
            show_you: flag(get("showYou"), d.show_you),
            click_through: flag(get("clickThrough"), d.click_through),
            bounds: bounds_of(get("bounds")),
        }
    }

    /// Read the file, or the defaults if it is missing, unreadable, or not JSON
    /// any more. Never an error: a caption bar that refuses to start because a
    /// profile file has a stray comma in it is worse than one at 26 px.
    pub fn load(path: &Path) -> Self {
        let raw = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .unwrap_or(Value::Null);
        Self::normalize(&raw)
    }
}

/// `normalizeBounds`, verbatim: a rectangle no caption bar could use is a
/// corrupt file, not a monitor, and is refused.
fn bounds_of(b: Option<&Value>) -> Option<Bounds> {
    let b = b?;
    if !b.is_object() {
        return None;
    }
    let n = |k: &str| b.get(k)?.as_f64().filter(|v| v.is_finite()).map(f64::round);
    let (x, y, width, height) = (n("x")?, n("y")?, n("width")?, n("height")?);
    if width < 240.0 || height < 90.0 {
        return None;
    }
    Some(Bounds {
        x: x as i32,
        y: y as i32,
        width: width as u32,
        height: height as u32,
    })
}

/// Where Electron put the file, when nobody passed `--settings`.
///
/// `app.getPath('userData')` on Linux is `$XDG_CONFIG_HOME/<app name>`, and the
/// app name Electron uses is `productName` from gui/package.json — "NX Recall",
/// space and capitals included. It is spelled out here rather than guessed at,
/// because the ordinary way this binary starts is with `--settings` handed to it
/// by the main process that already knows the answer; this fallback is for
/// somebody running it by hand.
pub fn default_settings_path() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    config.join("NX Recall").join("captions.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_absent_file_is_the_defaults() {
        let s = CaptionSettings::load(Path::new("/nonexistent/captions.json"));
        assert_eq!(s, CaptionSettings::default());
        // …and so is a file that stopped being JSON.
        assert_eq!(
            CaptionSettings::normalize(&json!("{{ not json")),
            CaptionSettings::default()
        );
    }

    /// The trap the JS calls out by name: `null` is ABSENT, not zero, and a
    /// field an older build wrote as null must come back as the default rather
    /// than the bottom of the range.
    #[test]
    fn null_is_absent_and_not_the_minimum() {
        let s = CaptionSettings::normalize(&json!({"size": null, "turns": null, "opacity": null}));
        assert_eq!(s.size, 26.0);
        assert_eq!(s.turns, 5);
        assert_eq!(s.opacity, 0.6);
    }

    #[test]
    fn a_hand_edited_file_cannot_produce_a_bar_no_control_could_undo() {
        let s = CaptionSettings::normalize(&json!({
            "size": 4000, "turns": 99, "hold_s": -5, "opacity": 12
        }));
        assert_eq!(s.size, 40.0);
        assert_eq!(s.turns, 8);
        assert_eq!(s.hold_s, 4.0);
        assert_eq!(s.opacity, 0.9);
    }

    /// Rounded to the step's own precision. 0.65 must not arrive as
    /// 0.6500000000000001, and a value between steps lands on one.
    #[test]
    fn opacity_lands_on_a_step_the_slider_can_produce() {
        assert_eq!(
            CaptionSettings::normalize(&json!({"opacity": 0.63})).opacity,
            0.65
        );
        assert_eq!(
            CaptionSettings::normalize(&json!({"opacity": 0.7})).opacity,
            0.7
        );
    }

    #[test]
    fn the_flags_default_on_and_are_read_as_javascript_reads_them() {
        let d = CaptionSettings::normalize(&json!({}));
        assert!(d.show_you && d.click_through);
        assert!(!CaptionSettings::normalize(&json!({"showYou": false})).show_you);
        assert!(!CaptionSettings::normalize(&json!({"clickThrough": false})).click_through);
    }

    #[test]
    fn a_rectangle_no_caption_bar_could_use_is_refused() {
        assert_eq!(
            CaptionSettings::normalize(&json!({"bounds": null})).bounds,
            None
        );
        assert_eq!(
            CaptionSettings::normalize(
                &json!({"bounds": {"x": 0, "y": 0, "width": 100, "height": 100}})
            )
            .bounds,
            None,
            "a 100px-wide bar was accepted"
        );
        assert_eq!(
            CaptionSettings::normalize(
                &json!({"bounds": {"x": 12, "y": 34, "width": 1100, "height": 340}})
            )
            .bounds,
            Some(Bounds {
                x: 12,
                y: 34,
                width: 1100,
                height: 340
            })
        );
    }

    /// The path the Electron side actually writes to, for the run where nobody
    /// passed `--settings`.
    #[test]
    fn the_default_path_is_electrons_own() {
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/tmp/does-not-matter") };
        assert_eq!(
            default_settings_path(),
            PathBuf::from("/tmp/does-not-matter/NX Recall/captions.json")
        );
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }
}
