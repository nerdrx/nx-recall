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
//!
//! ## The short line (0.11.0)
//!
//! That rule needs three function words, and it was measured on whole FLEURS
//! sentences. A lobby turn is three to eight words and mostly content:
//! "Tu arrêtes appartement." is French, has no French function word in it, and
//! sat on the user's screen untranslated. `spike/short_lang_bench.py` re-measures
//! the rule on 2-, 3-, 4- and 6-word fragments — the real distribution — and it
//! recalls 33.8% of them.
//!
//! Two stages are added below the script check and above nothing:
//! [`guess_by_diacritic`], a character only one shippable language writes, and
//! [`guess_by_trigram`], a character-trigram model in `crate::lang_ngrams`.
//! Together they take that 33.8% to 74.7% while the thing that must not happen
//! — a German or English line handed to a translator — stays at 0.00–0.12% per
//! fragment length against a gate of 0.5%. FINDINGS §21 has the per-language
//! table and says which languages were refused which stage.

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
/// Languages a voice may be TAGGED with. `de`/`en` are what the text
/// classifier can check; `ja` (0.11.0) is what the audio identifier and the
/// Japanese decoder can route — a friend tagged `ja` skips the European model
/// entirely (analysis.rs `Pre::Direct`).
pub const KNOWN: &[&str] = &["de", "en", "ja"];

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
    // 0.11.0, and the two stages are in this order for a reason. A character
    // only one language writes is *evidence*, not a model: it cannot be beaten
    // by a coincidence of frequencies, and it costs one pass over the string.
    // The trigram model is last because it is the only stage that can be
    // confidently wrong, and everything above it is cheaper and surer.
    if let Some(tag) = guess_by_diacritic(text) {
        return Some(OtherLang {
            tag,
            confident: true,
        });
    }
    if let Some(g) = guess_by_stopwords(text) {
        return Some(g);
    }
    guess_by_trigram(text).map(|tag| OtherLang {
        tag,
        confident: true,
    })
}

/// The 0.10.2 vote, unchanged — three function words and an outright win.
fn guess_by_stopwords(text: &str) -> Option<OtherLang> {
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

// ---- 0.11.0, the short line ------------------------------------------------
//
// The 0.10.2 rule above needs three function words. A VRChat turn is three to
// eight words and mostly *content* — "Tu arrêtes appartement." has none at all —
// so on the length a lobby actually speaks in, the shipped rule answers "I
// cannot tell" more often than it answers. `spike/short_lang_bench.py` cuts
// FLEURS test sentences into 2-, 3-, 4- and 6-word fragments and measures it:
// 33.8% recall over every shippable language and length. The two stages below
// take that to 74.7% with de/en false positives at 0.00–0.12% per length, under
// a gate of 95% precision at *every* length — see FINDINGS §21.

/// A character exactly one shippable language writes, when the line has one.
///
/// The whole stage is `crate::lang_ngrams::EXCLUSIVE`, and that table is
/// generated from the corpus rather than written down, because the intuition is
/// wrong about it in both directions. `ç` looks like the French stage's best
/// evidence and is Turkish's commonest accent and Portuguese's second; `ø` looks
/// Danish and is Norwegian too; `å` is three languages at once. Every one of
/// those was in the first draft, and each cost its language the gate — Danish
/// fell to 64.6% precision at two words, Swedish to 69.2%, French to 85.9%.
/// What survives is `ãõ` `ýčěřůž` `ąćęłńśż` `ğış` and `ñ`.
///
/// Two guards on top of the table: the win must be **outright** — a line
/// carrying one language's character and another's is answered "I cannot tell",
/// never split — and the line must have two words, because one word with an
/// accent in it is as likely to be a name as a sentence.
pub fn guess_by_diacritic(text: &str) -> Option<&'static str> {
    let low: String = text.chars().flat_map(char::to_lowercase).collect();
    let mut hit: Option<&'static str> = None;
    for (tag, chars) in crate::lang_ngrams::EXCLUSIVE {
        if chars.chars().any(|c| low.contains(c)) {
            if hit.is_some() {
                return None; // two languages arguing
            }
            hit = Some(tag);
        }
    }
    let tag = hit?;
    if !crate::lang_ngrams::DIACRITIC_SHIP.contains(&tag) {
        return None;
    }
    if words(text).len() < crate::lang_ngrams::MIN_WORDS {
        return None;
    }
    Some(tag)
}

/// Lowercased letters and single spaces, padded with one space each end.
///
/// The padding is what makes the model see word *edges*: ` th` and `nt ` are
/// most of what separates one language from another at this length, and without
/// the pad a two-word fragment contributes none of them.
fn ngram_text(text: &str) -> String {
    let mut inner = String::new();
    let mut prev_space = true;
    for ch in text.chars().flat_map(char::to_lowercase) {
        if ch.is_alphabetic() {
            inner.push(ch);
            prev_space = false;
        } else if !prev_space {
            inner.push(' ');
            prev_space = true;
        }
    }
    while inner.ends_with(' ') {
        inner.pop();
    }
    if inner.is_empty() {
        return String::new();
    }
    format!(" {inner} ")
}

/// A character's slot in [`crate::lang_ngrams::ALPHABET`], or 0 for one the
/// tables never saw. A linear scan over ~75 characters, on strings of at most a
/// few dozen: cheaper than the hash map that would replace it.
fn ngram_index(ch: char) -> u32 {
    crate::lang_ngrams::ALPHABET
        .chars()
        .position(|c| c == ch)
        .map_or(0, |i| i as u32 + 1)
}

/// The line's trigrams, packed the way the generated tables are keyed.
fn packed_trigrams(text: &str) -> Vec<u32> {
    let norm = ngram_text(text);
    let idx: Vec<u32> = norm.chars().map(ngram_index).collect();
    let radix = crate::lang_ngrams::RADIX;
    idx.windows(3)
        .map(|w| (w[0] * radix + w[1]) * radix + w[2])
        .collect()
}

/// Mean log-probability per trigram, per language.
fn ngram_scores(packed: &[u32]) -> Vec<(&'static str, f32)> {
    let scale = crate::lang_ngrams::LOG_SCALE;
    crate::lang_ngrams::TABLES
        .iter()
        .map(|(tag, table)| {
            let floor = crate::lang_ngrams::FLOORS
                .iter()
                .find(|(t, _)| t == tag)
                .map_or(-10.0, |(_, f)| *f as f32 / scale);
            let sum: f32 = packed
                .iter()
                .map(|k| match table.binary_search_by_key(k, |(key, _)| *key) {
                    Ok(i) => table[i].1 as f32 / scale,
                    Err(_) => floor,
                })
                .sum();
            (*tag, sum / packed.len() as f32)
        })
        .collect()
}

/// The character-trigram model (FINDINGS §21). `None` unless one language wins
/// by a margin, and the margin is what makes this safe.
///
/// The score is a *mean* over the line's trigrams, so its noise falls off as
/// `1/sqrt(n)`: a margin loose enough to name a six-word line names German
/// fragments at two words. Measured — a flat margin leaked 2.88% of the German
/// and English two-word fragments to a translator, nearly six times the gate.
/// So the margin is `A + B / sqrt(trigrams)`, which is that standard error with
/// a price on it, and it is required **twice**: over the runner-up, and again
/// over the better of German and English. The second one is not implied by the
/// first — the languages this daemon must never mistake are not usually the
/// runner-up, they are the two the reader already has.
///
/// German, English and Norwegian are in the tables and cannot win. The first
/// two are the negatives; Norwegian is the 0.10.2 blocker doing the same job it
/// does in the stopword vote — it has to be *able* to win so that its win can
/// be refused, or every Norwegian line is answered "Danish".
pub fn guess_by_trigram(text: &str) -> Option<&'static str> {
    if words(text).len() < crate::lang_ngrams::MIN_WORDS {
        return None;
    }
    let packed = packed_trigrams(text);
    if packed.is_empty() {
        return None;
    }
    let se = 1.0 / (packed.len() as f32).sqrt();
    let mut best: (&'static str, f32) = ("", f32::NEG_INFINITY);
    let mut runner = f32::NEG_INFINITY;
    let mut deen = f32::NEG_INFINITY;
    for (tag, score) in ngram_scores(&packed) {
        if tag == "de" || tag == "en" {
            deen = deen.max(score);
        }
        if score > best.1 {
            runner = best.1;
            best = (tag, score);
        } else if score > runner {
            runner = score;
        }
    }
    if best.1 - runner < crate::lang_ngrams::MARGIN_A + crate::lang_ngrams::MARGIN_B * se {
        return None;
    }
    if best.1 - deen < crate::lang_ngrams::MARGIN_DEEN_A + crate::lang_ngrams::MARGIN_DEEN_B * se {
        return None;
    }
    if !crate::lang_ngrams::TRIGRAM_SHIP.contains(&best.0) {
        // de, en, or the Norwegian blocker.
        return None;
    }
    Some(best.0)
}

// ---- end 0.11.0 ------------------------------------------------------------

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
                "unknown language {code:?}; this daemon knows {} only",
                KNOWN.join(", ")
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

    // ---- 0.11.0, the short line -------------------------------------------

    #[test]
    fn the_line_that_started_this_is_french() {
        // "Tu arrêtes appartement." — three words, not one of them a French
        // function word, and 0.10.2 answered `None`. The trigram stage names
        // it, and names it confidently enough to stamp the row.
        let g = guess_other("Tu arrêtes appartement.").expect("a language");
        assert_eq!(g.tag, "fr");
        assert!(g.confident);
    }

    #[test]
    fn the_two_languages_the_reader_has_are_still_never_guessed_at() {
        // The gate the whole feature is priced on: 0.00–0.12% of German and
        // English fragments per length. These are the two the user watched.
        for text in [
            "Ich hab das gestern gemacht",
            "I did that yesterday",
            // …and the shapes the new stages could plausibly break on: German
            // with umlauts (NOT exclusive characters), English with a Romance
            // loanword, and a two-word fragment of each.
            "das wär schön gewesen",
            "the cafe menu",
            "ich glaube",
            "not really",
        ] {
            assert_eq!(guess_other(text), None, "{text:?}");
        }
    }

    #[test]
    fn a_character_only_one_language_writes_settles_a_short_line() {
        // Two words each, and not a function word between them.
        for (text, want) in [
            ("Dziękuję bardzo", "pl"),  // ł ę ą ż ć ń ś
            ("Teşekkür ederim", "tr"),  // ş ğ ı
            ("Děkuji mnohokrát", "cs"), // ř ě ů č ž ý
            ("Não posso", "pt"),        // ã õ
            ("mañana temprano", "es"),  // ñ
        ] {
            assert_eq!(guess_by_diacritic(text), Some(want), "{text:?}");
            assert_eq!(guess_other(text).map(|g| g.tag), Some(want), "{text:?}");
        }
        // One word is a name, not a sentence.
        assert_eq!(guess_by_diacritic("Dziękuję"), None);
    }

    #[test]
    fn two_languages_characters_in_one_line_is_answered_i_cannot_tell() {
        // The conflict rule. A Polish character and a Spanish one in the same
        // line is not half of each — the stage declines and says nothing.
        assert_eq!(guess_by_diacritic("mañana słońce"), None);
        assert_eq!(guess_by_diacritic("não teşekkür"), None);
    }

    #[test]
    fn the_characters_the_eye_calls_exclusive_and_the_corpus_does_not() {
        // These four were in the hand-written first draft of the table and the
        // bench threw every one of them out. `ç` is Turkish and Portuguese as
        // much as French; `ê` is Portuguese; `ø` is Norwegian as well as
        // Danish; `å` is all three Scandinavian languages. Each cost its
        // language the 95% gate, so none of them is in the shipped table —
        // which is a fact about `lang_ngrams::EXCLUSIVE`, so assert it there.
        let table: String = crate::lang_ngrams::EXCLUSIVE
            .iter()
            .map(|(_, chars)| *chars)
            .collect();
        for ch in ['ç', 'ê', 'ø', 'å', 'ä', 'ö', 'ü', 'ß', 'é'] {
            assert!(!table.contains(ch), "{ch:?} is not exclusive to anything");
        }
        // Danish and Swedish therefore have no diacritic stage at all.
        for tag in ["da", "sv", "fr", "nl", "it", "fi"] {
            assert!(
                !crate::lang_ngrams::DIACRITIC_SHIP.contains(&tag),
                "{tag} has no exclusive character"
            );
        }
    }

    #[test]
    fn the_trigram_model_reads_a_known_sentence_in_each_language_it_ships() {
        for (text, want) in [
            ("je voudrais te montrer quelque chose", "fr"),
            ("la ciudad tiene muchos habitantes", "es"),
            ("voglio farti vedere una cosa", "it"),
            ("quero te mostrar uma coisa", "pt"),
            ("ik wil je iets laten zien", "nl"),
            ("chcę ci coś pokazać teraz", "pl"),
            ("sana bir şey göstermek istiyorum", "tr"),
            ("staden har många invånare", "sv"),
            ("haluan näyttää sinulle jotain", "fi"),
            ("mesto ma mnoho obyvatel", "cs"),
        ] {
            assert_eq!(guess_by_trigram(text), Some(want), "{text:?}");
        }
        // The two it must never answer with, and the blocker.
        for text in [
            "ich möchte dir etwas zeigen",
            "i want to show you something",
            "jeg vil vise deg noe",
        ] {
            let got = guess_by_trigram(text);
            assert!(
                !matches!(got, Some("de") | Some("en") | Some("no")),
                "{text:?} -> {got:?}"
            );
        }
    }

    #[test]
    fn the_trigram_stage_declines_when_nothing_wins_by_a_margin() {
        // No letters at all, and one word: below the two-word floor.
        assert_eq!(guess_by_trigram("2019"), None);
        assert_eq!(guess_by_trigram("bonjour"), None);
        // A name is not a sentence in the language it comes from.
        assert_eq!(guess_by_trigram("Marseille Rotterdam"), None);
        // And a real Spanish sentence that simply does not clear the margin:
        // Portuguese is 0.25 behind it and the margin at five words is 0.31.
        // Declining here is the rule working — the alternative to "I cannot
        // tell" is Portuguese, not Spanish. `spike/short_lang_bench.py` reads
        // this line the same way, which is what "the same rule" means.
        assert_eq!(
            guess_by_trigram("quiero mostrarte algo muy interesante"),
            None
        );
    }

    #[test]
    fn every_tag_the_new_stages_may_answer_with_is_one_the_daemon_ships() {
        // `GUESSABLE` is the promise; the two generated ship lists must be
        // inside it, or a stage could stamp a row with a tag 0.10.2 refused.
        for tag in crate::lang_ngrams::DIACRITIC_SHIP {
            assert!(GUESSABLE.contains(tag), "{tag} is not guessable");
        }
        for tag in crate::lang_ngrams::TRIGRAM_SHIP {
            assert!(GUESSABLE.contains(tag), "{tag} is not guessable");
        }
        // Norwegian is in the tables so that it can lose. It is in neither
        // ship list, exactly as in the stopword vote.
        assert!(
            crate::lang_ngrams::TABLES.iter().any(|(t, _)| *t == "no"),
            "the blocker must be able to win"
        );
        assert!(!crate::lang_ngrams::TRIGRAM_SHIP.contains(&"no"));
        for tag in ["de", "en"] {
            assert!(crate::lang_ngrams::TABLES.iter().any(|(t, _)| *t == tag));
            assert!(!crate::lang_ngrams::TRIGRAM_SHIP.contains(&tag));
        }
    }

    #[test]
    fn the_generated_tables_are_the_ones_the_bench_measured() {
        // The same discipline as the 0.10.2 test above. The tables themselves
        // are generated, so they cannot drift; the *constants* the decision is
        // made with are the ones that could, and every number in FINDINGS §21
        // is a number about these five.
        let bench = include_str!("../../../spike/short_lang_bench.py");
        let value = |name: &str| -> f32 {
            bench
                .lines()
                .find(|l| l.starts_with(&format!("{name} = ")))
                .and_then(|l| l.split_once(" = "))
                .and_then(|(_, v)| v.split_whitespace().next())
                .and_then(|v| v.trim_end_matches('#').trim().parse().ok())
                .unwrap_or_else(|| panic!("the bench has no {name}"))
        };
        assert_eq!(
            value("TRI_MIN_WORDS") as usize,
            crate::lang_ngrams::MIN_WORDS
        );
        assert_eq!(value("TRI_MARGIN_A"), crate::lang_ngrams::MARGIN_A);
        assert_eq!(value("TRI_MARGIN_B"), crate::lang_ngrams::MARGIN_B);
        assert_eq!(
            value("TRI_MARGIN_DEEN_A"),
            crate::lang_ngrams::MARGIN_DEEN_A
        );
        assert_eq!(
            value("TRI_MARGIN_DEEN_B"),
            crate::lang_ngrams::MARGIN_DEEN_B
        );
        // …and the packing the tables were keyed with.
        assert_eq!(
            crate::lang_ngrams::RADIX as usize,
            crate::lang_ngrams::ALPHABET.chars().count() + 2
        );
        for (_, table) in crate::lang_ngrams::TABLES {
            assert!(
                table.windows(2).all(|w| w[0].0 < w[1].0),
                "a table is not sorted, so the binary search is wrong"
            );
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
