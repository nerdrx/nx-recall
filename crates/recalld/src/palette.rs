//! The highlight palette: ten accent colours a person may pin to a voice, and
//! the rules for the emoji that goes with one.
//!
//! # Why tokens and not hex
//!
//! A highlight is read on two grounds (DESIGN §14.1 — NX Clear is light *and*
//! dark) and in three renderers: the desktop views, the desktop caption bar and
//! the headset overlay, which rasterises text itself and has no CSS at all. A
//! free-form `#rrggbb` would let somebody pin `#111111` to a friend and lose
//! the name entirely on the dark ground — and the daemon could not warn them,
//! because the daemon does not know which ground anybody is looking at.
//!
//! So what is stored is a **token name**, and each token is one HUE. Saturation
//! and lightness are the ground's business: the GUI spends `--sp-s`/`--sp-l`
//! (72%/28% light, 72%/74% dark — tokens.css), which are the same two numbers
//! `speakerHue` already renders every unhighlighted voice through, and the
//! overlay hard-codes the dark pair because that surface is always dark. That
//! is the whole legibility argument: a highlight is painted by the same
//! machinery, at the same measured contrast, as the automatic colour it
//! replaces. Nothing about a highlight can make a name unreadable.
//!
//! `hex` is carried alongside for one reason: a swatch in a picker must show
//! the colour as a *thing*, not as this row's rendering of it, and the brand
//! violet has an exact value (#7700FF) that the suite spends elsewhere.
//!
//! Mirrored in `gui/src/renderer/lib/palette.js` and in
//! `crates/nx-recall-overlay/src/palette.rs`, both of which cite this file.
//! Three copies of ten numbers, because the alternative is the overlay
//! depending on the daemon crate (see `feed.rs`'s module note) and the renderer
//! fetching a palette before it can draw a row.

use serde_json::{Value, json};

/// One entry: the token a person's row stores, the hue every surface paints it
/// at, and a canonical hex for swatches.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Accent {
    pub token: &'static str,
    pub hue: u16,
    pub hex: &'static str,
}

/// The ten. Ordered as a picker reads them — around the wheel from the brand
/// violet, which is first because it is the one with a name outside this file.
///
/// Ten is a deliberate ceiling. These are meant to be told apart at a glance in
/// a name column, and past about a dozen hues at fixed saturation the
/// neighbours stop being distinguishable — a palette that cannot be told apart
/// is a palette that marks nobody.
///
/// `rustfmt::skip` because this is a TABLE and rustfmt would give each field a
/// line of its own — sixty lines in which no two hues can be compared at a
/// glance, which is the only way to check that a palette is well spread. It is
/// also the shape `gui/test/palette.test.js` reads: that test parses this file
/// to prove the three copies of the list have not drifted, and one entry per
/// line is what makes parsing it a regex rather than a problem.
#[rustfmt::skip]
pub const PALETTE: &[Accent] = &[
    // The suite's own. #7700FF is hue 268°; painted through the ground's
    // saturation and lightness like every other entry, so "the brand colour"
    // here means the brand HUE, not a literal that would fail on one ground.
    Accent { token: "violet", hue: 268, hex: "#7700ff" },
    Accent { token: "indigo", hue: 232, hex: "#3355ee" },
    Accent { token: "cyan", hue: 192, hex: "#00a5c4" },
    Accent { token: "teal", hue: 168, hex: "#00a487" },
    Accent { token: "green", hue: 140, hex: "#1fa14e" },
    Accent { token: "lime", hue: 92, hex: "#5f9c1a" },
    Accent { token: "amber", hue: 44, hex: "#b8820a" },
    Accent { token: "orange", hue: 22, hex: "#cc6516" },
    Accent { token: "rose", hue: 350, hex: "#d6396b" },
    Accent { token: "magenta", hue: 312, hex: "#b83bc4" },
];

/// The entry for a token, or `None` if this build has never heard of it.
pub fn accent(token: &str) -> Option<&'static Accent> {
    PALETTE.iter().find(|a| a.token == token)
}

/// Is this a token this build paints?
pub fn is_accent(token: &str) -> bool {
    accent(token).is_some()
}

/// The palette as the wire carries it, for `speakers.palette` and for the
/// `palette` block on `status`. A client renders swatches from this rather than
/// from a copy it invented, so a daemon that grows an eleventh colour does not
/// need a matching GUI release to let somebody pick it.
pub fn wire() -> Value {
    json!(
        PALETTE
            .iter()
            .map(|a| json!({"token": a.token, "hue": a.hue, "hex": a.hex}))
            .collect::<Vec<_>>()
    )
}

/// How many grapheme clusters an icon may be.
///
/// Two, not one: a flag is one cluster, so is a family, so is a skin-toned
/// wave — but "🌙✨" is a perfectly reasonable mark for a person and refusing it
/// would be refusing an aesthetic rather than enforcing a limit. Three starts
/// to be a word.
pub const MAX_ICON_CLUSTERS: usize = 2;

/// Count grapheme clusters, well enough for this job and with no new dependency.
///
/// A full UAX-29 segmenter is the right tool and is a crate this daemon does not
/// otherwise need. What an emoji actually is, though, is a narrow shape: base
/// characters joined by ZWJ, followed by variation selectors, skin-tone
/// modifiers, keycaps and regional-indicator pairs. So: a new cluster starts at
/// a character that is none of those continuations, with the two joining rules
/// (a ZWJ swallows the next character; regional indicators pair up) handled
/// explicitly.
///
/// It errs toward counting FEWER clusters than a strict segmenter would, which
/// is the safe direction for a cap: the failure it can produce is accepting a
/// slightly odd two-and-a-bit-glyph icon, not rejecting a legitimate one.
pub fn grapheme_clusters(s: &str) -> usize {
    let mut n = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        // A regional indicator pairs with the next one to make one flag.
        if is_regional_indicator(c) {
            if chars.peek().is_some_and(|&d| is_regional_indicator(d)) {
                chars.next();
            }
            n += 1;
            continue;
        }
        n += 1;
        // Swallow this cluster's continuations, and anything a ZWJ binds on.
        loop {
            match chars.peek() {
                Some(&d) if is_continuation(d) => {
                    chars.next();
                }
                Some(&'\u{200d}') => {
                    chars.next();
                    // The ZWJ binds whatever follows into this same cluster.
                    chars.next();
                }
                _ => break,
            }
        }
    }
    n
}

fn is_regional_indicator(c: char) -> bool {
    ('\u{1f1e6}'..='\u{1f1ff}').contains(&c)
}

/// Variation selectors, skin tones, combining marks, keycaps, tag characters.
fn is_continuation(c: char) -> bool {
    matches!(c,
        '\u{fe00}'..='\u{fe0f}'      // variation selectors
        | '\u{1f3fb}'..='\u{1f3ff}'  // skin-tone modifiers
        | '\u{0300}'..='\u{036f}'    // combining diacriticals
        | '\u{20d0}'..='\u{20ff}'    // combining symbols, incl. the keycap U+20E3
        | '\u{e0020}'..='\u{e007f}'  // tag characters (subdivision flags)
    )
}

/// What went wrong with a proposed highlight, in the daemon's own error words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invalid {
    UnknownColour(String),
    IconTooLong(usize),
    IconHasWhitespace,
    IconHasControls,
}

impl Invalid {
    pub fn message(&self) -> String {
        match self {
            Invalid::UnknownColour(t) => format!(
                "colour must be one of the palette tokens ({}), got {t:?}",
                PALETTE
                    .iter()
                    .map(|a| a.token)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Invalid::IconTooLong(n) => {
                format!("icon must be at most {MAX_ICON_CLUSTERS} characters, got {n}")
            }
            Invalid::IconHasWhitespace => "icon must not contain whitespace".to_owned(),
            Invalid::IconHasControls => "icon must not contain control characters".to_owned(),
        }
    }
}

/// Check a colour token. `None` is "clear it", and clearing is always allowed.
pub fn check_colour(colour: Option<&str>) -> Result<(), Invalid> {
    match colour {
        None => Ok(()),
        Some(t) if is_accent(t) => Ok(()),
        Some(t) => Err(Invalid::UnknownColour(t.to_owned())),
    }
}

/// Check an icon. `None` clears; so does a string that is empty once trimmed —
/// a picker whose field a person emptied means "no icon", and making them press
/// a separate clear button to say the thing they just said is a worse control.
pub fn check_icon(icon: Option<&str>) -> Result<(), Invalid> {
    let Some(icon) = icon else { return Ok(()) };
    if icon.trim().is_empty() {
        return Ok(());
    }
    if icon.chars().any(char::is_whitespace) {
        return Err(Invalid::IconHasWhitespace);
    }
    if icon.chars().any(|c| c.is_control()) {
        return Err(Invalid::IconHasControls);
    }
    let n = grapheme_clusters(icon);
    if n > MAX_ICON_CLUSTERS {
        return Err(Invalid::IconTooLong(n));
    }
    Ok(())
}

/// Normalise an icon on the way into storage: trimmed-empty becomes `None`, so
/// there is exactly one representation of "no icon" in the database and every
/// reader can test it with `IS NULL`.
pub fn normalise_icon(icon: Option<&str>) -> Option<String> {
    icon.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_palette_is_ten_distinct_tokens_and_carries_the_brand() {
        assert_eq!(PALETTE.len(), 10);
        let mut tokens: Vec<&str> = PALETTE.iter().map(|a| a.token).collect();
        tokens.sort_unstable();
        let before = tokens.len();
        tokens.dedup();
        assert_eq!(tokens.len(), before, "two entries share a token");

        // The suite's colour is in the box, and it is the exact one.
        let violet = accent("violet").expect("the brand hue is a palette entry");
        assert_eq!(violet.hex, "#7700ff");

        // Hues are far enough apart to be told apart in a name column.
        let mut hues: Vec<u16> = PALETTE.iter().map(|a| a.hue).collect();
        hues.sort_unstable();
        for pair in hues.windows(2) {
            assert!(
                pair[1] - pair[0] >= 20,
                "hues {} and {} are too close to distinguish",
                pair[0],
                pair[1]
            );
        }
        for a in PALETTE {
            assert!(a.hue < 360, "{} is not a hue", a.token);
            assert_eq!(a.hex.len(), 7, "{} needs a #rrggbb swatch", a.token);
        }
    }

    #[test]
    fn an_unknown_token_is_refused_and_none_clears() {
        assert!(check_colour(None).is_ok());
        assert!(check_colour(Some("violet")).is_ok());
        assert_eq!(
            check_colour(Some("#ff0000")),
            Err(Invalid::UnknownColour("#ff0000".into()))
        );
        // Case matters: the token is an identifier, not a label.
        assert!(check_colour(Some("Violet")).is_err());
        assert!(check_colour(Some("")).is_err());
    }

    #[test]
    fn one_emoji_is_one_cluster_however_many_code_points_it_takes() {
        assert_eq!(grapheme_clusters(""), 0);
        assert_eq!(grapheme_clusters("a"), 1);
        assert_eq!(grapheme_clusters("\u{2b50}"), 1); // ★ plain
        assert_eq!(grapheme_clusters("\u{2764}\u{fe0f}"), 1); // heart + VS16
        assert_eq!(grapheme_clusters("\u{1f44b}\u{1f3fd}"), 1); // wave + skin tone
        assert_eq!(grapheme_clusters("\u{1f1e9}\u{1f1ea}"), 1); // 🇩🇪
        assert_eq!(grapheme_clusters("\u{1f1e9}\u{1f1ea}\u{1f1eb}\u{1f1f7}"), 2);
        // 👩‍🚀 — woman + ZWJ + rocket is one person, not two glyphs.
        assert_eq!(grapheme_clusters("\u{1f469}\u{200d}\u{1f680}"), 1);
        assert_eq!(grapheme_clusters("\u{1f319}\u{2728}"), 2); // 🌙✨
        assert_eq!(grapheme_clusters("abc"), 3);
    }

    #[test]
    fn the_icon_cap_is_two_clusters_and_whitespace_is_not_an_icon() {
        assert!(check_icon(None).is_ok());
        assert!(check_icon(Some("")).is_ok(), "empty clears");
        assert!(check_icon(Some("   ")).is_ok(), "blank clears");
        assert!(check_icon(Some("\u{1f319}")).is_ok());
        assert!(check_icon(Some("\u{1f319}\u{2728}")).is_ok());
        assert_eq!(
            check_icon(Some("\u{1f319}\u{2728}\u{2b50}")),
            Err(Invalid::IconTooLong(3))
        );
        // A name is not an icon.
        assert!(matches!(
            check_icon(Some("Kira")),
            Err(Invalid::IconTooLong(4))
        ));
        // Two glyphs with a space between them is two things, not one mark.
        assert_eq!(
            check_icon(Some("\u{1f319} \u{2728}")),
            Err(Invalid::IconHasWhitespace)
        );
        assert_eq!(check_icon(Some("a\u{0007}")), Err(Invalid::IconHasControls));
    }

    #[test]
    fn there_is_one_way_to_store_no_icon() {
        assert_eq!(normalise_icon(None), None);
        assert_eq!(normalise_icon(Some("")), None);
        assert_eq!(normalise_icon(Some("  ")), None);
        assert_eq!(
            normalise_icon(Some("\u{1f319}")),
            Some("\u{1f319}".to_owned())
        );
    }

    #[test]
    fn the_wire_palette_is_what_a_picker_needs() {
        let v = wire();
        let rows = v.as_array().expect("an array");
        assert_eq!(rows.len(), PALETTE.len());
        assert_eq!(rows[0]["token"], "violet");
        assert_eq!(rows[0]["hex"], "#7700ff");
        assert_eq!(rows[0]["hue"], 268);
    }
}
