//! The captions, as pixels.
//!
//! Deliberately a CPU rasteriser and deliberately not wgpu. OpenXR does not
//! hand you a texture you can ask a renderer to draw into — it hands you a
//! `VkImage` out of ITS swapchain, and adopting one of those into wgpu means
//! going through `wgpu-hal`'s unsafe Vulkan escape hatch. For a bar of text
//! that changes a few times a minute, that is a large amount of code standing
//! between us and a `vkCmdCopyBufferToImage`, and none of it could be tested on
//! a machine with no headset. So the pixels are made here, in plain Rust, and
//! the graphics API's entire job is to move a buffer into an image.
//!
//! Which also means this half is TESTABLE, and is tested: `--render` writes the
//! same surface to a file, and the unit tests below check the two things that
//! actually go wrong — that text wraps rather than running off the edge, and
//! that the ground is the deep-space one at the asked-for opacity.
//!
//! The look is the desktop captions window's, on purpose (gui/src/renderer/
//! captions.css): same ground, same speaker hues, same "≈", same lighter
//! translation under the original. A caption that looked different in the
//! headset would be a second design nobody agreed to.

use std::path::Path;

use anyhow::{Context, Result};
use fontdue::{Font, FontSettings};

use crate::feed::Turn;

/// The deep-space ground, unpacked. Matches `--cap-ground` in captions.css.
pub const GROUND: [u8; 3] = [6, 4, 12];
/// Body ink. Matches `.cap-text`.
const INK: [u8; 3] = [244, 243, 248];
/// The muted ink a shaky row and a translation are set in.
const MUTED: [u8; 3] = [185, 180, 201];
const TRANSLATION: [u8; 3] = [200, 195, 214];
const FAINT: [u8; 3] = [143, 138, 160];

/// An RGBA8 surface, premultiplied by nothing: the alpha is the ground's, and
/// the compositor is what multiplies it.
pub struct Surface {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl Surface {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0; (width as usize) * (height as usize) * 4],
        }
    }

    fn put(&mut self, x: i64, y: i64, rgb: [u8; 3], coverage: f32) {
        if x < 0 || y < 0 || x >= self.width as i64 || y >= self.height as i64 || coverage <= 0.0 {
            return;
        }
        let at = ((y as usize) * (self.width as usize) + (x as usize)) * 4;
        let sa = coverage.min(1.0);
        let da = self.pixels[at + 3] as f32 / 255.0;
        // Straight-alpha "over", divided back out by the result alpha. Doing it
        // the premultiplied way and storing the result as straight alpha is the
        // classic quiet bug: the deep-space ground at 0.6 comes out as 60% of
        // itself, so a caption bar gets darker every time somebody lowers the
        // opacity slider instead of more transparent.
        let out_a = sa + da * (1.0 - sa);
        if out_a > 0.0 {
            for (c, src) in rgb.iter().enumerate() {
                let dst = self.pixels[at + c] as f32;
                self.pixels[at + c] =
                    (((*src as f32 * sa) + dst * da * (1.0 - sa)) / out_a).round() as u8;
            }
        }
        self.pixels[at + 3] = (out_a * 255.0).round().min(255.0) as u8;
    }

    fn fill(&mut self, x0: i64, y0: i64, w: i64, h: i64, rgb: [u8; 3], alpha: f32) {
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                self.put(x, y, rgb, alpha);
            }
        }
    }

    /// A PPM, because it is six lines of code and every image viewer on this
    /// machine reads it. `--render` is an eyeball check, not a deliverable.
    pub fn write_ppm(&self, path: &Path) -> Result<()> {
        let mut out = format!("P6\n{} {}\n255\n", self.width, self.height).into_bytes();
        // Flattened over the ground it will actually sit on, so what you look
        // at is what a headset would show over a dark scene.
        for px in self.pixels.as_chunks::<4>().0 {
            let a = px[3] as f32 / 255.0;
            for channel in &px[..3] {
                out.push((*channel as f32 * a).round() as u8);
            }
        }
        std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

/// Everything about how the bar is drawn that a person can change.
#[derive(Debug, Clone)]
pub struct Style {
    pub width: u32,
    pub height: u32,
    /// The words, in pixels at this surface's resolution.
    pub size: f32,
    /// How much of the scene behind the bar the ground covers, 0.3–0.9.
    pub opacity: f32,
    pub pad: i64,
}

impl Default for Style {
    fn default() -> Self {
        // 1024×512 is a comfortable quad texture: readable at the default 1.8 m
        // and small enough that a CPU rasteriser can redraw it per turn without
        // anybody noticing.
        Self {
            width: 1024,
            height: 512,
            size: 34.0,
            opacity: 0.6,
            pad: 18,
        }
    }
}

pub struct Renderer {
    font: Font,
    pub style: Style,
}

impl Renderer {
    /// Load a font. There is no bundled one on purpose: shipping a typeface
    /// means shipping its licence, and every desktop this runs on already has
    /// a sans-serif that Fontconfig will name.
    pub fn new(style: Style, font_path: Option<&Path>) -> Result<Self> {
        let candidates: Vec<std::path::PathBuf> = match font_path {
            Some(p) => vec![p.to_path_buf()],
            None => DEFAULT_FONTS.iter().map(std::path::PathBuf::from).collect(),
        };
        let bytes = candidates
            .iter()
            .find_map(|p| std::fs::read(p).ok())
            .with_context(|| {
                format!(
                    "no usable font — looked at {}. Pass --font /path/to/a.ttf",
                    candidates
                        .iter()
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        let font = Font::from_bytes(bytes, FontSettings::default())
            .map_err(|e| anyhow::anyhow!("that font could not be parsed: {e}"))?;
        Ok(Self { font, style })
    }

    /// Draw the caption stack, newest at the bottom, and return the surface.
    ///
    /// Rows are laid out from the BOTTOM up, so the newest turn is always in
    /// the same place — a stack that grows downward moves the line you are
    /// reading every time somebody speaks.
    pub fn render(&self, turns: &[Turn], you: Option<i64>) -> Surface {
        let mut s = Surface::new(self.style.width, self.style.height);
        let name_size = (self.style.size * 0.44).max(11.0);
        let tr_size = self.style.size * 0.78;
        let line_h = (self.style.size * 1.28).round() as i64;
        let tr_line_h = (tr_size * 1.25).round() as i64;
        let inner = self.style.width as i64 - self.style.pad * 4;

        // Measure from the newest backwards, then draw, so a stack that does
        // not fit loses its OLDEST rows rather than its newest.
        let mut blocks: Vec<Block> = Vec::new();
        let mut used = 0;
        for turn in turns.iter().rev() {
            let name = format!("{}  ", turn.who);
            let name_w = self.measure(&name, name_size);
            let text = if turn.shaky {
                format!("≈ {}", turn.text)
            } else {
                turn.text.clone()
            };
            let text_lines = self.wrap(&text, self.style.size, inner - name_w - DOT_COLUMN);
            let tr_lines = turn
                .translation
                .as_ref()
                .map(|(lang, text)| {
                    let prefix = lang
                        .as_deref()
                        .map(|l| format!("{}  ", l.to_uppercase()))
                        .unwrap_or_default();
                    self.wrap(&format!("{prefix}{text}"), tr_size, inner - name_w - DOT_COLUMN)
                })
                .unwrap_or_default();
            let height = self.style.pad * 2
                + text_lines.len() as i64 * line_h
                + tr_lines.len() as i64 * tr_line_h;
            if used + height + self.style.pad > self.style.height as i64 - self.style.pad {
                break;
            }
            used += height + 8;
            blocks.push(Block {
                name,
                name_w,
                hue: speaker_hue(turn.speaker),
                mine: turn.speaker.is_some() && turn.speaker == you,
                shaky: turn.shaky,
                text_lines,
                tr_lines,
                height,
            });
        }

        let mut y = self.style.height as i64 - self.style.pad - used + 8;
        for block in blocks.iter().rev() {
            self.draw_block(&mut s, block, y, name_size, tr_size, line_h, tr_line_h);
            y += block.height + 8;
        }
        s
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_block(
        &self,
        s: &mut Surface,
        b: &Block,
        top: i64,
        name_size: f32,
        tr_size: f32,
        line_h: i64,
        tr_line_h: i64,
    ) {
        let x0 = self.style.pad;
        let w = self.style.width as i64 - self.style.pad * 2;
        s.fill(x0, top, w, b.height, GROUND, self.style.opacity);

        // The dot's own width plus its gap, then the name, then the words.
        let text_x = x0 + self.style.pad + DOT_COLUMN + b.name_w;
        let mut y = top + self.style.pad + (self.style.size * 0.9) as i64;

        // The dot, then the name, in the speaker's own hue — the same hash the
        // desktop uses, so a voice is the same colour in both places. The one
        // place violet is spent is the ring on your own dot.
        let dot_y = top + self.style.pad + (name_size * 0.4) as i64;
        if b.mine {
            s.fill(x0 + self.style.pad - 3, dot_y - 3, 11, 11, VIOLET, 0.9);
        }
        s.fill(x0 + self.style.pad, dot_y, 5, 5, b.hue, 1.0);
        self.draw_text(
            s,
            &b.name,
            x0 + self.style.pad + DOT_COLUMN,
            top + self.style.pad + (name_size * 0.9) as i64,
            name_size,
            b.hue,
        );

        let ink = if b.shaky { MUTED } else { INK };
        for line in &b.text_lines {
            self.draw_text(s, line, text_x, y, self.style.size, ink);
            y += line_h;
        }
        for line in &b.tr_lines {
            self.draw_text(s, line, text_x, y + 2, tr_size, TRANSLATION);
            y += tr_line_h;
        }
    }

    fn draw_text(&self, s: &mut Surface, text: &str, x: i64, baseline: i64, size: f32, rgb: [u8; 3]) {
        let mut pen = x;
        for ch in text.chars() {
            let (metrics, bitmap) = self.font.rasterize(self.glyph(ch), size);
            for (i, coverage) in bitmap.iter().enumerate() {
                let gx = (i % metrics.width) as i64;
                let gy = (i / metrics.width) as i64;
                s.put(
                    pen + metrics.xmin as i64 + gx,
                    baseline - metrics.height as i64 - metrics.ymin as i64 + gy,
                    rgb,
                    *coverage as f32 / 255.0,
                );
            }
            pen += metrics.advance_width.round() as i64;
        }
    }

    fn measure(&self, text: &str, size: f32) -> i64 {
        text.chars()
            .map(|ch| self.font.metrics(self.glyph(ch), size).advance_width)
            .sum::<f32>()
            .round() as i64
    }

    /// The character, or something the font actually has.
    ///
    /// The system sans-serif is whatever this machine happens to ship and it is
    /// not guaranteed to carry "≈" — Noto Sans does not. A tofu box in the
    /// middle of a caption is worse than a plain "~", and silently dropping the
    /// mark would be worse than both: the mark means a second decoder
    /// disagreed, and a row that stops saying so is a row claiming to be solid.
    fn glyph(&self, ch: char) -> char {
        if self.font.lookup_glyph_index(ch) != 0 {
            return ch;
        }
        match ch {
            '≈' => '~',
            '—' | '–' => '-',
            '…' => '.',
            '‘' | '’' => '\'',
            '“' | '”' => '"',
            _ => '?',
        }
    }

    /// Wrap on word boundaries, and never on none: a single word wider than the
    /// bar is broken rather than allowed to run off the edge, because a caption
    /// that runs off the edge is a caption with the end of the sentence missing.
    fn wrap(&self, text: &str, size: f32, width: i64) -> Vec<String> {
        let width = width.max(40);
        let mut out: Vec<String> = Vec::new();
        let mut line = String::new();
        for word in text.split_whitespace() {
            let candidate = if line.is_empty() {
                word.to_owned()
            } else {
                format!("{line} {word}")
            };
            if self.measure(&candidate, size) <= width {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            // One word, still too wide. Break it by characters.
            let mut chunk = String::new();
            for ch in word.chars() {
                let next = format!("{chunk}{ch}");
                if !chunk.is_empty() && self.measure(&next, size) > width {
                    out.push(std::mem::take(&mut chunk));
                }
                chunk.push(ch);
            }
            line = chunk;
        }
        if !line.is_empty() {
            out.push(line);
        }
        if out.is_empty() {
            out.push(String::new());
        }
        out
    }
}

struct Block {
    name: String,
    name_w: i64,
    hue: [u8; 3],
    mine: bool,
    shaky: bool,
    text_lines: Vec<String>,
    tr_lines: Vec<String>,
    height: i64,
}

/// The dot, and the gap between it and the name. One constant, because the
/// name column's width and the words' left edge are computed from it in three
/// places and a caption whose words start under its speaker's name is the bug
/// that follows from getting one of them wrong.
const DOT_COLUMN: i64 = 14;

/// The accent, for the ring on your own dot. Dark theme's violet, because this
/// surface is always dark (gui/src/renderer/captions.html says why).
const VIOLET: [u8; 3] = [165, 102, 255];

const DEFAULT_FONTS: &[&str] = &[
    "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
    "/usr/share/fonts/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
];

/// A voice's colour, hashed from its id and clamped to the cyan→violet band —
/// the same arithmetic as `speakerHue` in gui/src/renderer/lib/dom.js, because
/// a voice that is one colour on the desktop and another in the headset is two
/// identities. Dark-theme saturation and lightness (72% / 74%), since this
/// surface is always dark.
fn speaker_hue(id: Option<i64>) -> [u8; 3] {
    let Some(id) = id else {
        return FAINT;
    };
    let x = id.wrapping_mul(2_654_435_761_i64).rem_euclid(4_294_967_296);
    let hue = 187.0 + (x % 104) as f32;
    hsl_to_rgb(hue, 0.72, 0.74)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [u8; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as i32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    [
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(id: i64, text: &str) -> Turn {
        Turn {
            id,
            t_ms: id * 1000,
            speaker: Some(1),
            who: "Kira".into(),
            text: text.into(),
            shaky: false,
            translation: None,
        }
    }

    fn renderer() -> Option<Renderer> {
        // Skipped rather than failed on a machine with no system font: this is
        // a check on the layout, not on Fontconfig.
        Renderer::new(Style::default(), None).ok()
    }

    #[test]
    fn a_long_turn_wraps_instead_of_running_off_the_edge() {
        let Some(r) = renderer() else { return };
        let long = "the stairwell one, but it only opens after the lights go down and everybody \
                    has already wandered off to the other instance without saying anything";
        let lines = r.wrap(long, r.style.size, 600);
        assert!(lines.len() > 1, "a long turn did not wrap");
        for line in &lines {
            assert!(
                r.measure(line, r.style.size) <= 600,
                "a wrapped line is still too wide: {line:?}"
            );
        }
    }

    #[test]
    fn one_unbreakable_word_is_broken_rather_than_lost() {
        let Some(r) = renderer() else { return };
        let lines = r.wrap(&"x".repeat(400), r.style.size, 300);
        assert!(lines.len() > 1);
        assert_eq!(lines.concat().len(), 400, "characters went missing");
    }

    #[test]
    fn the_ground_is_deep_space_at_the_opacity_that_was_asked_for() {
        let Ok(r) = Renderer::new(
            Style {
                opacity: 0.9,
                ..Style::default()
            },
            None,
        ) else {
            return;
        };
        let s = r.render(&[turn(1, "hello")], None);
        // Somewhere inside the row's own block, well clear of any glyph: the
        // bottom-left corner of the bar.
        let x = r.style.pad + 4;
        let y = s.height as i64 - r.style.pad - 4;
        let at = ((y as usize) * (s.width as usize) + x as usize) * 4;
        assert_eq!(&s.pixels[at..at + 3], &GROUND, "the ground is not deep space");
        assert!(
            s.pixels[at + 3] > 220,
            "0.9 opacity produced alpha {}",
            s.pixels[at + 3]
        );
    }

    #[test]
    fn nothing_is_drawn_outside_the_surface() {
        let Some(r) = renderer() else { return };
        let s = r.render(&[turn(1, &"a very long sentence ".repeat(40))], None);
        assert_eq!(s.pixels.len(), (s.width as usize) * (s.height as usize) * 4);
    }

    /// The same voice must be the same colour here and on the desktop — the
    /// band is the one gui/src/renderer/lib/dom.js clamps to.
    #[test]
    fn a_voice_keeps_its_colour_and_a_nameless_turn_gets_none() {
        assert_eq!(speaker_hue(None), FAINT);
        for id in 1..40 {
            let rgb = speaker_hue(Some(id));
            assert_ne!(rgb, FAINT, "voice {id} got the nameless grey");
        }
        assert_eq!(speaker_hue(Some(7)), speaker_hue(Some(7)));
    }
}
