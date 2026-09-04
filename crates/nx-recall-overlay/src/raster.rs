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

use crate::feed::{TranslationDisplay, Turn};

/// The deep-space ground, unpacked. Matches `--cap-ground` in captions.css.
pub const GROUND: [u8; 3] = [6, 4, 12];
/// Body ink. Matches `.cap-text`.
const INK: [u8; 3] = [244, 243, 248];
/// The muted ink a shaky row is set in.
const MUTED: [u8; 3] = [185, 180, 201];
/// The second line of a row, whichever of the two it is. Named for its ROLE and
/// not its content since 0.10.2: `translation_display` decides whether the
/// subtext is the translation or the original, and a colour called TRANSLATION
/// sitting under half the rows' originals would be a name that lies.
const SUBTEXT: [u8; 3] = [200, 195, 214];
const FAINT: [u8; 3] = [143, 138, 160];

/// The two lines of one row, in the order they are drawn.
///
/// The whole of `translation_display` lives here, as a function of a turn and a
/// setting and nothing else — no font, no surface, no compositor — because it
/// is the one decision in this file that is a *rule* rather than arithmetic, and
/// the rule has to match the transcript's (`translationCell` in
/// gui/src/renderer/lib/marks.js) exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowLines {
    /// The line at full size: body ink, or the muted ink if the row is shaky.
    pub lead: String,
    /// The quieter, smaller line under it. `None` unless the turn was
    /// translated — nothing here ever invents a second line.
    pub sub: Option<String>,
    /// Whether `lead` is the translation. Not used by the drawing (both lines
    /// are drawn the same way whichever they are) but it is the fact the layout
    /// turns on, and a test that could not see it would be checking strings.
    pub lead_is_translation: bool,
}

/// Lay out one row.
///
/// What does NOT change with the setting, matching the transcript in both modes:
///
/// - **both lines are always there.** The original never leaves the row.
/// - **the subtext wears a language code**, and which language it is depends on
///   which line the subtext is: in `Under` it is the translation's target, so
///   the reader knows what they are being offered; in `Main` it is the
///   segment's own `lang`, which is what turns "a quieter second line" into
///   "the original, in Polish". The transcript carries the same two facts in a
///   `title` and a `.said-lang` chip; a caption bar has no hover, so both are
///   spelled on the line.
/// - **the "≈" stays on the LEAD.** In the transcript the shaky mark sits in
///   the row's meta cell, on neither line — it is a fact about the row. The
///   caption bar has no meta cell, so it goes on the line being read, which is
///   the one place it cannot be missed. A translation of a shaky reading is not
///   less doubtful than the reading.
pub fn row_lines(turn: &Turn, display: TranslationDisplay) -> RowLines {
    let mut said = if turn.shaky {
        format!("≈ {}", turn.text)
    } else {
        turn.text.clone()
    };
    // 0.12.4: a turn that is still being spoken (`crate::feed::Turn::growing`)
    // ends in an ellipsis and is otherwise an ordinary row. Deliberately the
    // ONLY difference — the same font, the same ink, the same ground. A slice's
    // words were decoded from their own audio at a boundary the VAD found and
    // will not be taken back, so drawing them as a guess would say something
    // false; what is unfinished is the sentence, and that is what "…" says.
    //
    // On the lead line and not the sub-line, because the sub-line is the
    // translation of a sentence that has not finished either, and two
    // ellipses on one row read as a mistake.
    if turn.growing {
        said.push('…');
    }
    let Some((tr_lang, tr_text)) = turn.translation.as_ref() else {
        // The ordinary row, and most rows. One line, no code, nothing implied.
        return RowLines {
            lead: said,
            sub: None,
            lead_is_translation: false,
        };
    };
    match display {
        TranslationDisplay::Under => RowLines {
            lead: said,
            sub: Some(tagged(tr_lang.as_deref(), tr_text)),
            lead_is_translation: false,
        },
        TranslationDisplay::Main => RowLines {
            // The translation leads, and the "≈" leads with it: it is the row
            // that is uncertain, not the line.
            lead: if turn.shaky {
                format!("≈ {tr_text}")
            } else {
                tr_text.clone()
            },
            sub: Some(tagged(turn.lang.as_deref(), &turn.text)),
            lead_is_translation: true,
        },
    }
}

/// A line with its language code in front of it, or just the line. Absent is
/// the ordinary case for `lang` on an older daemon and means exactly nothing.
fn tagged(lang: Option<&str>, text: &str) -> String {
    match lang.map(str::trim).filter(|l| !l.is_empty()) {
        Some(l) => format!("{}  {text}", l.to_uppercase()),
        None => text.to_owned(),
    }
}

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
    /// Which of a translated row's two lines leads (`[assist]
    /// translation_display`, 0.10.2). On the Style rather than an argument to
    /// `render` so that every caller — the desktop bar, `--render`, the headset
    /// path — picks it up by setting one field, and none of them can forget to
    /// pass it.
    pub translation_display: TranslationDisplay,
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
            // The daemon's default, so a caption bar that has not been told
            // anything lays a translated row out the way the transcript does.
            translation_display: TranslationDisplay::Main,
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

    /// A new size, opacity or surface, without re-reading the font off disk.
    /// The desktop path rebuilds its style every time captions.json is written,
    /// and a settings slider must not turn into a file read per frame.
    pub fn set_style(&mut self, style: Style) {
        self.style = style;
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
            // The icon goes BEFORE the name, in the name's own colour column,
            // so a highlighted row is picked out by two independent marks — one
            // for anybody, one for anybody who cannot separate these hues.
            // `drawable_icon` is what keeps that promise honest on a machine
            // whose font has no emoji: see its note.
            let name = match self.drawable_icon(turn.icon.as_deref()) {
                Some(icon) => format!("{icon} {}  ", turn.who),
                None => format!("{}  ", turn.who),
            };
            // 0.12.4: laughter, and only laughter. AFTER the name, where the
            // highlight icon is before it, so the two marks cannot be read as
            // one glyph — and through the same font check for the same reason:
            // `glyph` turns anything the font lacks into '?', and "Kira ?" is
            // worse than no mark at all. See `laugh_glyph`.
            let name = match self.laugh_glyph(turn.laughed) {
                Some(g) => format!("{} {g} ", name.trim_end()),
                None => name,
            };
            let name_w = self.measure(&name, name_size);
            let lines = row_lines(turn, self.style.translation_display);
            let column = inner - name_w - DOT_COLUMN;
            let text_lines = self.wrap(&lines.lead, self.style.size, column);
            let tr_lines = lines
                .sub
                .as_deref()
                .map(|sub| self.wrap(sub, tr_size, column))
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
                hue: turn_hue(turn),
                // Either the feed already stamped it (the desktop path, where
                // the roster is known before a caption exists) or the caller
                // named the voice (the headset path, which has no feed).
                mine: turn.mine || (turn.speaker.is_some() && turn.speaker == you),
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

        // Your own turns are dimmer, at the same YOU_DIM the desktop window uses
        // (gui/src/renderer/lib/captions.js): you already know what you said, and
        // the reason to keep the row at all is rhythm. The GROUND above is not
        // dimmed with it — a hole in the bar under one row would read as a
        // rendering fault rather than as "this one is yours".
        let dim = if b.mine { YOU_DIM } else { 1.0 };

        // The dot's own width plus its gap, then the name, then the words.
        let text_x = x0 + self.style.pad + DOT_COLUMN + b.name_w;
        let mut y = top + self.style.pad + (self.style.size * 0.9) as i64;

        // The dot, then the name, in the speaker's own hue — the same hash the
        // desktop uses, so a voice is the same colour in both places. The one
        // place violet is spent is the ring on your own dot.
        let dot_y = top + self.style.pad + (name_size * 0.4) as i64;
        if b.mine {
            s.fill(
                x0 + self.style.pad - 3,
                dot_y - 3,
                11,
                11,
                VIOLET,
                0.9 * dim,
            );
        }
        s.fill(x0 + self.style.pad, dot_y, 5, 5, b.hue, dim);
        self.draw_text(
            s,
            &b.name,
            x0 + self.style.pad + DOT_COLUMN,
            top + self.style.pad + (name_size * 0.9) as i64,
            name_size,
            b.hue,
            dim,
        );

        let ink = if b.shaky { MUTED } else { INK };
        for line in &b.text_lines {
            self.draw_text(s, line, text_x, y, self.style.size, ink, dim);
            y += line_h;
        }
        for line in &b.tr_lines {
            self.draw_text(s, line, text_x, y + 2, tr_size, SUBTEXT, dim);
            y += tr_line_h;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_text(
        &self,
        s: &mut Surface,
        text: &str,
        x: i64,
        baseline: i64,
        size: f32,
        rgb: [u8; 3],
        dim: f32,
    ) {
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
                    (*coverage as f32 / 255.0) * dim,
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

    /// The icon, if this machine's font can actually draw it.
    ///
    /// `glyph` turns anything the font lacks into `'?'`, which is right for a
    /// stray character inside a sentence and very wrong here: a highlight that
    /// renders as "? Kira" is worse than no highlight at all, and the system
    /// sans-serif this surface rasterises with (Noto Sans, DejaVu — see
    /// `DEFAULT_FONTS`) carries almost no emoji. There is no colour-emoji path
    /// in a monochrome coverage rasteriser to fall back to.
    ///
    /// So the icon is drawn only if every one of its characters is really in
    /// the font, and dropped silently otherwise. The colour half of the
    /// highlight always works, which is why dropping this half is a
    /// degradation rather than a failure — and why the desktop, which has a
    /// full font stack, is where an emoji highlight is really read.
    /// Every glyph the laughter mark will try, best first.
    ///
    /// Three, and the list is a ladder rather than a preference: the system
    /// sans-serif this surface rasterises with is whatever the machine happens
    /// to ship, and none of these is guaranteed. `♪`-style symbols are common
    /// in Noto Sans and DejaVu; `~` is in every font that has ever existed and
    /// is the floor. The emoji 😄 is deliberately **not** on the list — there
    /// is no colour-emoji path in a monochrome coverage rasteriser, and the
    /// fonts here carry almost no emoji anyway (see [`Self::drawable_icon`]).
    ///
    /// `☺` first because it is a face and reads instantly; `ᴴᴬ` was tried and
    /// rejected — modifier letters are missing from more fonts than they are
    /// present in, and a two-glyph mark next to a name looks like part of it.
    const LAUGH_GLYPHS: [char; 3] = ['\u{263a}', '\u{266a}', '~'];

    /// The laughter mark, if this machine's font can draw one at all.
    ///
    /// The same rule and the same reason as [`Self::drawable_icon`]: a mark
    /// that renders as a tofu box is worse than no mark, and this one is a
    /// *degradation* rather than a failure because the desktop transcript
    /// carries the same fact as a word (`gui/src/renderer/lib/marks.js`).
    ///
    /// Unlike the "≈" the shaky mark falls back for, dropping this silently is
    /// correct: "≈" means a second decoder disagreed and a row that stops
    /// saying so is a row claiming to be solid, whereas a caption with no
    /// laughter mark is just a caption.
    fn laugh_glyph(&self, laughed: bool) -> Option<char> {
        laughed
            .then(|| {
                Self::LAUGH_GLYPHS
                    .into_iter()
                    .find(|c| self.font.lookup_glyph_index(*c) != 0)
            })
            .flatten()
    }

    fn drawable_icon(&self, icon: Option<&str>) -> Option<String> {
        let icon = icon.map(str::trim).filter(|s| !s.is_empty())?;
        icon.chars()
            .all(|c| self.font.lookup_glyph_index(c) != 0)
            .then(|| icon.to_owned())
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

/// How much dimmer a row of yours is. The same number as `YOU_DIM` in
/// gui/src/renderer/lib/captions.js — one constant, two languages, because a
/// "You" row that is dimmer in one surface and not in the other is two designs.
pub const YOU_DIM: f32 = 0.55;

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
/// What colour this row's name and dot are.
///
/// A pinned highlight wins over the hash; anything else — no highlight, or a
/// token from a daemon newer than this build — falls through to the colour the
/// voice has always had. That fallthrough is the reason a highlight can never
/// make a caption worse: the failure mode is the old colour, not no colour.
///
/// Both branches end at the same `hsl_to_rgb` with the same saturation and
/// lightness, so a highlight changes WHICH hue a name wears and nothing about
/// how legible it is against this surface.
fn turn_hue(turn: &Turn) -> [u8; 3] {
    match turn.colour.as_deref().and_then(crate::palette::hue) {
        Some(hue) => hsl_to_rgb(hue, 0.72, 0.74),
        None => speaker_hue(turn.speaker),
    }
}

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
            lang: None,
            translation: None,
            mine: false,
            colour: None,
            icon: None,
            // A finished turn. The growing case has its own test in `feed`.
            growing: false,
            laughed: false,
        }
    }

    /// A translated turn: said in `lang`, read back in `tr_lang`.
    fn translated(said: &str, lang: &str, tr_lang: &str, tr: &str) -> Turn {
        Turn {
            lang: Some(lang.into()),
            translation: Some((Some(tr_lang.into()), tr.into())),
            ..turn(1, said)
        }
    }

    /// The layout decision, both ways, and the row it does not apply to.
    ///
    /// This is the whole of `translation_display` in the caption bar: everything
    /// after it is wrapping and pixels. It has to agree with `translationCell`
    /// in gui/src/renderer/lib/marks.js, because a turn that reads one way in
    /// the transcript and the other way over the game is two designs.
    #[test]
    fn main_puts_the_translation_on_the_line_and_the_original_under_it() {
        let t = translated(
            "das gleiche, auf Deutsch",
            "de",
            "en",
            "the same, in English",
        );
        let lines = row_lines(&t, TranslationDisplay::Main);
        assert!(lines.lead_is_translation);
        assert_eq!(lines.lead, "the same, in English");
        // The original keeps its own language code — the difference between
        // "a quieter second line" and "the original, in German".
        assert_eq!(lines.sub.as_deref(), Some("DE  das gleiche, auf Deutsch"));
    }

    #[test]
    fn under_is_0_9_0s_layout_with_the_original_leading() {
        let t = translated(
            "das gleiche, auf Deutsch",
            "de",
            "en",
            "the same, in English",
        );
        let lines = row_lines(&t, TranslationDisplay::Under);
        assert!(!lines.lead_is_translation);
        assert_eq!(lines.lead, "das gleiche, auf Deutsch");
        // …and here the code on the subtext is the TARGET, because that is the
        // fact the reader needs about the line they are being offered.
        assert_eq!(lines.sub.as_deref(), Some("EN  the same, in English"));
    }

    /// Most rows. Neither setting may invent a second line, and neither may
    /// hang a language code on a row that was never translated.
    #[test]
    fn a_turn_with_no_translation_is_one_line_in_either_mode() {
        let plain = turn(1, "just the one sentence");
        for display in [TranslationDisplay::Main, TranslationDisplay::Under] {
            let lines = row_lines(&plain, display);
            assert_eq!(lines.lead, "just the one sentence");
            assert_eq!(lines.sub, None, "a second line appeared from nowhere");
            assert!(!lines.lead_is_translation);
        }
        // Even when the daemon told us what language it was in: `lang` alone is
        // not a translation and must not put a code on the row.
        let mut known = plain.clone();
        known.lang = Some("de".into());
        assert_eq!(row_lines(&known, TranslationDisplay::Main).sub, None);
    }

    /// The "≈" is a fact about the ROW, not about one of its lines — the
    /// transcript puts it in the meta cell, and a caption bar has none. So it
    /// leads in both modes: a translation of a shaky reading is not less
    /// doubtful than the reading.
    #[test]
    fn the_shaky_mark_stays_on_the_line_being_read() {
        let mut t = translated("das gleiche", "de", "en", "the same");
        t.shaky = true;
        assert_eq!(row_lines(&t, TranslationDisplay::Main).lead, "≈ the same");
        assert_eq!(
            row_lines(&t, TranslationDisplay::Under).lead,
            "≈ das gleiche"
        );
        // The subtext never wears it twice.
        assert_eq!(
            row_lines(&t, TranslationDisplay::Main).sub.as_deref(),
            Some("DE  das gleiche")
        );
    }

    /// A daemon too old to send `lang`, and one that sends an empty one. The
    /// original is still the subtext; it just has nothing to be labelled with.
    #[test]
    fn a_missing_language_code_is_left_off_rather_than_guessed() {
        let mut t = translated("das gleiche", "de", "en", "the same");
        t.lang = None;
        assert_eq!(
            row_lines(&t, TranslationDisplay::Main).sub.as_deref(),
            Some("das gleiche")
        );
        t.lang = Some("  ".into());
        assert_eq!(
            row_lines(&t, TranslationDisplay::Main).sub.as_deref(),
            Some("das gleiche")
        );
        t.translation = Some((None, "the same".into()));
        assert_eq!(
            row_lines(&t, TranslationDisplay::Under).sub.as_deref(),
            Some("the same")
        );
    }

    /// Both modes draw two lines and neither is allowed to lose one, whatever
    /// the surface is. The regression this guards is a `main` row whose
    /// original fell off the bottom because the height was measured for the
    /// other layout.
    #[test]
    fn both_layouts_reach_the_surface_and_neither_drops_a_line() {
        let Some(mut r) = renderer() else { return };
        let t = translated(
            "das ist der lange deutsche satz, der umgebrochen werden muss",
            "de",
            "en",
            "this is the long English sentence that has to wrap",
        );
        let mut inked = Vec::new();
        for display in [TranslationDisplay::Under, TranslationDisplay::Main] {
            r.set_style(Style {
                translation_display: display,
                ..Style::default()
            });
            let s = r.render(std::slice::from_ref(&t), None);
            inked.push(
                s.pixels
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .filter(|p| p[3] > 0)
                    .count(),
            );
        }
        assert!(inked[0] > 0 && inked[1] > 0);
        // Same two lines, the other way up: near-identical coverage, and very
        // much not one line's worth.
        let (a, b) = (inked[0] as f64, inked[1] as f64);
        assert!(
            (a - b).abs() / a.max(b) < 0.35,
            "one layout drew far less than the other: {a} vs {b}"
        );
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
        assert_eq!(
            &s.pixels[at..at + 3],
            &GROUND,
            "the ground is not deep space"
        );
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

    /// Your own row is dimmer, and the bar under it is not: a hole in the
    /// ground under one row would read as a rendering fault.
    #[test]
    fn your_own_row_is_dimmer_and_its_ground_is_not() {
        let Some(r) = renderer() else { return };
        let ink = |s: &Surface| -> u64 {
            s.pixels
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| p[3] as u64)
                .sum()
        };
        let theirs = r.render(&[turn(1, "the same words, twice")], None);
        let mine = r.render(&[turn(1, "the same words, twice")], Some(1));
        assert!(
            ink(&mine) < ink(&theirs),
            "a row of yours was not dimmer than one of theirs"
        );
        // The ground, well clear of any glyph, is identical in both.
        let x = r.style.pad + 4;
        let y = theirs.height as i64 - r.style.pad - 4;
        let at = ((y as usize) * (theirs.width as usize) + x as usize) * 4;
        assert_eq!(&mine.pixels[at..at + 4], &theirs.pixels[at..at + 4]);
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

    /// A pinned colour replaces the hash, and only for the voice it was pinned
    /// to. The point of the feature is that one name in a stack is picked out;
    /// a highlight that also moved everybody else's colour would be a theme.
    #[test]
    fn a_highlight_repaints_one_voice_and_leaves_the_rest_alone() {
        let plain = turn(1, "hello");
        let lit = Turn {
            colour: Some("amber".into()),
            ..turn(1, "hello")
        };
        assert_eq!(turn_hue(&plain), speaker_hue(Some(1)));
        assert_ne!(turn_hue(&lit), turn_hue(&plain));
        assert_eq!(turn_hue(&lit), hsl_to_rgb(44.0, 0.72, 0.74));

        // Another voice, same stack, untouched.
        let other = Turn {
            speaker: Some(2),
            ..turn(2, "hello")
        };
        assert_eq!(turn_hue(&other), speaker_hue(Some(2)));
    }

    /// A colour this build has never heard of is the OLD colour, not no colour
    /// — a daemon may name an eleventh accent before this binary is rebuilt.
    #[test]
    fn a_colour_from_a_newer_daemon_falls_back_to_the_hash() {
        let odd = Turn {
            colour: Some("chartreuse".into()),
            ..turn(1, "hello")
        };
        assert_eq!(turn_hue(&odd), speaker_hue(Some(1)));

        // And a highlight on a nameless turn still cannot invent a speaker.
        let nameless = Turn {
            speaker: None,
            colour: None,
            ..turn(1, "hello")
        };
        assert_eq!(turn_hue(&nameless), FAINT);
    }

    /// The icon is drawn when the font has it and dropped — never turned into
    /// the `?` that `glyph` gives everything else — when it does not.
    #[test]
    fn an_icon_is_drawn_only_if_this_machines_font_really_has_it() {
        let Some(r) = renderer() else { return };
        assert_eq!(r.drawable_icon(None), None);
        assert_eq!(r.drawable_icon(Some("")), None);
        assert_eq!(r.drawable_icon(Some("   ")), None);
        // ASCII is in every font this list names, so it stands in for "the
        // font has this glyph" without depending on which font is installed.
        assert_eq!(r.drawable_icon(Some("x")), Some("x".to_owned()));
        // A private-use codepoint is in no font, and must vanish rather than
        // become a tofu or a question mark in front of somebody's name.
        assert_eq!(r.drawable_icon(Some("\u{f8ff}")), None);
    }

    /// An icon widens the name column rather than overprinting the words —
    /// the bug `DOT_COLUMN`'s note is about, one field later.
    #[test]
    fn an_icon_pushes_the_words_right_instead_of_colliding_with_them() {
        let Some(r) = renderer() else { return };
        let plain = turn(1, "some words");
        let lit = Turn {
            icon: Some("x".into()),
            ..turn(1, "some words")
        };
        let name_size = (r.style.size * 0.44).max(11.0);
        let plain_w = r.measure(&format!("{}  ", plain.who), name_size);
        let lit_w = r.measure(&format!("x {}  ", lit.who), name_size);
        assert!(lit_w > plain_w, "the icon took no width in the name column");

        // Both still render, and the highlighted one has more ink in it.
        let a = r.render(&[plain], None);
        let b = r.render(&[lit], None);
        assert_eq!(a.width, b.width);
        assert_eq!(a.height, b.height);
    }

    /// 0.12.4: laughter is the only event this surface draws, it goes AFTER
    /// the name, and it degrades to nothing rather than to a tofu box.
    #[test]
    fn laughter_is_one_glyph_after_the_name_and_only_one_this_font_has() {
        let Some(r) = renderer() else { return };
        assert_eq!(r.laugh_glyph(false), None, "a quiet turn wears no mark");
        // Whatever this machine's font is, the ladder ends at '~', which every
        // font has — so on a machine that can draw text at all there is a mark.
        let g = r
            .laugh_glyph(true)
            .expect("the ladder bottoms out at ASCII");
        assert!(
            Renderer::LAUGH_GLYPHS.contains(&g),
            "{g:?} is not on the ladder"
        );
        assert!(
            r.font.lookup_glyph_index(g) != 0,
            "{g:?} is not in this machine's font"
        );
        // The emoji nobody can draw in a coverage rasteriser is not on it.
        assert!(!Renderer::LAUGH_GLYPHS.contains(&'\u{1f604}'));

        // It widens the name column, exactly as the highlight icon does, so the
        // words move right instead of being drawn over.
        let quiet = turn(1, "der Tank ist explodiert");
        let loud = Turn {
            laughed: true,
            ..turn(1, "der Tank ist explodiert")
        };
        let a = r.render(&[quiet], None);
        let b = r.render(&[loud], None);
        assert_eq!(a.width, b.width);
        assert_eq!(a.height, b.height);
        assert!(
            ink(&b) > ink(&a),
            "the laughter mark put no pixels on the surface"
        );
    }

    /// How much was drawn. Coarse on purpose — the question is only "is there
    /// more ink than there was", and a pixel-exact golden of a font this
    /// machine happens to ship would fail on every other machine.
    fn ink(s: &Surface) -> u64 {
        // The alpha byte of every RGBA pixel. `as_chunks` rather than
        // `chunks_exact` because the chunk size is a constant and clippy is
        // right that the compiler can then see it.
        s.pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| p[3] as u64)
            .sum()
    }
}
