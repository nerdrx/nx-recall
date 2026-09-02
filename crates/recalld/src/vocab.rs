//! The vocabulary: the words this lobby uses that no ASR model has heard of.
//!
//! Names, worlds, in-jokes, the games people play. `vocab.get` and `vocab.set`
//! are the contract's shape for them: a user glossary that is persisted and
//! replaced wholesale, three automatic sources the daemon can see for itself,
//! and one `effective` list which is their union, capped, most recent first.
//!
//! ## What this does NOT do, and the number that decided it
//!
//! The 0.8.0 contract says the daemon biases the transducer toward `effective`.
//! It does not, and the reason is measured rather than architectural
//! (`spike/hotwords_bench.py`, 40 LibriSpeech utterances carrying a word that
//! appears exactly once in dev-clean, plus 40 that carry none of them):
//!
//! | configuration | targeted recall | control WER |
//! |---|---:|---:|
//! | greedy (what ships) | 85.0% | 6.9% |
//! | modified_beam_search, no hotwords | 82.5% | 8.3% |
//! | + hotwords @1.5 | 90.0% | 8.2% |
//! | + hotwords @2.0 | 88.8% | 8.0% |
//! | + hotwords @3.0 | 87.5% | **29.4%** |
//!
//! Three things in that table, in order of how much they matter:
//!
//! 1. **Biasing is real but small.** +9.1% relative recall over the same
//!    decoder, against a +20% gate. It does not clear the bar.
//! 2. **It costs a decoder.** sherpa-onnx only consults a context graph in
//!    `modified_beam_search`, and switching to it costs 1.6 pp of WER before
//!    any hotword is added — so the vocabulary would have to pay that back
//!    before it earned anything.
//! 3. **The failure mode is the one the gate was written for.** At score 3.0
//!    the glossary starts injecting itself into utterances that contain none of
//!    its words: control WER 8.3% → 29.4%. A vocabulary that rewrites the turns
//!    it was not about is worse than no vocabulary.
//!
//! Two mechanical findings came out of the same run and are recorded here
//! because they are the expensive part to rediscover: `modeling_unit = "bpe"`
//! with no `bpe_vocab` **segfaults the process** (the v3 tarball ships none),
//! and a usable one can be synthesised from `tokens.txt` by pairing each piece
//! with `-id` as its score.
//!
//! So the list is assembled, stored, served and announced — a person can curate
//! it, and it is what a decoder that can use it would be handed — and nothing
//! is fed to the recognizer. When that changes it changes here, behind the same
//! gate.

use anyhow::Result;
use serde_json::{Value, json};

use crate::store::Store;

/// `settings` key holding the user's glossary, as a JSON array of strings.
pub const USER_TERMS_KEY: &str = "vocab_user_terms";
/// `settings` key holding world names seen in the VRChat log, newest first.
pub const WORLDS_KEY: &str = "vocab_worlds";
/// How many world names are remembered. A world nobody has been in for a
/// hundred worlds is not this lobby's vocabulary any more.
pub const MAX_WORLDS: usize = 50;

/// The four automatic sources, kept apart on the wire so a user can see where a
/// term came from before deciding to keep it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutoTerms {
    pub roster: Vec<String>,
    pub worlds: Vec<String>,
    pub corrections: Vec<String>,
    pub speakers: Vec<String>,
}

/// The whole answer to `vocab.get`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Vocabulary {
    pub user: Vec<String>,
    pub auto: AutoTerms,
    pub effective: Vec<String>,
}

impl Vocabulary {
    pub fn to_json(&self) -> Value {
        json!({
            "user": self.user,
            "auto": {
                "roster": self.auto.roster,
                "worlds": self.auto.worlds,
                "corrections": self.auto.corrections,
                "speakers": self.auto.speakers,
            },
            "effective": self.effective,
            // Said out loud rather than left for a user to discover: the list
            // is real, and nothing is currently biased by it (module note).
            "applied_to_decoder": false,
        })
    }
}

/// Build the effective list: the user's glossary first, then the automatic
/// sources in the order they were passed, de-duplicated case-insensitively and
/// truncated to `cap`.
///
/// **Order is the priority.** The cap has to bite somewhere, and the two rules
/// it bites by are: what a person typed outranks what the daemon noticed, and
/// within the automatic sources, recency — every caller passes its list newest
/// first. De-duplication keeps the *first* spelling seen, so a user who writes
/// "Kübra" keeps their capitalisation over the roster's.
pub fn effective(user: &[String], auto: &AutoTerms, cap: usize) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    let sources = [
        user,
        &auto.speakers[..],
        &auto.corrections[..],
        &auto.roster[..],
        &auto.worlds[..],
    ];
    for source in sources {
        for term in source {
            let term = term.trim();
            if term.is_empty() || out.len() >= cap {
                continue;
            }
            let key = term.to_lowercase();
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            out.push(term.to_string());
        }
    }
    out
}

/// Split a stored or typed term list into terms worth biasing on.
///
/// A "term" is one to four words: a hotword is a token path, and a whole
/// sentence pasted into a glossary is a path no decoder will ever walk. Empty
/// entries and anything longer are dropped rather than silently mangled.
pub fn clean_terms(terms: &[String]) -> Vec<String> {
    terms
        .iter()
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| !t.is_empty() && t.split(' ').count() <= 4 && t.chars().count() <= 64)
        .collect()
}

/// The words a correction *added*: what the user typed minus what the model
/// wrote. Those are the words the model did not know, which is the definition
/// of the thing this list is for.
///
/// Compared case-insensitively and returned in the user's spelling: "Kübra" is
/// the correction, "kubra" is not what anybody would want written down.
pub fn corrected_words(before: &str, after: &str) -> Vec<String> {
    let had: Vec<String> = before
        .split_whitespace()
        .map(|w| strip_punctuation(w).to_lowercase())
        .collect();
    let mut out = Vec::new();
    for word in after.split_whitespace() {
        let clean = strip_punctuation(word);
        if clean.chars().count() < 3 {
            continue;
        }
        let key = clean.to_lowercase();
        if had.contains(&key) || out.iter().any(|w: &String| w.to_lowercase() == key) {
            continue;
        }
        out.push(clean.to_string());
    }
    out
}

fn strip_punctuation(word: &str) -> &str {
    word.trim_matches(|c: char| !c.is_alphanumeric() && c != '\'' && c != '-')
}

/// Read the user's glossary out of the settings table.
pub fn user_terms(store: &Store) -> Result<Vec<String>> {
    let raw = store.setting(USER_TERMS_KEY)?;
    Ok(match raw {
        Some(text) => clean_terms(&serde_json::from_str::<Vec<String>>(&text).unwrap_or_default()),
        None => Vec::new(),
    })
}

/// Replace the user's glossary. Whole-list replacement is the contract, and it
/// is also the only shape that can express a deletion.
pub fn set_user_terms(store: &Store, terms: &[String], cap: usize) -> Result<Vec<String>> {
    let mut cleaned = clean_terms(terms);
    cleaned.truncate(cap);
    store.set_setting(USER_TERMS_KEY, &serde_json::to_string(&cleaned)?)?;
    Ok(cleaned)
}

/// Remember a world name seen in the VRChat log. Newest first, capped, and a
/// name that is already known is moved to the front rather than duplicated.
pub fn remember_world(store: &Store, name: &str) -> Result<bool> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(false);
    }
    let mut worlds: Vec<String> = store
        .setting(WORLDS_KEY)?
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    if worlds.first().map(String::as_str) == Some(name) {
        return Ok(false);
    }
    worlds.retain(|w| !w.eq_ignore_ascii_case(name));
    worlds.insert(0, name.to_string());
    worlds.truncate(MAX_WORLDS);
    store.set_setting(WORLDS_KEY, &serde_json::to_string(&worlds)?)?;
    Ok(true)
}

/// Assemble the whole vocabulary from the database.
pub fn read(store: &Store, cap: usize) -> Result<Vocabulary> {
    let user = user_terms(store)?;
    let worlds: Vec<String> = store
        .setting(WORLDS_KEY)?
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let auto = AutoTerms {
        roster: clean_terms(&store.recent_roster_names(cap)?),
        worlds: clean_terms(&worlds),
        corrections: clean_terms(&store.correction_terms(cap)?),
        speakers: clean_terms(&store.named_speakers()?),
    };
    let effective = effective(&user, &auto, cap);
    Ok(Vocabulary {
        user,
        auto,
        effective,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_user_glossary_outranks_everything_the_daemon_noticed() {
        let auto = AutoTerms {
            roster: strings(&["Nyx", "Kübra"]),
            worlds: strings(&["The Great Pug"]),
            corrections: strings(&["Denglisch"]),
            speakers: strings(&["Lena"]),
        };
        let out = effective(&strings(&["Vergaberecht"]), &auto, 3);
        assert_eq!(out, strings(&["Vergaberecht", "Lena", "Denglisch"]));
    }

    #[test]
    fn duplicates_collapse_case_insensitively_and_keep_the_first_spelling() {
        let auto = AutoTerms {
            roster: strings(&["kübra", "NYX"]),
            speakers: strings(&["Kübra"]),
            ..Default::default()
        };
        let out = effective(&[], &auto, 10);
        assert_eq!(out, strings(&["Kübra", "NYX"]));
    }

    #[test]
    fn the_cap_bites_at_the_end_of_the_priority_order() {
        let auto = AutoTerms {
            roster: strings(&["a-one", "b-two", "c-three"]),
            ..Default::default()
        };
        assert_eq!(effective(&[], &auto, 2), strings(&["a-one", "b-two"]));
        assert!(effective(&[], &auto, 0).is_empty());
    }

    #[test]
    fn a_pasted_sentence_is_not_a_term() {
        let terms = strings(&[
            "Kübra",
            "The Great Pug",
            "   ",
            "this is a whole sentence somebody pasted in",
        ]);
        assert_eq!(clean_terms(&terms), strings(&["Kübra", "The Great Pug"]));
    }

    #[test]
    fn whitespace_inside_a_term_is_normalised_not_rejected() {
        assert_eq!(
            clean_terms(&strings(&["  The   Great  Pug "])),
            strings(&["The Great Pug"])
        );
    }

    #[test]
    fn a_correction_contributes_only_the_words_it_added() {
        assert_eq!(
            corrected_words(
                "we were in the great pug yesterday",
                "we were in Kübras Welt yesterday"
            ),
            strings(&["Kübras", "Welt"])
        );
    }

    #[test]
    fn a_correction_that_only_moved_punctuation_contributes_nothing() {
        assert!(corrected_words("hallo, wie geht's", "Hallo wie geht's!").is_empty());
    }

    #[test]
    fn short_words_are_never_glossary_material() {
        // "es" is not a name the model needs help with, and a two-letter
        // hotword is a token path half the vocabulary walks.
        assert!(corrected_words("das war", "das es war").is_empty());
    }
}
