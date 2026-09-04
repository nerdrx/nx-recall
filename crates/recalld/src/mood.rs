//! How a turn sounded (0.12.4): SenseVoice's own emotion and event tags, read
//! off the stored clips by a background pass.
//!
//! The user asked for one thing — *"colour in the text or tag the text with the
//! mood in the transcript"* — and the honest answer to it turned out to be two
//! different features with two different amounts of evidence behind them. This
//! module is where that split lives.
//!
//! ## Where the tags come from, and why they were free
//!
//! 0.11.6 shipped `sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17` to
//! transcribe Korean and Chinese (FINDINGS §27). SenseVoice does not produce a
//! transcript, it produces a transcript wrapped in metadata: language, emotion,
//! audio event, and whether inverse text normalisation ran. The daemon has been
//! throwing three of those four away ever since ([`crate::asr_cjk::strip_tags`]
//! and, before it, the fact that `transcribe` read only `text`) because nothing
//! asked for them.
//!
//! So the model was already downloaded, already catalogued, already loadable,
//! and already measured on this machine's cores. What this round adds is a
//! second reader of a decode that was already affordable — and a pass that runs
//! it over the rows the CJK route never touches, which is nearly all of them.
//!
//! ## What is measured, and what is therefore rendered
//!
//! Everything here was gated on `spike/mood_bench.py` over the user's own
//! archive before a line of GUI was written. FINDINGS §42 has the table; the
//! two sentences that decide what ships are:
//!
//! * **Events are rendered, on clips up to five seconds.** The laughter tag is
//!   3–10x more likely than chance to land on a clip the speech decoder had no
//!   words for, which is what a laugh looks like to an ASR — and it is *below*
//!   chance past five seconds, because the head answers "was there laughter
//!   anywhere in this clip" and a chip on a long row reads as "this turn was
//!   laughter". [`EVENT_MEASURED_MAX_S`] is where that boundary lives.
//! * **Mood is stored and NOT rendered** — [`MOOD_IS_MEASURED`] is the one place
//!   that says so, and it carries the reason. The model abstains
//!   (`<|EMO_UNKNOWN|>`) on **74.7%** of real turns, and on the quarter it does
//!   answer it agrees with a sentiment word list **31.6% of the time against a
//!   86.4% majority-class baseline** — it is not merely unproven, it is worse
//!   than a constant guess.
//!
//! Both halves are stored either way. A tag nobody renders costs three nullable
//! columns and buys the ability to re-measure next month against a bigger
//! archive without re-listening to it — and a *rendered* tag that is wrong is a
//! transcript that lies about how somebody felt, which is a worse thing to ship
//! than a blank column.
//!
//! ## The pass
//!
//! [`run`] is [`crate::sweep`]'s shape, deliberately, down to the sleep:
//!
//! * its own thread ([`crate::pipeline::background_current_thread`], so it
//!   inherits `[runtime].inference_nice` and `inference_cpus` and loses every
//!   scheduling contest it enters);
//! * **resumable**, on `segments.mood_at_ns IS NULL` — the queue is a column,
//!   not a cursor, so a daemon killed mid-pass resumes exactly where it was;
//! * **bounded**, at `[mood].rows_per_run` per opening of the gate;
//! * gates re-read **between rows**, so somebody who sits down at 04:00 gets
//!   their cores back within one clip rather than within one run;
//! * and the model is loaded on the first row of a run and **dropped when the
//!   queue empties**, which is the normal state once the archive is caught up.
//!   239 MB resident for the life of a daemon that has nothing left to listen
//!   to is 239 MB nobody agreed to.
//!
//! No GPU: SenseVoice-small int8 runs at RTF 0.08 on four niced cores
//! (measured, §42), which is a night's archive in a few minutes and is why this
//! pass has none of the night shift's card-minding machinery.
//!
//! ## Why this is not part of the night shift or the language sweep
//!
//! The same reason the language sweep is not part of the night shift, one rung
//! along. The night shift needs a gigabyte of whisper and a local compile; the
//! language sweep needs a 13 MB identifier and the CJK decoders; this needs
//! SenseVoice and nothing else. Folding it into either would make an unrelated
//! `enabled = false` silently switch it off.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::asr_cjk::{CjkAsr, Heard};
use crate::bus::Bus;
use crate::clock::utc_now_ns;
use crate::config::MoodConfig;
use crate::control::Control;
use crate::models::{CjkModel, ModelSet};
use crate::store::{MoodTotals, Store};

// ---------------------------------------------------------------------------
// the vocabulary
// ---------------------------------------------------------------------------

/// How a turn sounded, as a closed set of four.
///
/// SenseVoice's emotion head emits more than four — `FEARFUL`, `DISGUSTED`,
/// `SURPRISED` and `EMO_UNKNOWN` are all in its vocabulary — and this daemon
/// stores four. That is a decision and not an omission:
///
/// * `EMO_UNKNOWN` is the model declining to answer, which is [`None`] here.
///   It is also the **most common answer on real audio** — 74.7% of 15,366
///   clips (§42) — so treating it as a fifth mood would fill the transcript
///   with a chip that means "no comment".
/// * The other three are in the head's vocabulary and essentially never come
///   back on this corpus. Storing a word the user will see once a year, in a
///   palette that has to stay small enough to be told apart at a glance
///   (`crate::palette`), buys nothing; they are folded into [`None`] by
///   [`Mood::parse`] rather than invented a colour for.
///
/// If they ever do show up in numbers, the fix is one arm here and one palette
/// entry — which is exactly the shape a closed set is supposed to have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mood {
    Happy,
    Sad,
    Angry,
    Neutral,
}

impl Mood {
    /// The word stored in `segments.mood` and put on the wire. Lower case, and
    /// it is an identifier rather than a label: the GUI looks a palette token
    /// up by it and the daemon never renders it as prose.
    pub fn as_str(self) -> &'static str {
        match self {
            Mood::Happy => "happy",
            Mood::Sad => "sad",
            Mood::Angry => "angry",
            Mood::Neutral => "neutral",
        }
    }

    /// Read one of the model's `<|…|>` emotion tags.
    ///
    /// Accepts the tag with or without its brackets, and case-insensitively,
    /// because the two callers hand it over differently: the C API's `emotion`
    /// field carries `<|HAPPY|>` and a value read back out of `segments.mood`
    /// carries `happy`.
    /// `None` for `EMO_UNKNOWN`, for the three moods this daemon does not
    /// store, for an empty field (which is what the Japanese Parakeet gives —
    /// `nemo_ctc` has no emotion head) and for anything a newer model invents.
    pub fn parse(tag: &str) -> Option<Self> {
        match bare(tag).to_ascii_lowercase().as_str() {
            "happy" => Some(Mood::Happy),
            "sad" => Some(Mood::Sad),
            "angry" => Some(Mood::Angry),
            "neutral" => Some(Mood::Neutral),
            _ => None,
        }
    }

    /// Every mood, for a test and for the wire's legend.
    pub const ALL: &'static [Mood] = &[Mood::Happy, Mood::Sad, Mood::Angry, Mood::Neutral];
}

/// What else was on the clip besides speech, as a closed set of four.
///
/// `Speech` is in SenseVoice's event vocabulary and is deliberately **not**
/// here: it is on **96.5%** of this archive's turns (§42) and a mark that is on
/// nearly every row is not a mark. `Event_UNK` is the same abstention
/// `EMO_UNKNOWN` is.
///
/// What is left is the four that are *worth a glyph* — something happened on
/// this turn that the words do not record. Laughter is the one the user is
/// really asking about, and it is the one with a measurement behind it.
///
/// ## The four that are here, and the four that are not
///
/// Measured over 15,366 clips (§42), the head also emits `Cough` (60 rows),
/// `Sing` (27), `Breath` (5) and `Sneeze` (1) — and never once emitted
/// `Applause` or `Cry`, which are in this list. That asymmetry is deliberate
/// both ways:
///
/// * **`Applause` and `Cry` stay** even though this archive has none. They are
///   in the upstream vocabulary, they are things a person would want marked if
///   they happened, and an arm that costs one line and never fires is cheaper
///   than the release that has to add it.
/// * **`Cough`, `Breath` and `Sneeze` are left out** because they are not
///   *events in a conversation* — they are noises a body makes, and a
///   transcript that annotated somebody's cough would be a transcript nobody
///   wants to reread. `Sing` was the close call; it is out because 27 rows is
///   not enough to have measured it, and the rule in this round has been that
///   nothing renders that was not measured.
#[allow(rustdoc::private_intra_doc_links)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Event {
    /// `<|Laughter|>`. The only event the headset overlay draws (`docs/OVERLAY.md`).
    Laughter,
    /// `<|BGM|>` — music under the turn.
    Music,
    Applause,
    Cry,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Event::Laughter => "laughter",
            Event::Music => "music",
            Event::Applause => "applause",
            Event::Cry => "cry",
        }
    }

    /// Read one of the model's `<|…|>` event tags, in either spelling.
    ///
    /// The model's own words on the left, this daemon's on the right, and they
    /// differ in two places on purpose: `BGM` is jargon (it is what the
    /// upstream vocabulary calls background music) and `Cry` is a verb where
    /// every other entry is a noun.
    pub fn parse(tag: &str) -> Option<Self> {
        match bare(tag).to_ascii_lowercase().as_str() {
            "laughter" => Some(Event::Laughter),
            "bgm" | "music" => Some(Event::Music),
            "applause" => Some(Event::Applause),
            "cry" | "crying" => Some(Event::Cry),
            _ => None,
        }
    }

    pub const ALL: &'static [Event] = &[Event::Laughter, Event::Music, Event::Applause, Event::Cry];
}

/// Strip one `<|…|>` wrapper and the whitespace around it, if there is one.
///
/// Not [`crate::asr_cjk::strip_tags`], which *removes* tags from a sentence:
/// this keeps the one word inside, which is the opposite job on the same
/// syntax.
fn bare(tag: &str) -> &str {
    let t = tag.trim();
    t.strip_prefix("<|")
        .and_then(|t| t.strip_suffix("|>"))
        .unwrap_or(t)
        .trim()
}

/// Every `<|…|>` tag in a field, as the bare words inside them.
///
/// The event field is **one or more** tags run together — sherpa hands back
/// `<|BGM|><|Laughter|>` with nothing between them — so it cannot be split on a
/// separator: there is none, and splitting on `|>` leaves every piece missing
/// the closing half its own parser is looking for. That was the first version
/// and it silently found zero events on every row, which is the shape of bug
/// this whole module is trying not to ship.
///
/// A field with no brackets at all is one bare word, so the operator's
/// `laughter` and the model's `<|Laughter|>` reach [`Event::parse`] the same
/// way.
fn tags(field: &str) -> Vec<&str> {
    let field = field.trim();
    if !field.contains("<|") {
        return if field.is_empty() {
            Vec::new()
        } else {
            vec![field]
        };
    }
    let mut out = Vec::new();
    let mut rest = field;
    while let Some(open) = rest.find("<|") {
        rest = &rest[open + 2..];
        let Some(close) = rest.find("|>") else { break };
        out.push(rest[..close].trim());
        rest = &rest[close + 2..];
    }
    out
}

/// One clip, as this module reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reading {
    /// `None` when the model abstained, when the decoder has no emotion head,
    /// or when it named a mood this build does not store.
    pub mood: Option<Mood>,
    /// Sorted and deduplicated. Empty is the ordinary answer.
    pub events: Vec<Event>,
}

/// How long a clip may be before its event tag stops being about the clip.
///
/// **Five seconds, and this is the round's one genuinely surprising
/// measurement** (FINDINGS §42). Scored within duration bands against "the
/// speech decoder had no words for this clip" — which is what a laugh looks
/// like to an ASR, and the only proxy on this corpus with enough positives to
/// separate anything:
///
/// | clip length | tagged, wordless | untagged, wordless | lift |
/// |---|---:|---:|---:|
/// | 1.0–1.5 s | 86.7% | 28.7% | **3.02x** |
/// | 1.5–2.5 s | 71.2% | 11.3% | **6.30x** |
/// | 2.5–5.0 s | 44.4% | 4.6%  | **9.76x** |
/// | 5.0 s +   |  3.8% | 7.0%  | 0.55x |
///
/// The tag is strong up to five seconds and **below chance above it**, and the
/// reason is visible in the rows themselves: a 17-second turn is a paragraph of
/// German with a laugh somewhere in it, and the event head is answering "was
/// there laughter anywhere in this clip" while a chip on the row reads as "this
/// turn was laughter". Those are different claims, and only the first is what
/// the model was asked.
///
/// So the pass **does not store an event on a clip longer than this**. That is
/// asymmetric with the mood, which is stored on every clip, and the asymmetry
/// has a reason: a stored mood is never drawn, so keeping it costs nothing and
/// buys a re-measurement, whereas a stored event *is* drawn — and a mark the
/// measurement does not cover would either be rendered anyway or force this
/// number into three more files.
pub const EVENT_MEASURED_MAX_S: f32 = 5.0;

impl Reading {
    /// What the pass heard, out of what the decoder said.
    ///
    /// `duration_s` is the clip's length, and it is a parameter rather than
    /// something read from the row because of [`EVENT_MEASURED_MAX_S`]: past
    /// that length the event tag was measured and found to be worse than
    /// chance, so it is dropped here — in the one pure function every caller
    /// goes through — rather than in the worker, where a second caller could
    /// miss it.
    ///
    /// Total otherwise: there is no failure mode, because "the model said
    /// something this build does not know" and "the model said nothing" are the
    /// same answer and the same rendering.
    pub fn of(heard: &Heard, duration_s: f32) -> Self {
        let mut events: Vec<Event> = if duration_s <= EVENT_MEASURED_MAX_S {
            tags(&heard.event)
                .into_iter()
                .filter_map(Event::parse)
                .collect()
        } else {
            Vec::new()
        };
        events.sort_unstable();
        events.dedup();
        Self {
            // The mood is NOT bounded by length. It is not drawn at all
            // (`MOOD_IS_MEASURED`), so there is nothing for a bad one to spoil,
            // and a stored tag on a long clip is exactly the row a future
            // measurement would want.
            //
            // One tag, for this field, always — but read through the same
            // scanner, so a build of sherpa that ever ran two together does not
            // silently produce no mood at all.
            mood: tags(&heard.emotion).into_iter().find_map(Mood::parse),
            events,
        }
    }

    /// `segments.mood`, or `None` for a row the model had no opinion about.
    pub fn mood_column(&self) -> Option<&'static str> {
        self.mood.map(Mood::as_str)
    }

    /// `segments.events` — the sorted set, comma-joined — or `None` for a turn
    /// that carried none. `None` and not `""`, so there is exactly one
    /// representation of "nothing happened" and every reader can test it with
    /// `IS NULL`, which is the rule `palette::normalise_icon` follows too.
    pub fn events_column(&self) -> Option<String> {
        (!self.events.is_empty()).then(|| {
            self.events
                .iter()
                .map(|e| e.as_str())
                .collect::<Vec<_>>()
                .join(",")
        })
    }

    /// Did anything happen worth a mark? The one question the overlay asks.
    pub fn laughed(&self) -> bool {
        self.events.contains(&Event::Laughter)
    }
}

/// Read a stored `segments.events` string back into the set.
///
/// Tolerant of a value written by a newer daemon that knows a fifth event: the
/// unknown word is dropped and the rest of the row renders, which is the same
/// rule `palette::accent` follows for an unknown colour.
pub fn parse_events(stored: Option<&str>) -> Vec<Event> {
    let mut out: Vec<Event> = stored
        .unwrap_or("")
        .split(',')
        .filter_map(Event::parse)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------
// what the measurement decided
// ---------------------------------------------------------------------------

/// **Is the mood tag good enough to put in front of a person?**
///
/// `false`, and this constant is the entire mechanism by which that is true:
/// the daemon stores every mood it reads and puts it on the wire, and this
/// says whether any surface may *act* on it. `search.answer` reads it, the
/// digest's "how it felt" line reads it, `person.get` reads it, and the GUI
/// reads it off `status` so a client and a daemon can never disagree about
/// whether a feature is on.
///
/// ## The rule, fixed before the run
///
/// From `spike/FINDINGS.md` §42, decided in advance: **the mood tag ships if it
/// agrees with a text-sentiment baseline by a margin of 10 percentage points
/// over chance on rows where both have an opinion.** It did not. The numbers
/// are in §42 and the short version is two separate problems:
///
/// 1. **The model abstains.** `<|EMO_UNKNOWN|>` on **74.7%** of 15,366 real
///    turns. A mark that is absent three times in four is not a mark somebody
///    can read a transcript by.
/// 2. **On the quarter it answers, it is worse than a constant guess.** On the
///    1,001 rows where both the tag and the word list have an opinion, they
///    agree **31.6%** of the time — against **86.4%** for a predictor that
///    always says the majority class. The gate was +10 points; the result is
///    **−54.8**. That is not "unproven", it is anti-correlated: the tag says
///    `HAPPY` on 309 rows the words read as positive and `NEUTRAL` on 472 of
///    them, so it is mostly declining on exactly the rows a person would call
///    cheerful.
/// 3. **And the corpus is a language it does not speak.** This archive is
///    German and English; SenseVoice covers zh/en/ja/ko/yue. Its emotion head
///    was trained alongside a decoder that cannot read most of these rows, and
///    what it does on a German sentence is unvalidated by anybody, upstream
///    included. That is the likeliest explanation for (2) and it is a reason to
///    re-measure on a different corpus rather than to ship this one.
///
/// ## Why the column is written anyway
///
/// Because the measurement is the cheap half and the listening is the expensive
/// half. A stored tag can be re-scored next month against a bigger archive, or
/// against a person's own corrections, without opening a single clip again. A
/// column is three bytes a row; a re-listen is the whole archive.
///
/// ## Flipping it
///
/// One `bool`, one place, and everything downstream is already written and
/// already tested against both values — `mood_display` has four states and all
/// four act in both worlds (`gui/test`, `src/main/e2e.js`). What flipping it
/// must *not* be is a config setting: whether a measurement came out is not the
/// operator's opinion, and a switch here would be an invitation to turn on a
/// feature the evidence says is noise.
pub const MOOD_IS_MEASURED: bool = false;

/// The one sentence a surface prints where a mood would have gone. Kept beside
/// the constant so the reason can never drift from the decision.
pub const WHY_MOOD_IS_NOT_SHOWN: &str = "The mood tag is stored but not shown: the model declines to answer on most turns, \
     and on the rest it did not beat a word list by the margin set before the measurement \
     (FINDINGS §42). Laughter and music are shown, and were measured separately.";

/// Rows of somebody's the pass must have read before it will say anything about
/// how they sound.
///
/// **Thirty.** Not a statistical bar — it is the point below which a ratio is a
/// sentence about three clips. A person heard twice, once with the tag and once
/// without, is not somebody who "laughs half the time", and a page that says so
/// has invented a personality out of two rows.
pub const MIN_ROWS_FOR_A_SUMMARY: i64 = 30;

/// Share of a person's read rows that must carry laughter before the summary
/// mentions it at all. **One in twenty.** Below that it is the base rate and
/// says nothing about *them*.
pub const LAUGHTER_NOTABLE: f64 = 0.05;

/// How somebody sounds, in the shape a client renders — or `None`, which is the
/// answer for nearly everybody and is not an error.
///
/// Deliberately *not* a sentence. The daemon hands over counts, a denominator
/// and one boolean per claim it is willing to stand behind, and the client
/// writes the words; a daemon that shipped English prose would have to ship it
/// in every language the GUI is ever read in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    /// Rows the pass has read of theirs. The denominator, always present.
    pub read: i64,
    pub laughter: i64,
    /// `laughter / read`.
    pub laughter_share: f64,
    /// Is the laughter share above [`LAUGHTER_NOTABLE`]? The one claim this
    /// daemon will make about a person from these tags.
    pub laughs: bool,
    /// The commonest mood among the rows the model *did* answer on, and how
    /// many of them there were. Always `None` while [`MOOD_IS_MEASURED`] is
    /// false — the counts are still on the wire, and this is the field that
    /// says whether anything may be read off them.
    pub mood: Option<(Mood, i64)>,
}

/// Turn totals into a summary, or refuse.
///
/// `None` under [`MIN_ROWS_FOR_A_SUMMARY`], which is the common case and the
/// whole point: a person page with nothing to say about how somebody sounds
/// should say nothing, not "0%".
pub fn summary(t: &MoodTotals) -> Option<Summary> {
    if t.read < MIN_ROWS_FOR_A_SUMMARY {
        return None;
    }
    let share = t.laughter as f64 / t.read as f64;
    let mood = MOOD_IS_MEASURED
        .then(|| {
            [
                (Mood::Happy, t.happy),
                (Mood::Sad, t.sad),
                (Mood::Angry, t.angry),
                (Mood::Neutral, t.neutral),
            ]
            .into_iter()
            .filter(|(_, n)| *n > 0)
            .max_by_key(|(_, n)| *n)
        })
        .flatten();
    Some(Summary {
        read: t.read,
        laughter: t.laughter,
        laughter_share: share,
        laughs: share >= LAUGHTER_NOTABLE,
        mood,
    })
}

// ---------------------------------------------------------------------------
// where a client puts it (`[assist] mood_display`)
// ---------------------------------------------------------------------------

/// A chip at the end of the row saying what was heard. **The default.**
pub const DISPLAY_TAGS: &str = "tags";
/// The row's words take the mood's colour, and there is no chip.
pub const DISPLAY_TINT: &str = "tint";
pub const DISPLAY_BOTH: &str = "both";
/// Neither. The tags are still read and still stored — this is about the page,
/// not about the pass, which has its own switch in `[mood]`.
pub const DISPLAY_OFF: &str = "off";

/// The four, in the order the card offers them.
pub const DISPLAYS: &[&str] = &[DISPLAY_TAGS, DISPLAY_TINT, DISPLAY_BOTH, DISPLAY_OFF];

/// The live value of `[assist] mood_display`.
///
/// A static, for the reason [`crate::translate`]'s three are: it is read where
/// a segment is described, and a control that needs a restart is not a control.
/// It lives here rather than beside them because it is not a translation
/// setting — it shares a card with them and nothing else.
static DISPLAY: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());

/// Set it. Anything unrecognised — including unset — reads as
/// [`DISPLAY_TAGS`], so a config file with a typo in it gets the shipped
/// behaviour rather than a blank transcript.
pub fn set_display(mode: &str) {
    *DISPLAY.write().unwrap_or_else(|p| p.into_inner()) = mode.trim().to_ascii_lowercase();
}

pub fn display() -> &'static str {
    let now = DISPLAY.read().unwrap_or_else(|p| p.into_inner()).clone();
    DISPLAYS
        .iter()
        .copied()
        .find(|m| *m == now)
        .unwrap_or(DISPLAY_TAGS)
}

// ---------------------------------------------------------------------------
// the pass
// ---------------------------------------------------------------------------

/// What the worker is doing, as `status` reports it. [`crate::enrich::Phase`]'s
/// vocabulary, because a client already renders those five words.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Phase {
    /// `[mood].enabled` is false. The shipped state.
    #[default]
    Off,
    /// On, but SenseVoice is not installed. `models fetch --cjk` installs it.
    Unavailable,
    /// On and installed, but a gate is closed.
    Blocked,
    /// On, installed, nothing in the way, and nothing left to listen to.
    Idle,
    Running,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Off => "off",
            Phase::Unavailable => "unavailable",
            Phase::Blocked => "blocked",
            Phase::Idle => "idle",
            Phase::Running => "running",
        }
    }
}

/// The counters `status.mood` carries.
#[derive(Debug, Default)]
pub struct MoodStats {
    /// Rows listened to since the daemon started.
    pub read: AtomicU64,
    /// …of which carried a mood the model was willing to name.
    pub with_mood: AtomicU64,
    /// …and of which carried at least one event.
    pub with_event: AtomicU64,
    /// Rows stamped without opening a clip, because retention had taken it.
    pub no_audio: AtomicU64,
    pub last_run_ms: AtomicU64,
    phase: AtomicU64,
}

impl MoodStats {
    pub fn set_phase(&self, p: Phase) {
        self.phase.store(p as u64, Ordering::Relaxed);
    }

    pub fn phase(&self) -> Phase {
        match self.phase.load(Ordering::Relaxed) {
            1 => Phase::Unavailable,
            2 => Phase::Blocked,
            3 => Phase::Idle,
            4 => Phase::Running,
            _ => Phase::Off,
        }
    }
}

/// The stop flag, in the shape every background worker here uses.
#[derive(Debug, Default)]
pub struct MoodStop(AtomicBool);

impl MoodStop {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Why the pass is standing down, or `None` when it may run.
///
/// Re-read **between rows** rather than once per run, which is the night
/// shift's rule and is here for the night shift's reason: the point of a clock
/// is that somebody can take their machine back at 04:00.
///
/// Deliberately **without** a GPU check, unlike the night shift's: nothing here
/// touches the card. And deliberately **with** the pause check, like every
/// other background writer — pause means nothing is written down, and a pass
/// writing derived rows through a pause would make that sentence false.
pub fn gate(control: &Control, cfg: &MoodConfig, minute: u32, idle_min: i64) -> Option<String> {
    if !cfg.enabled {
        return Some("the mood pass is off".to_string());
    }
    if control.is_paused() {
        return Some("capture is paused — nothing is written down, including this".to_string());
    }
    let night = control.night();
    let in_window = crate::night::Hours::parse(&night.window).is_some_and(|h| h.contains(minute));
    let idle_enough = night.also_when_idle_min > 0 && idle_min >= night.also_when_idle_min;
    if !in_window && !idle_enough {
        return Some(format!(
            "outside {} and the machine has been busy within the last {} minutes",
            night.window, night.also_when_idle_min
        ));
    }
    None
}

/// One clip's worth of work, without a store lock and without a database.
///
/// Split out for the reason [`crate::asr_cjk::judge`] is: the whole rule can
/// then be exercised without a 239 MB model, and there is exactly one copy of
/// it.
pub fn listen(asr: &mut CjkAsr, samples: &[f32]) -> Reading {
    let duration_s = samples.len() as f32 / crate::config::SAMPLE_RATE as f32;
    Reading::of(&asr.listen(samples), duration_s)
}

/// The background thread.
///
/// Started whether or not the feature is on, like the night shift's and the
/// sweep's: the switch is live and something has to be watching it.
#[allow(clippy::too_many_arguments)]
pub fn run(
    store: Arc<Mutex<Store>>,
    control: Arc<Control>,
    bus: Arc<Bus>,
    models_root: Option<PathBuf>,
    data_dir: PathBuf,
    cfg: crate::config::Config,
    stats: Arc<MoodStats>,
    stop: Arc<MoodStop>,
) {
    crate::pipeline::background_current_thread(
        cfg.runtime.inference_nice,
        &cfg.runtime.inference_cpus,
    );
    let Some(root) = models_root else {
        debug!("no models root: the mood pass will not run");
        return;
    };
    let models = ModelSet::resolve_at(root, &cfg.models);
    let mut said = String::new();
    let mut said_unavailable = false;

    loop {
        if stop.stopped() {
            debug!("the mood pass stopped");
            return;
        }
        let mood_cfg = control.mood();
        match one_pass(
            &store,
            &control,
            &bus,
            &mood_cfg,
            &models,
            &data_dir,
            &stats,
            &stop,
            &mut said,
            &mut said_unavailable,
        ) {
            Ok(()) => {}
            Err(e) => warn!("a mood pass failed: {e:#}"),
        }
        // Nothing here is urgent. A minute between looks is invisible on a
        // sleeping machine and free on a busy one.
        let step = std::time::Duration::from_millis(250);
        let mut slept = std::time::Duration::ZERO;
        while slept < std::time::Duration::from_secs(60) {
            if stop.stopped() {
                break;
            }
            std::thread::sleep(step);
            slept += step;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn one_pass(
    store: &Arc<Mutex<Store>>,
    control: &Arc<Control>,
    bus: &Bus,
    cfg: &MoodConfig,
    models: &ModelSet,
    data_dir: &Path,
    stats: &MoodStats,
    stop: &MoodStop,
    said: &mut String,
    said_unavailable: &mut bool,
) -> Result<()> {
    if let Some(reason) = gate(control, cfg, local_minute_now(), control.idle_minutes()) {
        stats.set_phase(if cfg.enabled {
            Phase::Blocked
        } else {
            Phase::Off
        });
        if *said != reason {
            debug!("the mood pass is standing down: {reason}");
            *said = reason;
        }
        return Ok(());
    }
    said.clear();

    let model = models.sense_voice();
    if !model.present() {
        stats.set_phase(Phase::Unavailable);
        if !*said_unavailable {
            info!(
                "the mood pass needs the SenseVoice decoder: {}",
                CjkModel::how_to_get_it(&[crate::asr_cjk::KO, crate::asr_cjk::ZH])
            );
            *said_unavailable = true;
        }
        return Ok(());
    }
    *said_unavailable = false;

    // Nothing to do is the normal state once the archive is caught up, and it
    // is checked BEFORE the model is loaded: an idle pass that paid 239 MB a
    // minute to discover it had nothing to listen to would be the worst kind of
    // background job.
    let first = {
        let guard = store.lock().unwrap_or_else(|p| p.into_inner());
        guard.segments_for_mood(cfg.min_duration_s, cfg.batch_rows.max(1))?
    };
    if first.is_empty() {
        stats.set_phase(Phase::Idle);
        return Ok(());
    }

    stats.set_phase(Phase::Running);
    let started = std::time::Instant::now();
    // Loaded here and dropped at the end of this function — see the module
    // note. The one place in this daemon where an optional model is deliberately
    // NOT resident: this pass runs to completion and then has nothing to do
    // forever, unlike the CJK route, which may be asked about any turn.
    let mut asr = match CjkAsr::load(&model, models.asr_threads) {
        Ok(asr) => {
            info!(model = asr.model_id(), "the mood pass loaded SenseVoice");
            asr
        }
        Err(e) => {
            stats.set_phase(Phase::Unavailable);
            warn!("the mood pass could not load SenseVoice: {e:#}");
            return Ok(());
        }
    };

    let mut done = 0usize;
    let mut batch = first;
    while done < cfg.rows_per_run {
        if batch.is_empty() {
            stats.set_phase(Phase::Idle);
            break;
        }
        for row in batch.drain(..) {
            if done >= cfg.rows_per_run {
                break;
            }
            // Between rows, not between runs. A person who sits back down gets
            // their cores back within one clip.
            if stop.stopped()
                || gate(control, cfg, local_minute_now(), control.idle_minutes()).is_some()
            {
                stats
                    .last_run_ms
                    .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                return Ok(());
            }
            done += 1;
            // ---- read the clip and decode, with no lock held ----
            let samples = crate::ingest::read_wav(&data_dir.join(&row.audio_path)).ok();
            let reading = match samples {
                Some(s) if !s.is_empty() => Some(listen(&mut asr, &s)),
                // Retention has taken the audio, or it never landed. The row is
                // still stamped, so the queue moves on: there is nothing here
                // to listen to and there never will be.
                _ => {
                    stats.no_audio.fetch_add(1, Ordering::Relaxed);
                    None
                }
            };
            // ---- commit, with the lock held and no model running ----
            let at = utc_now_ns();
            let changed = {
                let guard = store.lock().unwrap_or_else(|p| p.into_inner());
                match &reading {
                    Some(r) => {
                        let events = r.events_column();
                        guard.set_segment_mood(row.id, r.mood_column(), events.as_deref(), at)?;
                        if r.mood.is_some() {
                            stats.with_mood.fetch_add(1, Ordering::Relaxed);
                        }
                        if !r.events.is_empty() {
                            stats.with_event.fetch_add(1, Ordering::Relaxed);
                        }
                        // A row the model had nothing to say about is a row
                        // whose rendering did not change, so no client is woken
                        // for it. On this corpus that is three rows in four.
                        r.mood.is_some() || !r.events.is_empty()
                    }
                    None => {
                        guard.set_segment_mood(row.id, None, None, at)?;
                        false
                    }
                }
            };
            stats.read.fetch_add(1, Ordering::Relaxed);
            if changed {
                let guard = store.lock().unwrap_or_else(|p| p.into_inner());
                crate::pipeline::publish_segment(bus, &guard, row.id);
            }
        }
        batch = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            guard.segments_for_mood(cfg.min_duration_s, cfg.batch_rows.max(1))?
        };
    }
    stats
        .last_run_ms
        .store(started.elapsed().as_millis() as u64, Ordering::Relaxed);
    if done > 0 {
        info!(rows = done, "the mood pass listened to some rows");
    }
    Ok(())
}

/// Local minute of day. The night shift's own, which is private to it;
/// duplicated here for the reason `crate::sweep` duplicates it — four lines,
/// and making it public would suggest it is an interface.
fn local_minute_now() -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let local = now + crate::clock::local_offset_s(now * 1_000_000_000);
    (local.rem_euclid(86_400) / 60) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn heard(emotion: &str, event: &str) -> Heard {
        Heard {
            text: "whatever".into(),
            lang: "<|de|>".into(),
            emotion: emotion.into(),
            event: event.into(),
        }
    }

    #[test]
    fn the_vocabulary_is_closed_and_abstention_is_not_a_mood() {
        assert_eq!(Mood::parse("<|HAPPY|>"), Some(Mood::Happy));
        assert_eq!(Mood::parse("HAPPY"), Some(Mood::Happy));
        assert_eq!(Mood::parse("happy"), Some(Mood::Happy));
        assert_eq!(Mood::parse("<|NEUTRAL|>"), Some(Mood::Neutral));
        assert_eq!(Mood::parse("<|ANGRY|>"), Some(Mood::Angry));
        assert_eq!(Mood::parse("<|SAD|>"), Some(Mood::Sad));
        // The model declining to answer is not a fifth mood.
        assert_eq!(Mood::parse("<|EMO_UNKNOWN|>"), None);
        // Neither is a mood this build does not store, nor an empty field —
        // which is what the Japanese Parakeet gives, having no emotion head.
        assert_eq!(Mood::parse("<|FEARFUL|>"), None);
        assert_eq!(Mood::parse("<|SURPRISED|>"), None);
        assert_eq!(Mood::parse(""), None);
        assert_eq!(Mood::ALL.len(), 4);
    }

    #[test]
    fn speech_is_not_an_event_and_bgm_is_called_music() {
        assert_eq!(Event::parse("<|Laughter|>"), Some(Event::Laughter));
        assert_eq!(Event::parse("<|BGM|>"), Some(Event::Music));
        assert_eq!(Event::parse("music"), Some(Event::Music));
        assert_eq!(Event::parse("<|Applause|>"), Some(Event::Applause));
        assert_eq!(Event::parse("<|Cry|>"), Some(Event::Cry));
        // On 97.5% of turns, so it is not a mark.
        assert_eq!(Event::parse("<|Speech|>"), None);
        assert_eq!(Event::parse("<|Event_UNK|>"), None);
        assert_eq!(Event::parse(""), None);
    }

    #[test]
    fn a_reading_is_a_sorted_set_and_an_empty_one_is_null() {
        let r = Reading::of(&heard("<|HAPPY|>", "<|Laughter|>"), 2.0);
        assert_eq!(r.mood, Some(Mood::Happy));
        assert_eq!(r.events, vec![Event::Laughter]);
        assert_eq!(r.mood_column(), Some("happy"));
        assert_eq!(r.events_column().as_deref(), Some("laughter"));
        assert!(r.laughed());

        // Two events on one clip come back sorted and joined, so two rows with
        // the same events are the same string and a LIKE finds either.
        let both = Reading::of(&heard("<|NEUTRAL|>", "<|BGM|><|Laughter|>"), 2.0);
        assert_eq!(both.events, vec![Event::Laughter, Event::Music]);
        assert_eq!(both.events_column().as_deref(), Some("laughter,music"));

        // The ordinary turn: speech, and a model with no opinion. NULL in both
        // columns, and NOT the empty string — one representation of nothing.
        let plain = Reading::of(&heard("<|EMO_UNKNOWN|>", "<|Speech|>"), 2.0);
        assert_eq!(plain.mood, None);
        assert!(plain.events.is_empty());
        assert_eq!(plain.mood_column(), None);
        assert_eq!(plain.events_column(), None);
        assert!(!plain.laughed());

        // And the Parakeet, which fills neither field.
        assert_eq!(Reading::of(&Heard::default(), 2.0), Reading::default());
    }

    /// The round's one surprising measurement, as a rule (see
    /// [`EVENT_MEASURED_MAX_S`]): past five seconds the event tag is below
    /// chance, so it is not stored — and the MOOD, which nothing draws, is.
    #[test]
    fn an_event_on_a_long_clip_is_not_stored_and_the_mood_still_is() {
        let said = heard("<|HAPPY|>", "<|Laughter|>");

        // Inside the band, and on the boundary itself, which is inclusive: the
        // 2.5–5.0s bucket is the strongest one measured (9.76x), so five
        // seconds belongs on the side that keeps the tag.
        for d in [1.0, 2.4, 4.9, EVENT_MEASURED_MAX_S] {
            let r = Reading::of(&said, d);
            assert!(r.laughed(), "a {d}s clip lost its laughter");
            assert_eq!(r.mood, Some(Mood::Happy));
        }

        // Past it: no event, and the reason is in the constant's table — a
        // seventeen-second turn is a paragraph with a laugh somewhere in it,
        // and a chip on the row would claim the turn WAS laughter.
        for d in [5.1, 12.0, 17.7] {
            let r = Reading::of(&said, d);
            assert!(
                !r.laughed(),
                "a {d}s clip kept a tag the measurement refuses"
            );
            assert!(r.events.is_empty());
            assert_eq!(r.events_column(), None);
            // …and the mood is untouched, which is the asymmetry the constant
            // argues for: nothing draws it, so storing it costs nothing and
            // buys the next measurement a row to look at.
            assert_eq!(r.mood, Some(Mood::Happy), "the mood was bounded too");
            assert_eq!(r.mood_column(), Some("happy"));
        }
    }

    #[test]
    fn a_stored_events_string_reads_back_and_tolerates_a_newer_daemon() {
        assert_eq!(
            parse_events(Some("laughter,music")),
            vec![Event::Laughter, Event::Music]
        );
        assert_eq!(parse_events(Some("music")), vec![Event::Music]);
        assert_eq!(parse_events(None), vec![]);
        assert_eq!(parse_events(Some("")), vec![]);
        // A fifth event from a newer build drops out; the rest of the row still
        // renders, which is `palette::accent`'s rule for an unknown colour.
        assert_eq!(parse_events(Some("laughter,sneeze")), vec![Event::Laughter]);
        assert_eq!(parse_events(Some("sneeze")), vec![]);
    }

    #[test]
    fn a_summary_needs_enough_rows_to_be_about_a_person() {
        let few = MoodTotals {
            read: 4,
            laughter: 2,
            ..Default::default()
        };
        assert_eq!(summary(&few), None, "two clips is not a personality");

        let quiet = MoodTotals {
            read: 200,
            laughter: 3,
            ..Default::default()
        };
        let s = summary(&quiet).expect("enough rows");
        assert_eq!(s.read, 200);
        assert!(!s.laughs, "1.5% is the base rate, not a fact about them");

        let loud = MoodTotals {
            read: 200,
            laughter: 24,
            happy: 40,
            neutral: 60,
            ..Default::default()
        };
        let s = summary(&loud).expect("enough rows");
        assert!(s.laughs);
        assert!((s.laughter_share - 0.12).abs() < 1e-9);
        // The mood half is silent while the measurement says it should be,
        // even though the counts that would fill it are right there.
        assert_eq!(s.mood.is_some(), MOOD_IS_MEASURED);
    }

    #[test]
    fn the_mood_verdict_is_a_constant_and_it_carries_its_reason() {
        // Not an assertion about which way it went — an assertion that the two
        // cannot drift apart. A build that flips the constant and leaves the
        // sentence saying the opposite is a build that lies to the person
        // reading the settings card.
        if !MOOD_IS_MEASURED {
            assert!(WHY_MOOD_IS_NOT_SHOWN.contains("not shown"));
            assert!(WHY_MOOD_IS_NOT_SHOWN.contains("§42"));
        }
    }

    #[test]
    fn the_gate_is_the_switch_the_pause_and_the_clock() {
        let control = Control::new(
            std::path::PathBuf::from("/nonexistent/nx-recall-mood"),
            None,
            &crate::allowlist::Allowlist::from_rules([("VRChat.exe", true)]),
        );
        let mut cfg = MoodConfig::default();
        // Off is the shipped state and it is the first thing checked, before
        // the clock: a daemon that logged "outside 03:00-07:00" for a feature
        // nobody turned on would be answering a question nobody asked.
        assert_eq!(
            gate(&control, &cfg, 4 * 60, 999).as_deref(),
            Some("the mood pass is off")
        );
        cfg.enabled = true;
        // Inside the night window: nothing in the way.
        assert_eq!(gate(&control, &cfg, 4 * 60, 0), None);
        // Outside it, on a busy machine: stand down.
        assert!(gate(&control, &cfg, 20 * 60, 0).is_some());
        // Outside it, on an idle one: run. This is the clause that gets the
        // archive read on a machine that is never asleep at 03:00.
        assert_eq!(gate(&control, &cfg, 20 * 60, 60), None);
        // And a pause beats all of it.
        control.pause();
        assert!(
            gate(&control, &cfg, 4 * 60, 999)
                .unwrap()
                .contains("paused")
        );
    }
}
