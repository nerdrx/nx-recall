//! One query box: turning a question into facets plus the words left over
//! (PROTOCOL "0.8.0 — the accuracy round", `search.ask`).
//!
//! A person does not search a transcript, they ask it something: *"was hat
//! Aspen gestern über den Shader gesagt?"*. Three different things are in that
//! sentence — a person, a time window and a subject — and only the third is a
//! search term. Feeding the whole sentence to FTS finds nothing, because
//! `segments_fts MATCH` ANDs its words and no turn contains "gestern".
//!
//! So this module reads the question apart:
//!
//! 1. **A time reference** becomes `from_ns`/`to_ns`.
//! 2. **A named speaker** becomes `speaker_id`.
//! 3. **Everything that is left**, minus the scaffolding words a question is
//!    made of, is the query.
//!
//! The result is handed back to the client as `interpretation`, which is the
//! whole point: a parser that guesses silently is a parser nobody can correct.
//! The GUI shows what was understood and lets a facet be dropped.
//!
//! ### Why this is not [`crate::timeref`]
//!
//! `timeref` resolves what a *speaker* said into the future: "Freitag" is the
//! Friday that has not happened yet, because the feature it serves is what is
//! still owed. A question is the mirror image — "am Montag" in a search box is
//! the Monday that already happened, and there is no reading of "gestern" at
//! all in a forward parser. The two share a calendar and nothing else, so this
//! is a second, deliberately retrospective table rather than a flag on the
//! first. Both fold de and en, both resolve against the machine's local day,
//! and both are Tier-2 rules: cheap, deterministic, wrong sometimes, and shown
//! to the user rather than applied behind their back.
//!
//! Everything here is pure — a question, a `now`, and the list of named voices
//! go in; an [`Interpretation`] comes out. No database, no models.

use crate::clock::{civil_from_days, days_from_civil, local_offset_s};

const SEC: i64 = 1_000_000_000;
const DAY_S: i64 = 86_400;

/// Which engine answered, reported so a client can say so.
pub mod mode {
    /// FTS and the vector leg, fused — the semantic model is installed.
    pub const HYBRID: &str = "hybrid";
    /// Keyword search alone: there is no semantic model on this machine.
    pub const FTS: &str = "fts";
    /// There were no words left after the facets were taken out ("what did
    /// Aspen say yesterday?"), so the facets themselves are the query and the
    /// answer is a slice of transcript rather than a ranked list.
    pub const FACETS: &str = "facets";
}

/// A voice a question may name. Only *named* speakers are offered: an
/// auto-label (`Speaker_07`) is not something anybody types into a question,
/// and matching one fuzzily would turn every stray number into a facet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub id: i64,
    pub label: String,
}

/// What the daemon understood. Every field is optional because every facet is
/// optional, and the client is shown all of them so it can disagree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Interpretation {
    /// The residual words, in the order they were asked. Empty when the whole
    /// question was facets.
    pub query: String,
    pub speaker_id: Option<i64>,
    /// The speaker's display name as it is spelled in the voicebank, not as it
    /// was typed: the point of showing it back is to confirm *who* was matched.
    pub speaker_label: Option<String>,
    /// Inclusive lower bound, UTC nanoseconds.
    pub from_ns: Option<i64>,
    /// Exclusive upper bound, UTC nanoseconds.
    pub to_ns: Option<i64>,
}

/// Read a question apart, relative to `now_ns`.
pub fn parse(question: &str, now_ns: i64, named: &[Named]) -> Interpretation {
    let mut tokens = tokenize(question);
    let mut out = Interpretation::default();

    // Time first: it is the only facet whose phrases contain words a name
    // could not be ("gestern", "last week"), so taking it out first cannot
    // steal a token from a speaker match.
    if let Some((from, to, span)) = match_time(&tokens, now_ns) {
        out.from_ns = Some(from);
        out.to_ns = Some(to);
        for t in &mut tokens[span.0..span.1] {
            t.taken = true;
        }
    }

    // Then the speaker, over what is left.
    if let Some((who, span)) = match_speaker(&tokens, named) {
        out.speaker_id = Some(who.id);
        out.speaker_label = Some(who.label.clone());
        for t in &mut tokens[span.0..span.1] {
            t.taken = true;
        }
    }

    // Then the scaffolding. A question is mostly question — "was hat … über …
    // gesagt" is six words of grammar around one word of content — and every
    // one of them ANDed into an FTS query is a guaranteed empty result.
    let residual: Vec<&str> = tokens
        .iter()
        .filter(|t| !t.taken && !is_scaffolding(&t.folded))
        .map(|t| &question[t.start..t.end])
        .collect();
    out.query = residual.join(" ");
    out
}

/// The residual words as an FTS5 expression: every token quoted, so an
/// apostrophe or a stray `*` in a question is a word to look for rather than
/// syntax to trip over. Every token and no others — the `interpretation` a
/// client is shown has to be the query that actually ran.
///
/// Empty in, empty out, which is the caller's signal to fall back to the
/// facets alone.
pub fn fts_expression(query: &str) -> String {
    query
        .split_whitespace()
        .map(|w| format!("\"{}\"", w.replace('"', "")))
        .collect::<Vec<_>>()
        .join(" ")
}

// ---------------------------------------------------------------------------
// tokens
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Token {
    start: usize,
    end: usize,
    /// Lower-cased, umlauts folded, apostrophes kept — the possessive is a
    /// thing the matcher wants to see.
    folded: String,
    taken: bool,
}

fn tokenize(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, ch) in text.char_indices() {
        let wordish = ch.is_alphanumeric() || ch == '_' || ch == '\'' || ch == '\u{2019}';
        match (wordish, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push(token(text, s, i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push(token(text, s, text.len()));
    }
    out
}

fn token(text: &str, start: usize, end: usize) -> Token {
    Token {
        start,
        end,
        folded: fold(&text[start..end]),
        taken: false,
    }
}

/// Lower-case, with the German umlauts and `ß` written out the way a keyboard
/// without them would. Not byte-length preserving — nothing here indexes back
/// into the folded string.
///
/// `pub(crate)` for [`crate::notes`], which folds a wake phrase the same way
/// and must not grow a second answer to "is this the same word".
pub(crate) fn fold_word(s: &str) -> String {
    fold(s)
}

fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            'ä' | 'Ä' => out.push_str("ae"),
            'ö' | 'Ö' => out.push_str("oe"),
            'ü' | 'Ü' => out.push_str("ue"),
            'ß' => out.push_str("ss"),
            '\u{2019}' => out.push('\''),
            other => out.extend(other.to_lowercase()),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// time, backwards
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum When {
    /// A whole local day, `n` days before today.
    DaysAgo(i64),
    /// Yesterday evening through this morning — the one window people mean by
    /// "gestern Abend" that is not a calendar day.
    LastNight,
    ThisMorning,
    /// Whole ISO weeks, Monday to Monday. 0 = this one, 1 = the last one.
    WeeksAgo(i64),
    /// Whole calendar months. 0 = this one, 1 = the last one.
    MonthsAgo(i64),
    /// The most recent occurrence of a weekday, today included. 0 = Sunday.
    Weekday(i64),
    /// A rolling window ending now.
    LastSeconds(i64),
}

/// De and en, longest phrase first. Every entry is *retrospective*: a question
/// is asked about what has already been said.
const PHRASES: &[(&str, When)] = &[
    // rolling windows
    ("in der letzten stunde", When::LastSeconds(3600)),
    ("in the last hour", When::LastSeconds(3600)),
    ("letzte stunde", When::LastSeconds(3600)),
    ("last hour", When::LastSeconds(3600)),
    ("die letzten 24 stunden", When::LastSeconds(24 * 3600)),
    ("last 24 hours", When::LastSeconds(24 * 3600)),
    // days
    ("vorgestern", When::DaysAgo(2)),
    ("day before yesterday", When::DaysAgo(2)),
    ("gestern abend", When::LastNight),
    ("gestern nacht", When::LastNight),
    ("letzte nacht", When::LastNight),
    ("yesterday evening", When::LastNight),
    ("yesterday night", When::LastNight),
    ("last night", When::LastNight),
    ("gestern", When::DaysAgo(1)),
    ("yesterday", When::DaysAgo(1)),
    ("heute morgen", When::ThisMorning),
    ("heute frueh", When::ThisMorning),
    ("this morning", When::ThisMorning),
    ("heute", When::DaysAgo(0)),
    ("today", When::DaysAgo(0)),
    // weeks
    ("in der letzten woche", When::WeeksAgo(1)),
    ("in der vergangenen woche", When::WeeksAgo(1)),
    ("letzte woche", When::WeeksAgo(1)),
    ("letzten woche", When::WeeksAgo(1)),
    ("vergangene woche", When::WeeksAgo(1)),
    ("vorige woche", When::WeeksAgo(1)),
    ("last week", When::WeeksAgo(1)),
    ("diese woche", When::WeeksAgo(0)),
    ("dieser woche", When::WeeksAgo(0)),
    ("this week", When::WeeksAgo(0)),
    // months
    ("letzten monat", When::MonthsAgo(1)),
    ("letzter monat", When::MonthsAgo(1)),
    ("vergangenen monat", When::MonthsAgo(1)),
    ("last month", When::MonthsAgo(1)),
    ("diesen monat", When::MonthsAgo(0)),
    ("this month", When::MonthsAgo(0)),
];

/// The weekday half, kept apart because it is a cross product: seven days
/// times the handful of words that can sit in front of one.
const WEEKDAYS: &[(&str, i64)] = &[
    ("sonntag", 0),
    ("sunday", 0),
    ("montag", 1),
    ("monday", 1),
    ("dienstag", 2),
    ("tuesday", 2),
    ("mittwoch", 3),
    ("wednesday", 3),
    ("donnerstag", 4),
    ("thursday", 4),
    ("freitag", 5),
    ("friday", 5),
    ("samstag", 6),
    ("sonnabend", 6),
    ("saturday", 6),
];

/// Words that may introduce a weekday and are swallowed with it, so "am
/// Montag" is one reference rather than a reference plus a stray preposition.
const WEEKDAY_LEAD: &[&str] = &[
    "am",
    "an",
    "letzten",
    "letzte",
    "vergangenen",
    "vorigen",
    "on",
    "last",
    "this",
];

/// The first time reference in the question, as `(from, to, token span)`.
fn match_time(tokens: &[Token], now_ns: i64) -> Option<(i64, i64, (usize, usize))> {
    let cal = Cal::new(now_ns);
    for i in 0..tokens.len() {
        // Longest phrase wins at each position, so "gestern abend" is never
        // read as "gestern" with a word after it.
        for len in (1..=5).rev() {
            let Some(end) = window_end(tokens, i, len) else {
                continue;
            };
            let phrase = joined(tokens, i, end);
            if let Some((_, when)) = PHRASES.iter().find(|(p, _)| *p == phrase) {
                let (from, to) = cal.window(*when, now_ns);
                return Some((from, to, (i, end)));
            }
        }
        // A weekday, with or without the word in front of it.
        for (lead, first) in [(true, i + 1), (false, i)] {
            if lead && (tokens.len() <= i + 1 || !WEEKDAY_LEAD.contains(&tokens[i].folded.as_str()))
            {
                continue;
            }
            if let Some(t) = tokens.get(first)
                && let Some((_, w)) = WEEKDAYS.iter().find(|(n, _)| *n == t.folded)
            {
                let (from, to) = cal.window(When::Weekday(*w), now_ns);
                return Some((from, to, (i, first + 1)));
            }
        }
    }
    None
}

fn window_end(tokens: &[Token], start: usize, len: usize) -> Option<usize> {
    let end = start + len;
    (end <= tokens.len()).then_some(end)
}

fn joined(tokens: &[Token], start: usize, end: usize) -> String {
    tokens[start..end]
        .iter()
        .map(|t| t.folded.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Local-day arithmetic, the same trick [`crate::timeref`] plays: the offset is
/// read once, for `now`, and every answer is written back as UTC nanoseconds.
struct Cal {
    offset_s: i64,
}

impl Cal {
    fn new(now_ns: i64) -> Self {
        Self {
            offset_s: local_offset_s(now_ns),
        }
    }

    fn today(&self, now_ns: i64) -> i64 {
        (now_ns.div_euclid(SEC) + self.offset_s).div_euclid(DAY_S)
    }

    /// UTC nanoseconds at the start of a local day, plus `tod` seconds.
    fn at(&self, day: i64, tod: i64) -> i64 {
        (day * DAY_S + tod - self.offset_s) * SEC
    }

    fn day(&self, day: i64) -> (i64, i64) {
        (self.at(day, 0), self.at(day + 1, 0))
    }

    fn window(&self, when: When, now_ns: i64) -> (i64, i64) {
        let today = self.today(now_ns);
        match when {
            When::DaysAgo(n) => self.day(today - n),
            // 18:00 yesterday to 06:00 today. Not a calendar day and
            // deliberately so — "was hat sie gestern Abend gesagt" is about an
            // evening that ran past midnight as often as not.
            When::LastNight => (self.at(today - 1, 18 * 3600), self.at(today, 6 * 3600)),
            When::ThisMorning => (self.at(today, 0), self.at(today, 12 * 3600)),
            When::WeeksAgo(n) => {
                let monday = today - (weekday_of(today) + 6).rem_euclid(7) - 7 * n;
                (self.at(monday, 0), self.at(monday + 7, 0))
            }
            When::MonthsAgo(n) => {
                let (y, m, _) = civil_from_days(today);
                let months = y * 12 + (m as i64 - 1) - n;
                let (y0, m0) = (months.div_euclid(12), months.rem_euclid(12) as u32 + 1);
                let start = days_from_civil(y0, m0, 1);
                let next = months + 1;
                let end = days_from_civil(next.div_euclid(12), next.rem_euclid(12) as u32 + 1, 1);
                (self.at(start, 0), self.at(end, 0))
            }
            // Today counts: a question asked on a Monday about "Monday" is
            // about this morning far more often than about a week ago. The
            // mirror of `timeref`'s rule, and for the mirror reason.
            When::Weekday(target) => {
                let back = (weekday_of(today) - target).rem_euclid(7);
                self.day(today - back)
            }
            When::LastSeconds(s) => (now_ns - s * SEC, now_ns),
        }
    }
}

/// 0 = Sunday. 1970-01-01 was a Thursday, hence the +4.
fn weekday_of(day: i64) -> i64 {
    (day + 4).rem_euclid(7)
}

// ---------------------------------------------------------------------------
// speakers
// ---------------------------------------------------------------------------

/// The named voice this question is about, as `(voice, token span)`.
///
/// Longest label first, so "Aspen Wren" beats "Aspen" when both are in the
/// voicebank and both are in the question.
fn match_speaker<'a>(tokens: &[Token], named: &'a [Named]) -> Option<(&'a Named, (usize, usize))> {
    let mut candidates: Vec<(&Named, Vec<String>)> = named
        .iter()
        .map(|n| {
            (
                n,
                n.label
                    .split_whitespace()
                    .map(fold)
                    .filter(|w| !w.is_empty())
                    .collect::<Vec<_>>(),
            )
        })
        .filter(|(_, words)| !words.is_empty())
        .collect();
    candidates.sort_by_key(|a| std::cmp::Reverse(a.1.len()));

    // Two passes: an exact match anywhere in the question beats a fuzzy one,
    // so a typo in a question that also names somebody exactly cannot win.
    for fuzzy in [false, true] {
        for (who, words) in &candidates {
            for i in 0..tokens.len() {
                let end = i + words.len();
                if end > tokens.len() || tokens[i..end].iter().any(|t| t.taken) {
                    continue;
                }
                if tokens[i..end]
                    .iter()
                    .zip(words)
                    .all(|(t, w)| name_matches(&t.folded, w, fuzzy))
                {
                    return Some((who, (i, end)));
                }
            }
        }
    }
    None
}

/// Does one token name one word of a display name?
///
/// Possessives on both sides of the language: "Aspens" (German genitive),
/// "Aspen's" and the curly-quote form typing gives you. `von Aspen` needs
/// nothing special — "von" is scaffolding and falls out on its own.
fn name_matches(token: &str, word: &str, fuzzy: bool) -> bool {
    let stem = token
        .strip_suffix("'s")
        .or_else(|| token.strip_suffix('\''))
        .unwrap_or(token);
    if stem == word {
        return true;
    }
    if let Some(bare) = stem.strip_suffix('s')
        && bare == word
    {
        return true;
    }
    // A four-letter name is one edit away from too many other words to be
    // worth guessing at; "Kira" must not be found by "Kiro".
    fuzzy && word.chars().count() >= 5 && within_one(stem, word)
}

/// True when `a` and `b` are at most one insertion, deletion or substitution
/// apart. Bounded — the interesting case is a name, or a wake word the ASR
/// nearly heard ([`crate::notes`]).
pub(crate) fn within_one(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let (long, short) = if a.len() >= b.len() {
        (&a, &b)
    } else {
        (&b, &a)
    };
    let (mut i, mut j, mut slack) = (0, 0, true);
    while i < long.len() && j < short.len() {
        if long[i] == short[j] {
            i += 1;
            j += 1;
            continue;
        }
        if !slack {
            return false;
        }
        slack = false;
        if long.len() == short.len() {
            i += 1;
            j += 1;
        } else {
            i += 1;
        }
    }
    slack || long.len() - i + short.len() - j == 0
}

// ---------------------------------------------------------------------------
// scaffolding
// ---------------------------------------------------------------------------

/// The words a question is built out of, in both languages: interrogatives,
/// auxiliaries, articles, pronouns, prepositions, and the verbs of saying that
/// every "what did X say about Y" is made of.
///
/// **Language words are in here on purpose.** "auf Deutsch" in a question is
/// not a facet — there is no `lang` in the contract's `interpretation`, and
/// inventing one would filter a search by something the user never asked to
/// filter by. It is not a search term either: no turn contains the word
/// "Deutsch" because it was spoken in German. So it is scaffolding, and it
/// falls out with the rest of the grammar.
const SCAFFOLDING: &[&str] = &[
    // de — interrogatives and their compounds
    "was",
    "wer",
    "wen",
    "wem",
    "wessen",
    "wann",
    "wo",
    "wie",
    "warum",
    "wieso",
    "weshalb",
    "worueber",
    "worum",
    "wovon",
    "welche",
    "welcher",
    "welches",
    "welchen",
    // de — auxiliaries and the verbs of saying
    "hat",
    "hatte",
    "hatten",
    "habe",
    "haben",
    "hast",
    "ist",
    "sind",
    "war",
    "waren",
    "wird",
    "wurde",
    "wurden",
    "sagt",
    "sagte",
    "sagten",
    "gesagt",
    "sagen",
    "erzaehlt",
    "erzaehlte",
    "meinte",
    "meint",
    "gemeint",
    "spricht",
    "sprach",
    "gesprochen",
    "sprechen",
    "geredet",
    "redet",
    "erwaehnt",
    "erwaehnte",
    // de — articles, pronouns, prepositions, filler
    "der",
    "die",
    "das",
    "den",
    "dem",
    "des",
    "ein",
    "eine",
    "einen",
    "einem",
    "einer",
    "eines",
    "und",
    "oder",
    "aber",
    "ueber",
    "von",
    "vom",
    "zu",
    "zum",
    "zur",
    "auf",
    "in",
    "im",
    "an",
    "am",
    "bei",
    "beim",
    "mit",
    "fuer",
    "als",
    "dass",
    "da",
    "doch",
    "denn",
    "mal",
    "nochmal",
    "eigentlich",
    "ich",
    "du",
    "er",
    "sie",
    "es",
    "wir",
    "ihr",
    "mich",
    "mir",
    "uns",
    "euch",
    "ihm",
    "ihn",
    "man",
    "etwas",
    "irgendwas",
    "nochmals",
    // en — interrogatives, auxiliaries, the verbs of saying
    "what",
    "who",
    "whom",
    "whose",
    "when",
    "where",
    "why",
    "how",
    "which",
    "did",
    "does",
    "do",
    "is",
    "are",
    "were",
    "has",
    "have",
    "had",
    "say",
    "says",
    "said",
    "saying",
    "tell",
    "tells",
    "told",
    "talk",
    "talks",
    "talked",
    "talking",
    "mention",
    "mentions",
    "mentioned",
    "speak",
    "spoke",
    "spoken",
    // en — articles, pronouns, prepositions, filler
    "the",
    "a",
    "an",
    "and",
    "or",
    "but",
    "about",
    "of",
    "to",
    "in",
    "on",
    "at",
    "for",
    "with",
    "from",
    "that",
    "this",
    "these",
    "those",
    "i",
    "me",
    "my",
    "we",
    "us",
    "you",
    "your",
    "he",
    "him",
    "she",
    "her",
    "it",
    "they",
    "them",
    "anything",
    "something",
    "again",
    "ever",
    // the language words, which are neither a facet nor a search term
    "deutsch",
    "deutsche",
    "deutschen",
    "german",
    "englisch",
    "englische",
    "english",
    "sprache",
    "language",
];

fn is_scaffolding(folded: &str) -> bool {
    SCAFFOLDING.contains(&folded)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-03-11 was a Wednesday. Local noon on the test machine, whatever
    /// timezone that is: everything below is asserted against the same local
    /// calendar the parser uses, never against a hardcoded UTC instant.
    fn now() -> i64 {
        let day = days_from_civil(2026, 3, 11);
        (day * DAY_S + 12 * 3600 - local_offset_s(day * DAY_S * SEC)) * SEC
    }

    /// Start of a local day, `delta` days from the day `now()` falls on —
    /// computed here rather than borrowed from `Cal`, so the table below is an
    /// independent statement of what the windows should be.
    fn midnight(delta: i64) -> i64 {
        let offset = local_offset_s(now());
        let today = (now().div_euclid(SEC) + offset).div_euclid(DAY_S);
        ((today + delta) * DAY_S - offset) * SEC
    }

    fn roster() -> Vec<Named> {
        vec![
            Named {
                id: 7,
                label: "Aspen".into(),
            },
            Named {
                id: 9,
                label: "Kira".into(),
            },
            Named {
                id: 11,
                label: "Aspen Wren".into(),
            },
        ]
    }

    fn ask(q: &str) -> Interpretation {
        parse(q, now(), &roster())
    }

    /// The shipped parser table: a question, and what the daemon is contracted
    /// to have understood. `now` is Wednesday 2026-03-11, local noon.
    #[test]
    fn the_question_table() {
        struct Case {
            q: &'static str,
            speaker: Option<i64>,
            from: Option<i64>,
            to: Option<i64>,
            query: &'static str,
        }
        let cases = [
            // --- German -------------------------------------------------
            Case {
                q: "was hat Aspen gestern über den Shader gesagt?",
                speaker: Some(7),
                from: Some(midnight(-1)),
                to: Some(midnight(0)),
                query: "Shader",
            },
            Case {
                q: "Aspens Meinung zum Portal letzte Woche",
                speaker: Some(7),
                // Wednesday 2026-03-11 → this week's Monday is the 9th, so
                // last week is Monday the 2nd to Monday the 9th.
                from: Some(midnight(-9)),
                to: Some(midnight(-2)),
                query: "Meinung Portal",
            },
            Case {
                q: "was hat von Aspen am Montag geklungen wie ein Plan",
                speaker: Some(7),
                // Monday the 9th, two days back from Wednesday.
                from: Some(midnight(-2)),
                to: Some(midnight(-1)),
                query: "geklungen Plan",
            },
            Case {
                q: "hat Kira heute etwas über die Welt gesagt",
                speaker: Some(9),
                from: Some(midnight(0)),
                to: Some(midnight(1)),
                query: "Welt",
            },
            Case {
                q: "was hat Aspen Wren vorgestern erzählt",
                speaker: Some(11),
                from: Some(midnight(-2)),
                to: Some(midnight(-1)),
                query: "",
            },
            Case {
                q: "was wurde auf Deutsch über den Shader gesagt",
                speaker: None,
                from: None,
                to: None,
                query: "Shader",
            },
            // --- English ------------------------------------------------
            Case {
                q: "what did Aspen say about the shader yesterday?",
                speaker: Some(7),
                from: Some(midnight(-1)),
                to: Some(midnight(0)),
                query: "shader",
            },
            Case {
                q: "Aspen's take on the portal last week",
                speaker: Some(7),
                from: Some(midnight(-9)),
                to: Some(midnight(-2)),
                query: "take portal",
            },
            Case {
                q: "did Kira mention the fountain on Monday",
                speaker: Some(9),
                from: Some(midnight(-2)),
                to: Some(midnight(-1)),
                query: "fountain",
            },
            Case {
                q: "what did anyone say about avatars this week",
                speaker: None,
                from: Some(midnight(-2)),
                to: Some(midnight(5)),
                query: "anyone avatars",
            },
            Case {
                q: "shader compile error",
                speaker: None,
                from: None,
                to: None,
                query: "shader compile error",
            },
        ];

        for c in cases {
            let got = ask(c.q);
            assert_eq!(got.speaker_id, c.speaker, "speaker of {:?}", c.q);
            assert_eq!(got.from_ns, c.from, "from of {:?}", c.q);
            assert_eq!(got.to_ns, c.to, "to of {:?}", c.q);
            assert_eq!(got.query, c.query, "query of {:?}", c.q);
        }
    }

    #[test]
    fn the_speaker_comes_back_spelled_as_the_voicebank_spells_it() {
        // Which is the point of showing an interpretation at all: the user
        // typed a possessive and a lower case, and gets told who was matched.
        let i = ask("aspens shader");
        assert_eq!(i.speaker_label.as_deref(), Some("Aspen"));
        assert_eq!(i.query, "shader");
    }

    #[test]
    fn a_one_letter_slip_in_a_long_name_still_finds_the_voice() {
        assert_eq!(ask("what did Aspun say").speaker_id, Some(7));
        // But a short name is not guessed at — four letters is one edit away
        // from too much of the language.
        assert_eq!(ask("what did Kiro say").speaker_id, None);
    }

    #[test]
    fn an_exact_name_beats_a_fuzzy_one() {
        let i = ask("Kira and Aspun");
        assert_eq!(i.speaker_id, Some(9), "the exact match wins");
    }

    #[test]
    fn the_longest_name_wins_when_both_are_in_the_book() {
        assert_eq!(ask("what did Aspen Wren say").speaker_id, Some(11));
        assert_eq!(ask("what did Aspen say").speaker_id, Some(7));
    }

    #[test]
    fn a_night_is_not_a_calendar_day() {
        let i = ask("was hat Aspen gestern Abend gesagt");
        assert_eq!(i.from_ns, Some(midnight(-1) + 18 * 3600 * SEC));
        assert_eq!(i.to_ns, Some(midnight(0) + 6 * 3600 * SEC));
        // And the longer phrase won: "gestern" alone would have been a day.
        assert_eq!(i.query, "");
    }

    #[test]
    fn a_rolling_window_ends_now_rather_than_at_midnight() {
        let i = ask("shader in the last hour");
        assert_eq!(i.to_ns, Some(now()));
        assert_eq!(i.from_ns, Some(now() - 3600 * SEC));
    }

    #[test]
    fn a_month_is_a_calendar_month() {
        let i = ask("portal last month");
        // February 2026: the 1st to March 1st.
        let feb1 = days_from_civil(2026, 2, 1);
        let mar1 = days_from_civil(2026, 3, 1);
        let offset = local_offset_s(now());
        assert_eq!(i.from_ns, Some((feb1 * DAY_S - offset) * SEC));
        assert_eq!(i.to_ns, Some((mar1 * DAY_S - offset) * SEC));
    }

    #[test]
    fn a_weekday_asked_on_that_weekday_is_today() {
        // `timeref` reads a bare weekday forwards, a week away. A question is
        // the other direction and this is the case that proves it.
        let i = ask("what did Kira say on Wednesday");
        assert_eq!(i.from_ns, Some(midnight(0)));
        assert_eq!(i.to_ns, Some(midnight(1)));
    }

    #[test]
    fn a_question_that_is_all_facets_leaves_no_query() {
        let i = ask("was hat Aspen gestern gesagt?");
        assert_eq!(i.speaker_id, Some(7));
        assert!(i.query.is_empty());
        assert!(fts_expression(&i.query).is_empty());
    }

    #[test]
    fn the_fts_expression_quotes_every_word_and_drops_the_tiny_ones() {
        assert_eq!(fts_expression("shader portal"), "\"shader\" \"portal\"");
        // An apostrophe is a character in a word, never syntax.
        assert_eq!(fts_expression("don't"), "\"don't\"");
        // Every residual word, and no others: the interpretation a client is
        // shown must be the query that actually ran.
        assert_eq!(fts_expression("ok shader"), "\"ok\" \"shader\"");
    }

    #[test]
    fn nothing_is_a_facet_when_nothing_says_so() {
        let i = ask("");
        assert_eq!(i, Interpretation::default());
    }

    #[test]
    fn an_unnamed_voice_is_never_matched() {
        // Auto-labels are not offered to the parser, so a question that
        // happens to contain one is words, not a facet.
        let i = parse("what did Speaker_07 say", now(), &[]);
        assert_eq!(i.speaker_id, None);
        assert_eq!(i.query, "Speaker_07");
    }
}
