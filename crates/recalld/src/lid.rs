//! Which language a turn was *spoken* in, read off the audio (0.11.0).
//!
//! ## Why this exists, when there is already a text classifier
//!
//! Because the text classifier cannot see the failure it would have to catch.
//! Parakeet-TDT-0.6b-v3 covers 25 European languages and no Japanese, and it
//! does not decline a Japanese turn — it *transliterates* it. Measured on the
//! user's own evening: "sumimasen, ogenki desu ka" came back as
//!
//! > Sima Sen Okenki Deska.
//!
//! Latin letters, English-shaped words, capitalised and punctuated. There is
//! nothing in that string for [`crate::lang::classify`] to catch, nothing for
//! [`crate::lang::guess_other`]'s script rule to catch (no kana ever reaches
//! it), and nothing a human skimming a transcript would flag either. Every
//! correction mechanism this daemon has before 0.11.0 reads *text*, so every
//! one of them is blind to it.
//!
//! Nor can the per-speaker tags route around it ([`crate::analysis::Analyzer::
//! correct_language`]): the user speaks Japanese themselves, mid-evening,
//! between German and English turns. A tag is a fact about a voice and this is
//! a fact about a *turn*. The decision has to come from the audio.
//!
//! ## What it is
//!
//! sherpa-onnx's `SpokenLanguageIdentification`: the multilingual Whisper
//! encoder plus one decoder step, read for the language token rather than for
//! words. Whisper-tiny int8 — 12.9 MB of encoder — which is small enough that
//! the question "can we afford to ask on every turn" has a happy answer.
//!
//! ### Measured (`spike/lid_bench.py`, FINDINGS §22)
//!
//! FLEURS test, 200 utterances per language, whisper-tiny int8 on 4 cores:
//!
//! | length | ja recall | de correct | en correct | de→ja | en→ja | RTF   |
//! |--------|----------:|-----------:|-----------:|------:|------:|------:|
//! | full   |    100.0% |      99.5% |     100.0% |  0.0% |  0.0% | 0.016 |
//! | 3.0 s  |     96.5% |      91.0% |     100.0% |  0.0% |  0.0% | 0.019 |
//! | 1.5 s  |     86.5% |      69.5% |      96.5% |  0.0% |  0.0% | 0.031 |
//!
//! The gate was ja recall ≥ 90% at 3 s with de/en → ja ≤ 1% and RTF ≤ 0.05.
//! It clears all three, and the column that decides the design is the fourth
//! and fifth: **not one of 400 German and English utterances was heard as
//! Japanese, at any length.** That asymmetry is what makes the router safe to
//! run at all — the cost of a miss is a turn that stays transliterated, which
//! is what happens today, and the cost of a false positive would be a German
//! turn re-decoded by a model that only speaks Japanese.
//!
//! Whisper-base was measured alongside it and is **not** used, and not to save
//! the download: base has the better recall (99.5% ja at 3 s) and fails the
//! gate in the direction that costs something — it hears 3% of English at 3 s
//! and 5% at 1.5 s as Japanese. A missed Japanese turn stays as wrong as it is
//! today; a German turn handed to a Japanese-only decoder is a new kind of
//! wrong. The full table is on [`LID_WHISPER`]'s catalogue entry.
//!
//! ## No score, and what stands in for one
//!
//! The sherpa C API returns a language string and nothing else —
//! `SherpaOnnxSpokenLanguageIdentificationResult` carries exactly one field,
//! `lang`. There is no posterior to threshold, so [`Reading::confidence`] is
//! built out of the only thing the API does give: **agreement**. LID is run
//! over `windows` slices of the turn and the confidence is the share of them
//! that said the same thing.
//!
//! `windows` defaults to 1, which makes the confidence trivially 1.0, and that
//! default is a measurement rather than a shrug: at zero false positives in
//! 400 negatives there is nothing for a second window to rule out, and a vote
//! would triple the only cost this feature has. The knob is real for a machine
//! that hears something this corpus did not — see `[asr].lid_windows`.

use std::path::Path;

use anyhow::{Context, Result};
use sherpa_rs::language_id::{SpokenLanguageId, SpokenLanguageIdConfig};

use crate::config::SAMPLE_RATE;
use crate::models::{LID_WHISPER, LidModel};

/// What the identifier heard, and how much of the vote it took.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// The BCP-47-ish tag Whisper's language token maps to (`ja`, `de`, `en`,
    /// …). Lower-cased on the way out; sherpa returns it that way already, and
    /// nothing downstream should have to know that.
    pub lang: String,
    /// Share of the windows that agreed, in `(0, 1]`. Always 1.0 when only one
    /// window was asked, which is the default — see the module note.
    pub confidence: f32,
}

impl Reading {
    pub fn is(&self, tag: &str) -> bool {
        self.lang == tag
    }
}

/// The spoken-language identifier, loaded on demand and resident from then on.
///
/// Loaded lazily for the same reason the arbiters are: it is 13 MB of encoder
/// that a machine which never meets a Japanese speaker should not be holding,
/// and "not installed" is a normal state that must cost one warning line rather
/// than one per turn.
pub struct Lid {
    slid: SpokenLanguageId,
    model_id: String,
    windows: usize,
}

impl Lid {
    pub fn load(model: &LidModel, threads: i32, windows: usize) -> Result<Self> {
        let path = |p: &Path| -> Result<String> {
            Ok(p.to_str()
                .with_context(|| format!("model path {} is not valid UTF-8", p.display()))?
                .to_string())
        };
        let slid = SpokenLanguageId::new(SpokenLanguageIdConfig {
            encoder: path(&model.encoder)?,
            decoder: path(&model.decoder)?,
            // The same budget as the primary ASR: this runs on the same
            // deprioritised inference thread and answers about one turn at a
            // time.
            num_threads: Some(threads.max(1)),
            provider: Some("cpu".into()),
            debug: false,
        });
        Ok(Self {
            slid,
            model_id: model.model_id(),
            windows: windows.max(1),
        })
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// What language this audio was spoken in, or `None` if the identifier
    /// declined to say.
    ///
    /// Never returns an error: a language identifier that fails is a language
    /// identifier that has no opinion, and one bad turn must not stop capture.
    pub fn identify(&mut self, samples: &[f32]) -> Option<Reading> {
        if samples.is_empty() {
            return None;
        }
        let mut heard: Vec<String> = Vec::with_capacity(self.windows);
        for window in windows_of(samples, self.windows) {
            match self.slid.compute(window.to_vec(), SAMPLE_RATE) {
                Ok(lang) if !lang.trim().is_empty() => heard.push(lang.trim().to_lowercase()),
                Ok(_) => {}
                Err(e) => {
                    tracing::debug!("the language identifier had no answer: {e}");
                }
            }
        }
        majority(&heard)
    }
}

/// `n` evenly spaced windows over `samples`, each as long as the turn divided
/// by… nothing: every window is the *whole* turn when `n == 1`, and otherwise
/// they are `2/3`-length slices spread across it.
///
/// Overlapping on purpose when the turn is short: the question a second window
/// answers is "does this reading survive being asked about a different part of
/// the audio", and on a 2 s turn two windows share most of their samples but
/// not their edges — which is exactly where a hedged reading falls apart.
fn windows_of(samples: &[f32], n: usize) -> Vec<&[f32]> {
    if n <= 1 || samples.len() < 3 {
        return vec![samples];
    }
    let want = (samples.len() * 2 / 3).max(1);
    let span = samples.len() - want;
    (0..n)
        .map(|i| {
            let start = span * i / (n - 1);
            &samples[start..start + want]
        })
        .collect()
}

/// The most common reading and the share of the vote it took, `None` for no
/// readings at all.
///
/// A tie loses: with an even number of windows and a two-way split there is no
/// majority, and "half of me heard Japanese" is not a reason to hand a turn to
/// a decoder that speaks nothing else. Ties are resolved by taking the first
/// reading only when it is a strict plurality.
fn majority(heard: &[String]) -> Option<Reading> {
    let mut best: Option<(&str, usize)> = None;
    let mut tied = false;
    for lang in heard {
        let n = heard.iter().filter(|l| *l == lang).count();
        match best {
            Some((_, m)) if n < m => {}
            Some((b, m)) if n == m && b != lang.as_str() => tied = true,
            _ => {
                best = Some((lang.as_str(), n));
                tied = false;
            }
        }
    }
    let (lang, n) = best?;
    if tied {
        return None;
    }
    Some(Reading {
        lang: lang.to_string(),
        confidence: n as f32 / heard.len() as f32,
    })
}

/// The line every "it is not installed" message says, so the CLI, the daemon's
/// warning and `models status` all name the same command.
pub fn how_to_get_it() -> String {
    format!(
        "the spoken-language identifier is not installed. `recalld models fetch --japanese` \
         installs {} together with the Japanese decoder ({}), and until then a Japanese turn \
         stays transliterated into Latin nonsense.",
        LID_WHISPER.dir,
        crate::fetch::human(crate::models::japanese_download_bytes()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_window_is_the_whole_turn() {
        let s: Vec<f32> = (0..100).map(|i| i as f32).collect();
        let w = windows_of(&s, 1);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].len(), 100);
    }

    #[test]
    fn several_windows_span_the_turn_and_overlap() {
        let s: Vec<f32> = (0..99).map(|i| i as f32).collect();
        let w = windows_of(&s, 3);
        assert_eq!(w.len(), 3);
        // Two thirds each, and the first and last touch the two ends.
        assert!(w.iter().all(|x| x.len() == 66));
        assert_eq!(w[0][0], 0.0);
        assert_eq!(*w[2].last().unwrap(), 98.0);
        // They overlap, which is the point: the same turn asked about twice.
        assert_ne!(w[0][0], w[1][0]);
    }

    #[test]
    fn a_turn_too_short_to_slice_is_asked_about_once() {
        assert_eq!(windows_of(&[1.0, 2.0], 3).len(), 1);
    }

    #[test]
    fn a_single_reading_is_full_confidence() {
        let r = majority(&["ja".to_string()]).unwrap();
        assert_eq!(r.lang, "ja");
        assert_eq!(r.confidence, 1.0);
        assert!(r.is("ja"));
    }

    #[test]
    fn a_split_vote_reports_the_share_the_winner_took() {
        let r = majority(&["ja".into(), "ja".into(), "en".into()]).unwrap();
        assert_eq!(r.lang, "ja");
        assert!((r.confidence - 2.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn a_tie_is_not_an_answer() {
        // Half of the windows heard Japanese. That is not a reason to hand the
        // turn to a decoder that speaks nothing else.
        assert_eq!(majority(&["ja".into(), "en".into()]), None);
        assert_eq!(majority(&[]), None);
    }
}
