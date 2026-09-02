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
//!
//! ## The third language (0.10.2)
//!
//! "Translate everything that is not German or English" needs one thing the
//! two-way classifier cannot give: the difference between *a language I do not
//! read* and *words I could not read at all*. Both come out `Unclear` today, so
//! a French turn and a mumbled German one are stamped identically and neither
//! can be queued without queueing the other.
//!
//! [`guess_other`] is that difference and nothing more. It never contradicts
//! [`classify`] — a row already stamped `de` or `en` is not its business — and
//! it is deliberately a *narrower* set than [`OFFERED`], the list a person may
//! pick a target from. A person may say they read Norwegian; the guesser will
//! not claim a sentence is Norwegian, because it cannot tell Norwegian from
//! Danish and a confident wrong stamp is worse than no stamp. The numbers
//! behind both halves of that sentence are in `spike/guess_other_bench.py` and
//! quoted on [`GUESSABLE`].

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

// ---- 0.10.2, the third language -------------------------------------------

/// The languages a client may offer as a target or as one you read.
///
/// A pair list rather than two, so a code and the name shown beside it cannot
/// drift apart, and in the order clients render them: the two the daemon
/// classifies natively first, then the rest by how often a VRChat lobby
/// produces them.
///
/// This is a **wider** set than [`guess_other`] can answer with — see the
/// module note on the two halves of 0.10.2. Norwegian is the example: a person
/// may declare that they read it, and a person may translate into it, but the
/// guesser refuses to *claim* a sentence is Norwegian because it cannot tell it
/// from Danish.
pub const OFFERED: &[(&str, &str)] = &[
    ("en", "English"),
    ("de", "German"),
    ("fr", "French"),
    ("es", "Spanish"),
    ("it", "Italian"),
    ("pt", "Portuguese"),
    ("nl", "Dutch"),
    ("pl", "Polish"),
    ("ru", "Russian"),
    ("uk", "Ukrainian"),
    ("ja", "Japanese"),
    ("zh", "Chinese"),
    ("ko", "Korean"),
    ("tr", "Turkish"),
    ("sv", "Swedish"),
    ("da", "Danish"),
    ("no", "Norwegian"),
    ("fi", "Finnish"),
    ("cs", "Czech"),
];

/// The English name of a language tag, or `None` for a tag nothing offers.
pub fn name_of(tag: &str) -> Option<&'static str> {
    OFFERED
        .iter()
        .find(|(code, _)| *code == tag)
        .map(|(_, name)| *name)
}

/// Is this a tag a client may set as a target or as a language you read?
pub fn offered(tag: &str) -> bool {
    name_of(tag).is_some()
}

/// What [`guess_other`] concluded: a tag, and whether it is sure enough to be
/// written onto the row.
///
/// Two fields rather than one, because the two answers are used differently.
/// *Any* guess is enough to put a turn in the translation queue — the cost of
/// being wrong there is one model call that comes back an echo and is dropped.
/// Only a **confident** guess is written into `segments.lang`, because that
/// column is read by the language prior, the arbiter and the transcript, and a
/// wrong stamp there propagates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtherLang {
    pub tag: &'static str,
    pub confident: bool,
}

/// Stopwords a guess may be won on, per language. **Verbatim from
/// `spike/guess_other_bench.py`**, which is where the precision figures come
/// from, and asserted equal to it by a test below.
const OTHER_STOPWORDS: &[(&str, &str)] = &[
    (
        "fr",
        "le la les des une est ne pas que qui pour dans sur avec aux cette il elle nous vous ils elles mais ou plus sont été être ce",
    ),
    (
        "es",
        "el los las del y en que es un una por para con su como más pero está están fue sus",
    ),
    (
        "it",
        "il lo gli le di della che non è un una per con sono come più anche dei nel alla",
    ),
    (
        "pt",
        "os as do da dos das que não um uma por para com mais mas está são se na no",
    ),
    (
        "nl",
        "het een van niet dat op te voor zijn er ook maar als aan door om worden werd deze",
    ),
    (
        "pl",
        "w nie na że to się do jest ale jak po dla od przez czy tylko już oraz który",
    ),
    (
        "tr",
        "bir bu için ile çok daha var olarak gibi ki ise ancak sonra kadar veya olan",
    ),
    (
        "sv",
        "och att är för av på inte till som han var men den det ett har med om",
    ),
    (
        "da",
        "og af ikke til er jeg han hun som men det den har med om der blev",
    ),
    (
        "no",
        "og av ikke til er jeg han hun som men det den har med om det være også",
    ),
    (
        "fi",
        "ja on ei että se ovat kuin myös mutta niin tai kun jos hän oli ollut sekä",
    ),
    (
        "cs",
        "a v na se že je to do za od pro ale jak nebo který jsou byl také jako",
    ),
];

/// The tags [`guess_other`] is allowed to answer with: every language in
/// `OTHER_STOPWORDS` that cleared the gate, plus the ones settled by script.
///
/// `spike/guess_other_bench.py`, 4200 FLEURS sentences — 200 per language, plus
/// 200 German and 200 English as negatives — precision measured over the whole
/// mixed set. The gate was **90% precision**, and every tag here cleared it:
/// ar el ja ko ru uk zh at 99–100%, fi it nl pl tr at 100%, cs da es 98.4/98.4/98.3,
/// fr pt 97.8, sv 93.1. **Not one German or English sentence in the 400
/// negatives was guessed to be anything at all.**
///
/// Norwegian is in the vote table and is not here. Its function words are
/// Danish's — 64.8% precision when it was allowed to win — so it stays as a
/// **blocker**: a Norwegian sentence still wins the vote, and winning with a
/// tag that is not shipped is answered "I cannot tell" rather than "Danish".
/// Removing it entirely would have handed those sentences to Danish and taken
/// Danish's 98.4% down with them.
pub const GUESSABLE: &[&str] = &[
    "ar", "cs", "da", "el", "es", "fi", "fr", "it", "ja", "ko", "nl", "pl", "pt", "ru", "sv", "tr",
    "uk", "zh",
];

/// Stopwords a language must have before it may win.
const MIN_VOTES: usize = 3;
/// …and before the guess is written onto the row.
const CONFIDENT_VOTES: usize = 4;

/// Which language *other than German or English* this is, if any.
///
/// [`classify`] answers a two-way question and answers `Unclear` for everything
/// else, which is why a French turn and a mumbled German one are stamped
/// identically today. This is the third-language half: script first, because a
/// stopword vote cannot be wrong about a sentence with no Latin letters in it,
/// then the same kind of vote [`classify`] uses, over twelve more tables.
///
/// Two guards, both measured:
///
/// * a language wins only with `MIN_VOTES` stopwords and **strictly more than
///   de+en together** ([`stopword_votes`]) — so a German sentence with one
///   Dutch-looking word in it is still German;
/// * the win must be outright. A tie is `None`, exactly as in [`classify`],
///   because the two languages that tie here are always the two nobody can
///   tell apart from three function words.
pub fn guess_other(text: &str) -> Option<OtherLang> {
    if let Some(tag) = guess_by_script(text) {
        return Some(OtherLang {
            tag,
            confident: true,
        });
    }
    let ws = words(text);
    if ws.is_empty() {
        return None;
    }
    let (de, en) = (
        ws.iter().filter(|w| DE.contains(&w.as_str())).count(),
        ws.iter().filter(|w| EN.contains(&w.as_str())).count(),
    );
    let mut best = 0usize;
    let mut best_tag = "";
    let mut tied = false;
    for (tag, list) in OTHER_STOPWORDS {
        let n = ws
            .iter()
            .filter(|w| list.split(' ').any(|s| s == w.as_str()))
            .count();
        if n > best {
            best = n;
            best_tag = tag;
            tied = false;
        } else if n == best && n > 0 {
            tied = true;
        }
    }
    if best < MIN_VOTES || best <= de + en || tied {
        return None;
    }
    if !GUESSABLE.contains(&best_tag) {
        // A blocker won. See `GUESSABLE`.
        return None;
    }
    Some(OtherLang {
        tag: best_tag,
        confident: best >= CONFIDENT_VOTES,
    })
}

/// The writing system, when there is one that settles the question.
fn guess_by_script(text: &str) -> Option<&'static str> {
    let mut letters = 0usize;
    let (mut kana, mut hangul, mut han, mut cyr, mut arab, mut greek) = (0, 0, 0, 0, 0, 0);
    for ch in text.chars() {
        if !ch.is_alphabetic() {
            continue;
        }
        letters += 1;
        match ch as u32 {
            0x3040..=0x30FF | 0x31F0..=0x31FF | 0xFF66..=0xFF9D => kana += 1,
            0x1100..=0x11FF | 0x3130..=0x318F | 0xAC00..=0xD7A3 => hangul += 1,
            0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF => han += 1,
            0x0400..=0x052F => cyr += 1,
            0x0600..=0x06FF
            | 0x0750..=0x077F
            | 0x08A0..=0x08FF
            | 0xFB50..=0xFDFF
            | 0xFE70..=0xFEFF => arab += 1,
            0x0370..=0x03FF | 0x1F00..=0x1FFF => greek += 1,
            _ => {}
        }
    }
    // Kana and hangul are checked by PRESENCE and the rest by dominance.
    // Japanese writes its content words in the same Han characters Chinese
    // uses and its grammar in kana, so a kanji-heavy Japanese sentence is
    // *dominated* by a script Chinese also has: the bench read 22 Japanese
    // sentences as Chinese under a dominance rule, and zh's precision was
    // 87.2%. Two characters, so one borrowed word is not a language.
    if kana >= 2 {
        return Some("ja");
    }
    if hangul >= 2 {
        return Some("ko");
    }
    if letters == 0 {
        return None;
    }
    // A third of the letters: a Russian world name inside a German sentence is
    // not a Russian sentence.
    let third = |n: usize| n * 3 >= letters;
    if third(cyr) {
        // Ukrainian has four letters Russian does not. Any of them settles it;
        // otherwise Russian, which is the commoner case by a long way.
        return Some(
            if text
                .chars()
                .any(|c| matches!(c, 'ї' | 'і' | 'є' | 'ґ' | 'Ї' | 'І' | 'Є' | 'Ґ'))
            {
                "uk"
            } else {
                "ru"
            },
        );
    }
    if third(han) {
        return Some("zh");
    }
    if third(arab) {
        return Some("ar");
    }
    if third(greek) {
        return Some("el");
    }
    None
}

// ---- end 0.10.2 ------------------------------------------------------------

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

/// How many of each language's stopwords a transcript contains, as
/// `(de, en)`.
///
/// The raw vote behind [`classify`], exposed for a caller that needs the
/// evidence rather than the verdict (0.9.0, `crate::night`). The difference
/// matters exactly once: `classify` settles on German the moment it sees an
/// umlaut or an ß, which is right for the two-language question it was built
/// for and **wrong as a filter against a third language** — "Tack för att ni
/// tittade" is Swedish with an ö in it, and FINDINGS §12 measured
/// whisper-large-v3 producing precisely that on German lobby audio. A caller
/// that is deciding whether to overwrite a transcript asks for the counts and
/// insists on real evidence.
pub fn stopword_votes(text: &str) -> (usize, usize) {
    let ws = words(text);
    (
        ws.iter().filter(|w| DE.contains(&w.as_str())).count(),
        ws.iter().filter(|w| EN.contains(&w.as_str())).count(),
    )
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

    // ---- 0.10.2, the third language ---------------------------------------

    #[test]
    fn the_shipped_tables_are_the_ones_the_precision_was_measured_with() {
        // The same discipline as `translate::TRANSLATE_GBNF`: the numbers on
        // `GUESSABLE` are only about this code if this code is the code the
        // bench ran. The bench's tables are parsed straight out of its source.
        let bench = include_str!("../../../spike/guess_other_bench.py");
        let mut seen = 0;
        for (tag, list) in OTHER_STOPWORDS {
            let needle = format!("\"{tag}\": \"");
            let line = bench
                .lines()
                .find(|l| l.trim_start().starts_with(&needle))
                .unwrap_or_else(|| panic!("the bench has no table for {tag}"));
            let words = line
                .split_once("\": \"")
                .and_then(|(_, rest)| rest.split_once("\".split()"))
                .expect("a bench table line")
                .0;
            assert_eq!(*list, words, "the {tag} table drifted from the bench");
            seen += 1;
        }
        assert_eq!(seen, OTHER_STOPWORDS.len());
        // …and the ship list, which is the gate's actual output.
        let ship = bench
            .lines()
            .find(|l| l.starts_with("SHIP = set("))
            .expect("the bench's ship set");
        let mut want: Vec<&str> = ship
            .split_once('"')
            .and_then(|(_, r)| r.split_once('"'))
            .expect("the ship list")
            .0
            .split(' ')
            .collect();
        want.sort_unstable();
        assert_eq!(want, GUESSABLE, "the shipped languages drifted");
        // The de/en tables the vote is compared against are the same ones.
        for (name, list) in [("DE", DE), ("EN", EN)] {
            let line = bench
                .lines()
                .find(|l| l.starts_with(&format!("{name} = \"")))
                .expect("the bench's de/en table");
            let words = line
                .split_once(" = \"")
                .and_then(|(_, r)| r.split_once("\".split()"))
                .expect("a table")
                .0;
            assert_eq!(
                words.split(' ').collect::<Vec<_>>(),
                list,
                "the {name} table drifted from the bench"
            );
        }
    }

    #[test]
    fn a_script_settles_it_and_a_borrowed_word_does_not() {
        for (text, want) in [
            ("это единственный способ сделать это", "ru"),
            ("це єдиний спосіб це зробити", "uk"),
            ("それが唯一の方法だと思う", "ja"),
            ("그것이 유일한 방법이라고 생각해요", "ko"),
            ("我认为这是唯一的方法", "zh"),
            ("أعتقد أن هذه هي الطريقة الوحيدة", "ar"),
            ("νομίζω ότι αυτός είναι ο μόνος τρόπος", "el"),
        ] {
            assert_eq!(guess_other(text).map(|g| g.tag), Some(want), "{text:?}");
            assert!(guess_other(text).unwrap().confident, "{text:?}");
        }
        // A world name in another script inside an English sentence is not a
        // turn in that language.
        assert_eq!(guess_other("meet me in the 東京 world tonight"), None);
        // Japanese written mostly in kanji is still Japanese, which is the
        // whole reason kana are checked by presence: under a dominance rule
        // this reads as Chinese.
        assert_eq!(
            guess_other("東京駅の近くで待ってる").map(|g| g.tag),
            Some("ja")
        );
    }

    #[test]
    fn the_languages_the_user_actually_reads_are_never_guessed_at() {
        // The one failure that matters: German or English handed to a
        // translator because a third language was read into it. Zero of the
        // 400 negatives in the bench, and these are the shapes that come
        // closest — Dutch-looking German, and English with Romance loanwords.
        for text in [
            "ich glaube das ist der einzige weg das zu machen",
            "das war doch nur ein test mit dem neuen mikrofon",
            "i think that is the only way to do it",
            "the cafe menu had a la carte options for the whole group",
        ] {
            assert_eq!(guess_other(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_latin_script_guess_needs_an_outright_win_over_de_and_en() {
        let fr = guess_other("je ne sais pas ce que c'est mais il est dans la boîte").unwrap();
        assert_eq!(fr.tag, "fr");
        assert!(fr.confident);
        assert_eq!(
            guess_other("no way").map(|g| g.tag),
            None,
            "two words vote for nothing"
        );
        // Three votes is the floor and it is not a confident stamp: enough to
        // ask the model, not enough to write into `segments.lang`.
        let g = guess_other("het is niet voor mij").unwrap();
        assert_eq!(g.tag, "nl");
        assert!(!g.confident, "three votes is a queue, not a stamp");
    }

    #[test]
    fn a_language_that_cannot_be_told_from_another_is_answered_i_cannot_tell() {
        // Norwegian Bokmål. It wins its own vote and is refused, which is what
        // keeps Danish's precision at 98.4% instead of handing it Norwegian.
        assert_eq!(
            guess_other("jeg tror ikke det er den eneste måten å gjøre det på"),
            None
        );
        assert!(!GUESSABLE.contains(&"no"), "Norwegian must not be shipped");
        assert!(offered("no"), "…but it is still a language you can pick");
    }

    #[test]
    fn the_offered_list_is_a_code_and_a_name_that_cannot_drift() {
        assert_eq!(name_of("en"), Some("English"));
        assert_eq!(name_of("uk"), Some("Ukrainian"));
        assert_eq!(name_of("xx"), None);
        assert!(offered("de") && !offered("xx"));
        assert_eq!(OFFERED[0].0, "en", "the target's default leads the list");
        // The two lists overlap and neither contains the other, which is not
        // an oversight. `no` is offered and not guessable (it cannot be told
        // from Danish); `ar` and `el` are guessable and not offered (a turn in
        // them is recognised as needing translation, but nobody asked to
        // *read* in them). Both directions are load-bearing.
        assert!(offered("no") && !GUESSABLE.contains(&"no"));
        assert!(GUESSABLE.contains(&"ar") && !offered("ar"));
        for tag in ["fr", "es", "ja", "ru"] {
            assert!(offered(tag) && GUESSABLE.contains(&tag), "{tag}");
        }
    }

    #[test]
    fn word_count_is_the_mint_bars_measure() {
        assert_eq!(word_count(""), 0);
        assert_eq!(word_count("uh"), 1);
        assert_eq!(word_count("uh huh"), 2);
        assert_eq!(word_count("...!"), 0);
    }
}
