//! The highlight palette, headset side.
//!
//! A mirror of `crates/recalld/src/palette.rs` — the same ten tokens and the
//! same ten hues — kept as a copy rather than an import for the reason
//! `feed.rs` gives about `PROTO`: this binary does not depend on `recalld`, and
//! making it do so to share thirty numbers would drag an ONNX runtime and a
//! PipeWire link into a process whose whole job is to draw a bar.
//!
//! Only the hue crosses over. Saturation and lightness are 72%/74% here,
//! hard-coded exactly as `speaker_hue` hard-codes them, because this surface is
//! always dark (see `raster.rs`'s module note). That is what makes a highlight
//! obey the same contrast the automatic colour was measured at: the highlight
//! changes *which* hue a name wears, never how legible it is.

/// Token → hue, in the daemon's order. See the note above before editing:
/// these must equal `recalld`'s `PALETTE`, and `palette_matches_the_daemon` in
/// `gui/test/palette.test.js` checks the third copy against the same list.
const HUES: &[(&str, f32)] = &[
    ("violet", 268.0),
    ("indigo", 232.0),
    ("cyan", 192.0),
    ("teal", 168.0),
    ("green", 140.0),
    ("lime", 92.0),
    ("amber", 44.0),
    ("orange", 22.0),
    ("rose", 350.0),
    ("magenta", 312.0),
];

/// The hue for a palette token, or `None` for a token this build does not know.
///
/// Unknown is not an error and must not be: a daemon newer than this overlay
/// may name an eleventh colour, and the right answer then is the voice's
/// ordinary hashed hue — a name in the wrong colour, not a name that fails to
/// draw.
pub fn hue(token: &str) -> Option<f32> {
    HUES.iter().find(|(t, _)| *t == token).map(|(_, h)| *h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_token_resolves_and_an_unknown_one_does_not() {
        assert_eq!(HUES.len(), 10);
        assert_eq!(hue("violet"), Some(268.0));
        assert_eq!(hue("magenta"), Some(312.0));
        // A colour from a newer daemon falls back rather than failing.
        assert_eq!(hue("chartreuse"), None);
        assert_eq!(hue(""), None);
    }
}
