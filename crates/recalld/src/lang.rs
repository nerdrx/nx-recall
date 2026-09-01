//! Which language a transcript reads as — from the *text*, not the model.
//!
//! The multilingual ASR export returns words and no language tag, and on short
//! fragments it does not merely fail to say: it *commits to the wrong one*.
//! Measured on FLEURS German cut to lobby-sized windows (`spike/lang_flip.py`):
//! 12% of 1 s fragments and 5% of 2 s fragments come back reading as English,
//! against a median real turn of 2.4 s. That is the motivation for storing a
//! language at all and for the per-speaker correction built on top of it.
//!
//! The classifier is the one from the spike, ported verbatim in behaviour:
//! an umlaut/ß check, then de/en stopword voting. Tiny, deterministic, no model
//! and no allocation beyond the word split — it runs on every transcript the
//! daemon writes, so it has to cost nothing. It knows exactly two languages,
//! which is why `speakers.set_languages` refuses anything else: a tag the
//! classifier cannot check is a tag that can never be acted on.

/// What a transcript reads as. Deliberately four answers, not two: "I cannot
/// tell" and "there are no words" are different facts, and neither is a
/// language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    De,
    En,
    /// Words, but no majority either way — a name, a number, "ok yeah".
    Unclear,
    /// No word characters at all.
    Empty,
}

impl Lang {
    /// The BCP-47 tag to store, or `None` when this is not a language.
    pub fn tag(self) -> Option<&'static str> {
        match self {
            Lang::De => Some("de"),
            Lang::En => Some("en"),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Lang::De => "de",
            Lang::En => "en",
            Lang::Unclear => "unclear",
            Lang::Empty => "empty",
        }
    }
}

/// Every language tag this daemon understands, in the order clients show them.
pub const KNOWN: &[&str] = &["de", "en"];

const DE: &[&str] = &[
    "der", "die", "das", "und", "ist", "nicht", "ich", "du", "wir", "ihr", "sie", "es", "ein",
    "eine", "einen", "dem", "den", "mit", "von", "für", "auf", "als", "auch", "aber", "wenn",
    "dann", "noch", "schon", "nur", "mal", "was", "wie", "wo", "ja", "nein", "doch", "beim", "vom",
    "zur", "zum", "über", "unter", "zwischen", "gegen", "ohne", "durch",
];

const EN: &[&str] = &[
    "the", "a", "an", "and", "is", "are", "was", "were", "not", "i", "you", "we", "they", "it",
    "this", "that", "of", "to", "in", "for", "on", "with", "as", "at", "by", "from", "but", "if",
    "then", "just", "only", "what", "how", "where", "yes", "no", "about", "into", "over", "under",
    "between", "against", "without", "through",
];

/// A word is a run of letters and apostrophes, lowercased. Digits are not
/// words: "2019" votes for nothing.
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphabetic() || ch == '\'' || ch == '\u{2019}' {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Classify one transcript. Pure, and the only place the rule lives.
///
/// An umlaut or ß settles it outright: the English side of this pair has none,
/// and a multilingual transducer that emitted one was decoding German. Failing
/// that it is a straight stopword vote, and a tie is `Unclear` rather than a
/// coin flip — a wrong answer here would re-decode the right audio.
pub fn classify(text: &str) -> Lang {
    let ws = words(text);
    if ws.is_empty() {
        return Lang::Empty;
    }
    if text
        .chars()
        .flat_map(char::to_lowercase)
        .any(|c| matches!(c, 'ä' | 'ö' | 'ü' | 'ß'))
    {
        return Lang::De;
    }
    let de = ws.iter().filter(|w| DE.contains(&w.as_str())).count();
    let en = ws.iter().filter(|w| EN.contains(&w.as_str())).count();
    match de.cmp(&en) {
        std::cmp::Ordering::Greater => Lang::De,
        std::cmp::Ordering::Less => Lang::En,
        std::cmp::Ordering::Equal => Lang::Unclear,
    }
}

/// How many words a transcript has, for the mint bar (DESIGN §5 / 0.6.1): a
/// grunt is not a voice, and "two words" is the cheapest honest test of that.
pub fn word_count(text: &str) -> usize {
    crate::asr::normalise_words(text).len()
}

/// Parse a stored `speakers.languages` value: a JSON array of tags.
///
/// `None` — the column is NULL — means *any language*, which is the default and
/// the only state a speaker has until somebody says otherwise. An empty array
/// means the same thing and is normalised to `None` on the way in, so "any"
/// has exactly one representation in the database.
pub fn parse_languages(raw: Option<&str>) -> Option<Vec<String>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let parsed: Vec<String> = serde_json::from_str(raw).ok()?;
    if parsed.is_empty() {
        None
    } else {
        Some(parsed)
    }
}

/// Normalise and validate what a client sent. `Ok(None)` is "any".
///
/// Only tags the classifier knows are accepted: storing `fr` would promise a
/// correction this daemon cannot make (DESIGN §4 — there is one non-English
/// decoder in the catalogue and it is English-only).
pub fn normalise_languages(codes: &[String]) -> Result<Option<Vec<String>>, String> {
    let mut out: Vec<String> = Vec::new();
    for code in codes {
        let code = code.trim().to_ascii_lowercase();
        if code.is_empty() || code == "any" {
            continue;
        }
        if !KNOWN.contains(&code.as_str()) {
            return Err(format!(
                "unknown language {code:?}; this daemon classifies {} only",
                KNOWN.join(" and ")
            ));
        }
        if !out.contains(&code) {
            out.push(code);
        }
    }
    out.sort();
    Ok(if out.is_empty() { None } else { Some(out) })
}

/// The single language a speaker is pinned to, if they are pinned to exactly
/// one. Two languages (or none) means no correction is possible: a bilingual
/// speaker's German turn is not a mistake.
pub fn sole_language(languages: Option<&Vec<String>>) -> Option<&str> {
    match languages {
        Some(v) if v.len() == 1 => Some(v[0].as_str()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_umlaut_settles_it_outright() {
        assert_eq!(classify("das wär schön"), Lang::De);
        assert_eq!(classify("Größe"), Lang::De);
        // Even when the English stopwords outnumber it: no English sentence
        // contains an ß, so this is a German decode with English filler words.
        assert_eq!(classify("the a an of über"), Lang::De);
    }

    #[test]
    fn stopwords_vote_when_there_is_no_umlaut() {
        assert_eq!(classify("ich weiss nicht was das ist"), Lang::De);
        assert_eq!(classify("i do not know what that is"), Lang::En);
    }

    #[test]
    fn a_tie_is_unclear_rather_than_a_coin_flip() {
        // One stopword each way.
        assert_eq!(classify("das is"), Lang::Unclear);
        // Content words only: nothing votes.
        assert_eq!(classify("Marseille Rotterdam"), Lang::Unclear);
        assert_eq!(classify("okay"), Lang::Unclear);
    }

    #[test]
    fn no_words_is_empty_and_is_not_a_language() {
        assert_eq!(classify(""), Lang::Empty);
        assert_eq!(classify("   "), Lang::Empty);
        assert_eq!(classify("... -- ,"), Lang::Empty);
        assert_eq!(classify("2019 42"), Lang::Empty);
        assert_eq!(Lang::Empty.tag(), None);
        assert_eq!(Lang::Unclear.tag(), None);
    }

    #[test]
    fn the_flip_case_from_the_spike_is_what_this_catches() {
        // A German turn the multilingual export decoded as English — the 12%
        // case at 1 s that this whole feature exists for.
        assert_eq!(classify("I think that is the only way to do it"), Lang::En);
        // …and the same speaker's German, correctly decoded.
        assert_eq!(classify("ich glaube das ist der einzige weg"), Lang::De);
    }

    #[test]
    fn languages_round_trip_through_the_column() {
        assert_eq!(parse_languages(None), None);
        assert_eq!(parse_languages(Some("")), None);
        assert_eq!(parse_languages(Some("[]")), None);
        assert_eq!(
            parse_languages(Some(r#"["de","en"]"#)),
            Some(vec!["de".to_string(), "en".to_string()])
        );
        // Junk in the column reads as "any" rather than crashing a list query.
        assert_eq!(parse_languages(Some("not json")), None);
    }

    #[test]
    fn only_the_two_languages_the_classifier_knows_are_accepted() {
        assert_eq!(normalise_languages(&[]).unwrap(), None);
        assert_eq!(
            normalise_languages(&["any".into()]).unwrap(),
            None,
            "\"any\" is how a client spells NULL"
        );
        assert_eq!(
            normalise_languages(&["EN".into(), "en".into()]).unwrap(),
            Some(vec!["en".to_string()]),
            "case-folded and de-duplicated"
        );
        assert_eq!(
            normalise_languages(&["en".into(), "de".into()]).unwrap(),
            Some(vec!["de".to_string(), "en".to_string()]),
            "sorted, so one setting has one representation"
        );
        assert!(normalise_languages(&["fr".into()]).is_err());
    }

    #[test]
    fn a_bilingual_speaker_has_no_sole_language_to_correct_against() {
        let both = vec!["de".to_string(), "en".to_string()];
        let one = vec!["en".to_string()];
        assert_eq!(sole_language(Some(&both)), None);
        assert_eq!(sole_language(Some(&one)), Some("en"));
        assert_eq!(sole_language(None), None);
    }

    #[test]
    fn word_count_is_the_mint_bars_measure() {
        assert_eq!(word_count(""), 0);
        assert_eq!(word_count("uh"), 1);
        assert_eq!(word_count("uh huh"), 2);
        assert_eq!(word_count("...!"), 0);
    }
}
