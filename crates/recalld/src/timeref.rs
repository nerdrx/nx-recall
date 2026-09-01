//! Time references in speech, resolved against the moment they were said
//! (GRAPH.md Tier 2).
//!
//! > "Time references: de+en rule parser ("Freitag", "morgen Abend", "next
//! > week") → `time_refs(segment_id, resolved_utc, raw)`. Resolution is
//! > relative to the segment's capture time — that is the whole trick."
//!
//! That sentence is the whole module. "Friday" is not a date; it is a date
//! *plus the day it was spoken on*, and the daemon is the only thing that has
//! both. A client cannot re-derive this later — by the time anybody reads the
//! transcript, "Friday" has moved.
//!
//! Tier 2 rules: deterministic, no models, wrong sometimes, cheap always, and
//! marked as a guess wherever it surfaces. Every row carries the extractor and
//! its version so a later parser's output is distinguishable from this one's.
//!
//! ### Local time, consulted and not stored
//!
//! A person who says "Freitag" means Friday where they are sitting, so the
//! resolution has to know the machine's UTC offset — and then writes the answer
//! back as UTC nanoseconds like every other instant in the schema. The offset
//! is read for the instant the words were captured
//! ([`crate::clock::local_offset_s`]), so a reference spoken in August and one
//! spoken in December each land on the right hour. A reference that *crosses* a
//! DST boundary is resolved with the speaking side's offset and is therefore an
//! hour out; a candidate due date is a suggestion and an hour is not worth a
//! timezone database.
//!
//! ### What it gets wrong, on purpose
//!
//! - **A bare weekday is always in the future.** "Freitag" said on a Friday
//!   resolves to the Friday a week away, not to today. There is no way to tell
//!   "come on Friday" from "as I said on Friday" from the words alone, and the
//!   whole feature is about what is still owed.
//! - **A day reference has no time of day.** It resolves to local midnight and
//!   is marked [`kind::DAY`] (or [`kind::WEEKDAY`]), so a surface renders a date
//!   rather than inventing an hour. A clock time that follows one in the same
//!   sentence anchors to it — "Freitag um 18 Uhr" is one date, said twice.
//! - **A bare clock reading is 12-hour when it could be.** "at 6" resolves to
//!   whichever of 06:00 and 18:00 comes first, because that is what a person
//!   saying "at 6" means and the alternative is picking one at random.

use std::sync::OnceLock;

use regex::Regex;

use crate::clock::{civil_from_days, local_offset_s};

/// Written on every row, so a later parser's output is never mistaken for this
/// one's — GRAPH.md's Tier 2 provenance rule (`extractor`, `version`).
pub const EXTRACTOR: &str = "timeref";
pub const VERSION: u32 = 1;

/// What sort of reference this was, which is really *how precise it is*. A
/// surface renders a `DAY` as a date and a `CLOCK` as a date and a time; the
/// difference is the honest one between "Friday" and "Friday at six".
pub mod kind {
    /// A named weekday: "Freitag", "on Friday".
    pub const WEEKDAY: &str = "weekday";
    /// A named day relative to today: "morgen", "tonight", "übermorgen".
    pub const DAY: &str = "day";
    /// "nächste Woche", "next week" — resolved to that week's Monday.
    pub const WEEK: &str = "week";
    /// "am Wochenende", "this weekend" — resolved to the coming Saturday.
    pub const WEEKEND: &str = "weekend";
    /// A clock reading: "18:00", "um 6 Uhr", "at 6", "9pm".
    pub const CLOCK: &str = "clock";
    /// A duration from the moment of speaking: "in 10 Minuten", "in 2 hours".
    pub const IN: &str = "in";
}

/// One reference, as it was said and as it resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeRef {
    /// The phrase as it appears in the transcript, in its original casing. It
    /// is what a surface shows next to the resolved date, because a resolved
    /// date nobody can trace back to a word is not evidence of anything.
    pub raw: String,
    pub resolved_utc_ns: i64,
    pub kind: &'static str,
}

const SEC: i64 = 1_000_000_000;
const DAY_S: i64 = 86_400;

/// Every reference in `text`, in the order it was spoken, resolved against
/// `at_utc_ns` — the capture time of the turn the words came from.
pub fn extract(text: &str, at_utc_ns: i64) -> Vec<TimeRef> {
    let folded = fold(text);
    let cal = Local::new(at_utc_ns);
    let (today, now_tod) = cal.split(at_utc_ns);

    let mut hits = collect(&folded);
    // Longest match at the earliest position wins, and overlaps are dropped:
    // "heute abend" is one reference, never "heute" plus a stray word.
    hits.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut chosen: Vec<Hit> = Vec::new();
    for hit in hits {
        if chosen.last().is_some_and(|prev| hit.start < prev.end) {
            continue;
        }
        chosen.push(hit);
    }

    // A clock reading with a day reference before it in the same breath belongs
    // to that day. Nothing else in this module looks backwards.
    let mut anchor: Option<i64> = None;
    let mut out = Vec::new();
    for hit in chosen {
        let (resolved, day) = match hit.what {
            What::Weekday(target) => {
                let d = today + days_ahead(weekday_of(today), target);
                (cal.at(d, 0), Some(d))
            }
            What::Day { days, tod } => {
                let d = today + days;
                (cal.at(d, tod), Some(d))
            }
            What::NextWeek => {
                // Monday, which is where a week starts for everyone who says
                // "next week" and means a working one.
                let d = today + days_ahead(weekday_of(today), MONDAY);
                (cal.at(d, 0), Some(d))
            }
            What::Weekend => {
                let d = today + days_ahead(weekday_of(today), SATURDAY);
                (cal.at(d, 0), Some(d))
            }
            What::Clock { tod, ambiguous } => {
                let day = match anchor {
                    // Anchored: the day was already named, so the reading is
                    // simply that day's clock, past or future.
                    Some(d) => d,
                    // Unanchored: the next time the clock reads this.
                    None if tod > now_tod => today,
                    None if ambiguous && tod + 12 * 3600 > now_tod => today,
                    None => today + 1,
                };
                let tod = if anchor.is_none() && ambiguous && tod <= now_tod && day == today {
                    tod + 12 * 3600
                } else {
                    tod
                };
                (cal.at(day, tod), None)
            }
            What::In(seconds) => (at_utc_ns + seconds * SEC, None),
        };
        if let Some(d) = day {
            anchor = Some(d);
        }
        out.push(TimeRef {
            raw: text[hit.start..hit.end].trim().to_string(),
            resolved_utc_ns: resolved,
            kind: hit.kind,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// the local calendar
// ---------------------------------------------------------------------------

/// Days-since-epoch arithmetic in the machine's own timezone. The offset is
/// captured once, for the instant the words were spoken; see the module note.
struct Local {
    offset_s: i64,
}

impl Local {
    fn new(at_utc_ns: i64) -> Self {
        Self {
            offset_s: local_offset_s(at_utc_ns),
        }
    }

    /// `(local day since the epoch, seconds into that day)`.
    fn split(&self, utc_ns: i64) -> (i64, i64) {
        let s = utc_ns.div_euclid(SEC) + self.offset_s;
        (s.div_euclid(DAY_S), s.rem_euclid(DAY_S))
    }

    /// UTC nanoseconds for a local day at a local time of day.
    fn at(&self, day: i64, tod: i64) -> i64 {
        (day * DAY_S + tod - self.offset_s) * SEC
    }
}

const SUNDAY: i64 = 0;
const MONDAY: i64 = 1;
const SATURDAY: i64 = 6;

/// 0 = Sunday. 1970-01-01 was a Thursday, hence the +4.
fn weekday_of(day: i64) -> i64 {
    (day + 4).rem_euclid(7)
}

/// Days from `from` to the next `target`, always **1..=7**: a named day is in
/// the future, and a Friday named on a Friday is the one a week away.
fn days_ahead(from: i64, target: i64) -> i64 {
    match (target - from).rem_euclid(7) {
        0 => 7,
        n => n,
    }
}

/// Calendar date of a resolved reference, in local time — for tests and for
/// anything that wants to print one without a date crate.
pub fn local_date(resolved_utc_ns: i64) -> (i64, u32, u32) {
    let (day, _) = Local::new(resolved_utc_ns).split(resolved_utc_ns);
    civil_from_days(day)
}

/// Local time of day of a resolved reference, as `(hour, minute)`.
pub fn local_time(resolved_utc_ns: i64) -> (i64, i64) {
    let (_, tod) = Local::new(resolved_utc_ns).split(resolved_utc_ns);
    (tod / 3600, (tod / 60) % 60)
}

// ---------------------------------------------------------------------------
// matching
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum What {
    Weekday(i64),
    /// Days from today, and a time of day in seconds.
    Day {
        days: i64,
        tod: i64,
    },
    NextWeek,
    Weekend,
    Clock {
        tod: i64,
        /// A 12-hour reading with no am/pm: 06:00 and 18:00 are both candidates.
        ambiguous: bool,
    },
    In(i64),
}

struct Hit {
    start: usize,
    end: usize,
    kind: &'static str,
    what: What,
}

/// Lower-case ASCII plus the three German umlauts, **byte-length preserving**,
/// so a match's offsets index the original string and `raw` keeps its casing.
/// (`A-Z` are one byte either way; `Ä/Ö/Ü` and `ä/ö/ü` are two.)
fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        out.push(match ch {
            'A'..='Z' => ch.to_ascii_lowercase(),
            'Ä' => 'ä',
            'Ö' => 'ö',
            'Ü' => 'ü',
            other => other,
        });
    }
    out
}

fn re(slot: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    slot.get_or_init(|| Regex::new(pattern).expect("a compile-time pattern"))
}

fn collect(folded: &str) -> Vec<Hit> {
    let mut out = Vec::new();
    weekdays(folded, &mut out);
    days(folded, &mut out);
    weeks(folded, &mut out);
    weekends(folded, &mut out);
    durations(folded, &mut out);
    clocks(folded, &mut out);
    out
}

fn weekdays(s: &str, out: &mut Vec<Hit>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    // The qualifier is swallowed so `raw` reads as it was said ("am Freitag"),
    // not as a bare word the user has to reconstruct.
    let rx = re(
        &RE,
        r"(?:\b(?:am|an einem|nächsten|nächste|kommenden|kommende|diesen|next|on|this|come)\s+)?\b(montag|dienstag|mittwoch|donnerstag|freitag|samstag|sonnabend|sonntag|monday|tuesday|wednesday|thursday|friday|saturday|sunday)\b",
    );
    for m in rx.captures_iter(s) {
        let whole = m.get(0).expect("group 0");
        let target = match &m[1] {
            "sonntag" | "sunday" => SUNDAY,
            "montag" | "monday" => 1,
            "dienstag" | "tuesday" => 2,
            "mittwoch" | "wednesday" => 3,
            "donnerstag" | "thursday" => 4,
            "freitag" | "friday" => 5,
            _ => SATURDAY,
        };
        out.push(Hit {
            start: whole.start(),
            end: whole.end(),
            kind: kind::WEEKDAY,
            what: What::Weekday(target),
        });
    }
}

fn days(s: &str, out: &mut Vec<Hit>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let rx = re(
        &RE,
        r"\b(übermorgen|morgen früh|morgen abend|morgen|heute abend|heute nacht|heute|tomorrow morning|tomorrow evening|tomorrow night|tomorrow|tonight|today|later today|this evening)\b",
    );
    for m in rx.captures_iter(s) {
        let whole = m.get(0).expect("group 0");
        // "guten Morgen" is a greeting, not a date. The only cheap guard worth
        // having: folding threw away the capital that would have told us.
        if preceded_by(s, whole.start(), &["guten", "good"]) {
            continue;
        }
        let (days, tod) = match &m[1] {
            "übermorgen" => (2, 0),
            "morgen früh" | "tomorrow morning" => (1, 8 * 3600),
            "morgen abend" | "tomorrow evening" | "tomorrow night" => (1, 20 * 3600),
            "morgen" | "tomorrow" => (1, 0),
            "heute abend" | "heute nacht" | "tonight" | "this evening" => (0, 20 * 3600),
            _ => (0, 0),
        };
        out.push(Hit {
            start: whole.start(),
            end: whole.end(),
            kind: kind::DAY,
            what: What::Day { days, tod },
        });
    }
}

fn weeks(s: &str, out: &mut Vec<Hit>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let rx = re(
        &RE,
        r"\b(?:(?:in\s+der\s+)?nächste[nr]?\s+woche|nächste\s+woche|next\s+week)\b",
    );
    for m in rx.find_iter(s) {
        out.push(Hit {
            start: m.start(),
            end: m.end(),
            kind: kind::WEEK,
            what: What::NextWeek,
        });
    }
}

fn weekends(s: &str, out: &mut Vec<Hit>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let rx = re(
        &RE,
        r"(?:\b(?:am|dieses|diesem|nächstes|on the|this|the)\s+)?\bwochenende\b|(?:\b(?:on the|this|the|next)\s+)?\bweekend\b",
    );
    for m in rx.find_iter(s) {
        out.push(Hit {
            start: m.start(),
            end: m.end(),
            kind: kind::WEEKEND,
            what: What::Weekend,
        });
    }
}

fn durations(s: &str, out: &mut Vec<Hit>) {
    static RE: OnceLock<Regex> = OnceLock::new();
    let rx = re(
        &RE,
        r"\bin\s+(\d{1,3})\s*(minuten|minute|mins|min|minutes|stunden|stunde|std|hours|hour|hrs|hr|tagen|tage|tag|days|day|wochen|woche|weeks|week)\b",
    );
    for m in rx.captures_iter(s) {
        let whole = m.get(0).expect("group 0");
        let Ok(n) = m[1].parse::<i64>() else { continue };
        let unit = match &m[2] {
            "minuten" | "minute" | "mins" | "min" | "minutes" => 60,
            "stunden" | "stunde" | "std" | "hours" | "hour" | "hrs" | "hr" => 3600,
            "tagen" | "tage" | "tag" | "days" | "day" => DAY_S,
            _ => 7 * DAY_S,
        };
        out.push(Hit {
            start: whole.start(),
            end: whole.end(),
            kind: kind::IN,
            what: What::In(n * unit),
        });
    }
}

fn clocks(s: &str, out: &mut Vec<Hit>) {
    static COLON: OnceLock<Regex> = OnceLock::new();
    static UHR: OnceLock<Regex> = OnceLock::new();
    static MERIDIEM: OnceLock<Regex> = OnceLock::new();
    static AT: OnceLock<Regex> = OnceLock::new();

    // 18:00, 9:30 pm, 21.15 Uhr
    let colon = re(
        &COLON,
        r"\b(?:um\s+|at\s+|gegen\s+)?(\d{1,2})[:.](\d{2})\s*(uhr|am|pm|a\.m\.|p\.m\.)?",
    );
    for m in colon.captures_iter(s) {
        push_clock(
            out,
            m.get(0).expect("group 0").start(),
            m.get(0).expect("group 0").end(),
            m[1].parse().ok(),
            m[2].parse().ok(),
            m.get(3).map(|g| g.as_str()),
        );
    }

    // "um 18 Uhr", "18 Uhr 30", "6 uhr"
    let uhr = re(
        &UHR,
        r"\b(?:um\s+|gegen\s+)?(\d{1,2})\s*uhr(?:\s*(\d{2}))?\b",
    );
    for m in uhr.captures_iter(s) {
        push_clock(
            out,
            m.get(0).expect("group 0").start(),
            m.get(0).expect("group 0").end(),
            m[1].parse().ok(),
            m.get(2).and_then(|g| g.as_str().parse().ok()).or(Some(0)),
            Some("uhr"),
        );
    }

    // "6pm", "10 am"
    let meridiem = re(&MERIDIEM, r"\b(\d{1,2})\s*(am|pm|a\.m\.|p\.m\.)\b");
    for m in meridiem.captures_iter(s) {
        push_clock(
            out,
            m.get(0).expect("group 0").start(),
            m.get(0).expect("group 0").end(),
            m[1].parse().ok(),
            Some(0),
            Some(&m[2]),
        );
    }

    // "at 6" — the bare English form. Only after "at": a loose number is a
    // count of people far more often than it is a time. "at 6:30" also matches
    // here, and loses: the colon form starts at the same offset and is longer,
    // which is exactly what the overlap rule in `extract` is for.
    let at = re(&AT, r"\bat\s+(\d{1,2})\b");
    for m in at.captures_iter(s) {
        push_clock(
            out,
            m.get(0).expect("group 0").start(),
            m.get(0).expect("group 0").end(),
            m[1].parse().ok(),
            Some(0),
            None,
        );
    }
}

fn push_clock(
    out: &mut Vec<Hit>,
    start: usize,
    end: usize,
    hour: Option<i64>,
    minute: Option<i64>,
    suffix: Option<&str>,
) {
    let (Some(mut hour), Some(minute)) = (hour, minute) else {
        return;
    };
    if minute > 59 || hour > 23 {
        return;
    }
    let pm = matches!(suffix, Some("pm" | "p.m."));
    let am = matches!(suffix, Some("am" | "a.m."));
    if pm && hour < 12 {
        hour += 12;
    }
    if am && hour == 12 {
        hour = 0;
    }
    out.push(Hit {
        start,
        end,
        kind: kind::CLOCK,
        what: What::Clock {
            tod: hour * 3600 + minute * 60,
            // Only a bare 1..=11 could equally mean the afternoon.
            ambiguous: !pm && !am && (1..=11).contains(&hour),
        },
    });
}

/// Is the word immediately before `at` one of `words`?
fn preceded_by(s: &str, at: usize, words: &[&str]) -> bool {
    let before = s[..at].trim_end();
    words.iter().any(|w| {
        before.ends_with(w) && {
            let rest = &before[..before.len() - w.len()];
            // Start-of-string counts as a boundary, or "guten Morgen" as the
            // whole line — the commonest case — would slip the guard.
            rest.is_empty() || !rest.ends_with(char::is_alphanumeric)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::days_from_civil;

    /// A Wednesday, 2026-09-02, at 19:30 local.
    fn wednesday_evening() -> i64 {
        local_instant(2026, 9, 2, 19, 30)
    }

    /// A local wall-clock instant, as UTC nanoseconds — the same arithmetic the
    /// parser does, so a test is not quietly asserting the test machine's zone.
    fn local_instant(y: i64, m: u32, d: u32, hour: i64, minute: i64) -> i64 {
        let naive = (days_from_civil(y, m, d) * DAY_S + hour * 3600 + minute * 60) * SEC;
        // The offset is a function of the instant, so it takes one pass to find
        // the offset and a second to apply it. One iteration is exact except
        // within an hour of a DST switch, which no test here sits on.
        naive - local_offset_s(naive) * SEC
    }

    fn one(text: &str, at: i64) -> TimeRef {
        let refs = extract(text, at);
        assert_eq!(refs.len(), 1, "{text:?} produced {refs:?}");
        refs.into_iter().next().expect("one")
    }

    fn ymd(r: &TimeRef) -> (i64, u32, u32) {
        local_date(r.resolved_utc_ns)
    }

    // ---- the table: German -------------------------------------------------

    #[test]
    fn a_bare_weekday_resolves_to_the_next_one() {
        // Wednesday the 2nd: Friday is the 4th.
        let r = one("ich mach die Doku bis Freitag fertig", wednesday_evening());
        assert_eq!(r.kind, kind::WEEKDAY);
        assert_eq!(ymd(&r), (2026, 9, 4));
        assert_eq!(local_time(r.resolved_utc_ns), (0, 0));
        assert_eq!(r.raw, "Freitag", "the phrase is kept as it was said");
    }

    /// The case the rule exists to make explicit: a Friday named on a Friday is
    /// next Friday, because the feature is about what is still owed.
    #[test]
    fn a_weekday_named_on_that_weekday_is_a_week_away() {
        let friday = local_instant(2026, 9, 4, 11, 0);
        let r = one("bis Freitag hab ich das", friday);
        assert_eq!(ymd(&r), (2026, 9, 11));
    }

    #[test]
    fn the_german_day_words_resolve_around_the_capture_time() {
        let at = wednesday_evening();
        assert_eq!(
            ymd(&one("ich schick dir morgen den Link", at)),
            (2026, 9, 3)
        );
        assert_eq!(ymd(&one("heute mach ich das noch", at)), (2026, 9, 2));
        assert_eq!(ymd(&one("übermorgen bin ich wieder da", at)), (2026, 9, 4));

        // "heute Abend" is one reference with an hour on it, never "heute".
        let evening = one("heute Abend zeig ich es dir", at);
        assert_eq!(evening.kind, kind::DAY);
        assert_eq!(ymd(&evening), (2026, 9, 2));
        assert_eq!(local_time(evening.resolved_utc_ns), (20, 0));
        assert_eq!(evening.raw, "heute Abend");

        let early = one("morgen früh schick ich es", at);
        assert_eq!(ymd(&early), (2026, 9, 3));
        assert_eq!(local_time(early.resolved_utc_ns), (8, 0));
    }

    #[test]
    fn next_week_is_that_weeks_monday_and_the_weekend_is_saturday() {
        let at = wednesday_evening();
        let week = one("nächste Woche mache ich das", at);
        assert_eq!(week.kind, kind::WEEK);
        assert_eq!(ymd(&week), (2026, 9, 7), "the Monday after this Wednesday");

        let weekend = one("am Wochenende bauen wir das", at);
        assert_eq!(weekend.kind, kind::WEEKEND);
        assert_eq!(ymd(&weekend), (2026, 9, 5), "the coming Saturday");
        assert_eq!(weekend.raw, "am Wochenende");
    }

    #[test]
    fn german_clock_readings_parse_in_both_spellings() {
        let at = wednesday_evening(); // 19:30
        let r = one("um 18 Uhr bin ich da", at);
        assert_eq!(r.kind, kind::CLOCK);
        assert_eq!(local_time(r.resolved_utc_ns), (18, 0));
        assert_eq!(ymd(&r), (2026, 9, 3), "18:00 has passed, so tomorrow");

        let half = one("21:15 gehts los", at);
        assert_eq!(local_time(half.resolved_utc_ns), (21, 15));
        assert_eq!(ymd(&half), (2026, 9, 2), "still ahead today");
    }

    #[test]
    fn in_x_minutes_counts_from_the_moment_it_was_said() {
        let at = wednesday_evening();
        let r = one("bin in 10 Minuten zurück", at);
        assert_eq!(r.kind, kind::IN);
        assert_eq!(r.resolved_utc_ns, at + 600 * SEC);
        assert_eq!(one("in 2 Stunden", at).resolved_utc_ns, at + 7200 * SEC);
        assert_eq!(one("in 3 Tagen", at).resolved_utc_ns, at + 3 * DAY_S * SEC);
    }

    // ---- the table: English ------------------------------------------------

    #[test]
    fn the_english_day_words_resolve_around_the_capture_time() {
        let at = wednesday_evening();
        assert_eq!(ymd(&one("I'll send it tomorrow", at)), (2026, 9, 3));
        assert_eq!(ymd(&one("today, promise", at)), (2026, 9, 2));

        let tonight = one("yeah I'll send it to you tonight", at);
        assert_eq!(tonight.kind, kind::DAY);
        assert_eq!(local_time(tonight.resolved_utc_ns), (20, 0));
    }

    #[test]
    fn english_weekdays_and_weeks_resolve_like_their_german_twins() {
        let at = wednesday_evening();
        let friday = one("I will drop them in your DMs on Friday", at);
        assert_eq!(friday.kind, kind::WEEKDAY);
        assert_eq!(ymd(&friday), (2026, 9, 4));
        assert_eq!(friday.raw, "on Friday");

        assert_eq!(ymd(&one("next week for sure", at)), (2026, 9, 7));
        assert_eq!(ymd(&one("this weekend", at)), (2026, 9, 5));
    }

    #[test]
    fn a_bare_english_hour_takes_whichever_reading_comes_first() {
        // 09:00 said at 19:30: neither 9 nor 21 has passed by much — 21:00 is
        // the next time the clock reads it, so that is what it means.
        let at = wednesday_evening();
        let evening = one("see you at 9", at);
        assert_eq!(local_time(evening.resolved_utc_ns), (21, 0));
        assert_eq!(ymd(&evening), (2026, 9, 2));

        // Explicit am/pm is never guessed at.
        let am = one("at 9am", local_instant(2026, 9, 2, 6, 0));
        assert_eq!(local_time(am.resolved_utc_ns), (9, 0));
        let pm = one("6pm works", local_instant(2026, 9, 2, 6, 0));
        assert_eq!(local_time(pm.resolved_utc_ns), (18, 0));
    }

    #[test]
    fn in_x_hours_reads_the_english_units_too() {
        let at = wednesday_evening();
        assert_eq!(one("in 20 minutes", at).resolved_utc_ns, at + 1200 * SEC);
        assert_eq!(one("in 1 hour", at).resolved_utc_ns, at + 3600 * SEC);
    }

    // ---- combination and refusal -------------------------------------------

    /// The one backwards-looking rule in the module: a clock reading after a
    /// named day belongs to that day.
    #[test]
    fn a_clock_time_anchors_to_the_day_named_before_it() {
        let refs = extract("Freitag um 18 Uhr, ja?", wednesday_evening());
        assert_eq!(refs.len(), 2, "{refs:?}");
        assert_eq!(refs[0].kind, kind::WEEKDAY);
        assert_eq!(ymd(&refs[0]), (2026, 9, 4));
        assert_eq!(refs[1].kind, kind::CLOCK);
        assert_eq!(ymd(&refs[1]), (2026, 9, 4), "the same Friday, not tomorrow");
        assert_eq!(local_time(refs[1].resolved_utc_ns), (18, 0));
    }

    #[test]
    fn an_anchored_clock_may_be_earlier_in_the_day_than_now() {
        // 09:00 on a named Friday is 09:00 on that Friday, even though 09:00
        // today is already behind us.
        let refs = extract("Friday at 9", wednesday_evening());
        assert_eq!(refs.len(), 2);
        assert_eq!(ymd(&refs[1]), (2026, 9, 4));
        assert_eq!(local_time(refs[1].resolved_utc_ns), (9, 0));
    }

    #[test]
    fn text_with_no_reference_produces_nothing() {
        for text in [
            "wait, which portal was it",
            "the stairwell one",
            "guten Morgen",
            "good morning everyone",
            "there were 20 people in there",
            "",
        ] {
            assert!(
                extract(text, wednesday_evening()).is_empty(),
                "{text:?} produced a time reference"
            );
        }
    }

    #[test]
    fn a_nonsense_clock_reading_is_refused_rather_than_wrapped() {
        assert!(extract("99:99", wednesday_evening()).is_empty());
        assert!(extract("at 47", wednesday_evening()).is_empty());
        // 25:00 is not an hour, and must not silently become 1am.
        assert!(extract("25:00", wednesday_evening()).is_empty());
    }

    #[test]
    fn overlapping_matches_keep_the_longest_one() {
        let refs = extract("heute abend", wednesday_evening());
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].raw, "heute abend");
        // "übermorgen" must not also produce a bare "morgen".
        assert_eq!(extract("übermorgen", wednesday_evening()).len(), 1);
    }

    #[test]
    fn folding_preserves_byte_offsets_so_raw_keeps_its_casing() {
        for text in ["Nächste Woche", "ÜBERMORGEN", "Freitag", "Straße", "ÄÖÜ"] {
            assert_eq!(
                fold(text).len(),
                text.len(),
                "{text:?} changed length when folded, which would corrupt every offset"
            );
        }
        let r = one("Nächste Woche mache ich das", wednesday_evening());
        assert_eq!(r.raw, "Nächste Woche");
    }

    #[test]
    fn several_references_come_back_in_the_order_they_were_spoken() {
        let refs = extract(
            "heute nicht, aber morgen, und sonst nächste Woche",
            wednesday_evening(),
        );
        let kinds: Vec<&str> = refs.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, vec![kind::DAY, kind::DAY, kind::WEEK]);
        assert!(refs[0].resolved_utc_ns < refs[1].resolved_utc_ns);
        assert!(refs[1].resolved_utc_ns < refs[2].resolved_utc_ns);
    }

    #[test]
    fn the_provenance_constants_are_what_rows_will_carry() {
        assert_eq!(EXTRACTOR, "timeref");
        assert_eq!(VERSION, 1);
    }
}
