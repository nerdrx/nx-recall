//! A translator that only translates (PROTOCOL 0.11.0).
//!
//! 0.9.0 translated turns by *asking a chat model to*: qwen2.5-3b, a system
//! prompt made of six refusals, a grammar that admits one field, and three
//! guards on the way back ([`crate::translate`]). Every one of those parts
//! exists because a general model shown a line of conversation would rather
//! answer it than translate it.
//!
//! NLLB-200-distilled-600M has no such temptation. It is a sequence-to-sequence
//! translator with the target language as its first decoder token; there is no
//! prompt to argue with, no instruction to ignore, and no way for it to reply
//! to the sentence instead. It is also a fifth of the parameters and runs
//! inside this process through `ort`, which the daemon already links for
//! [`crate::semantic`], rather than as a 1.9 GB child.
//!
//! `spike/nllb_bench.py` measured it against the shipped Qwen path on thirteen
//! directions of FLEURS, 100 parallel sentences each, by chrF and by the e5
//! cosine §16 used. FINDINGS §23 has the table.
//!
//! ## The licence, said plainly
//!
//! NLLB-200 is **CC-BY-NC 4.0**. That is fine for this install — a private,
//! personal daemon on one person's machine — and it is a blocker for anything
//! commercial: if NX Hub ever sells this, the translator group cannot ship with
//! it. It is an *optional* download for exactly that reason, and the catalogue
//! entry says so next to the bytes.
//!
//! ## The export, and the one thing about it that is not obvious
//!
//! Two ONNX graphs from the transformers.js mirror, int8: an encoder, and a
//! **merged** decoder — one graph carrying both the no-cache and the
//! with-cache paths behind an `If` node switched by a `use_cache_branch`
//! input. On a cached step that graph returns `present.*.encoder.*` as
//! `(0, 16, 1, 64)` *dummies*, not the cross-attention cache it was given.
//! Feeding those back is a `Reshape` failure two tokens into every sentence,
//! and it is the only interesting bug in this file: the cross-attention KV is
//! computed once, on the first step, and then **held**. Only the self-attention
//! cache grows.
//!
//! ## Discipline
//!
//! One session pair, loaded once, used from the assistant worker's thread —
//! which already carries the project's nice and CPU pin
//! ([`crate::pipeline::deprioritise_current_thread`]), so the ONNX threads
//! inherit both. Greedy decoding, 128 tokens maximum: a turn is a sentence, and
//! a translator that has said 128 tokens about one has stopped translating.

use std::path::Path;

use anyhow::{Context, Result, bail};
use ort::session::{Session, SessionInputValue};
use ort::value::TensorRef;

/// Decoder layers in the distilled 600M. Asserted against the export on load,
/// so a swapped model file is an error rather than a silently wrong cache.
const LAYERS: usize = 12;
/// Attention heads, and the per-head width. Same reason.
const HEADS: i64 = 16;
const HEAD_DIM: i64 = 64;

/// `</s>`, which is both the end of a sentence and the decoder's start token.
const EOS: i64 = 2;

/// Tokens one translation may take. The same bound, and the same argument, as
/// [`crate::translate`]'s 400-token cap on the Qwen path: four sentences is a
/// long turn and anything past it is not a translation any more.
pub const MAX_NEW_TOKENS: usize = 128;

/// Longest source accepted, in tokens. NLLB's positions run to 1024; a turn is
/// a second or two of speech, so this only bounds a pathological transcript.
pub const MAX_SOURCE_TOKENS: usize = 256;

/// Our two-letter language tags, as NLLB spells them.
///
/// NLLB names a language *and a script*, which is the whole reason this table
/// exists rather than a string concatenation: `sr_Cyrl` and `sr_Latn` are the
/// same language and different tokens, and guessing wrong produces fluent
/// output in the wrong alphabet. Only the tags this daemon can actually see are
/// here — the twenty [`crate::lang`] knows about — and an unknown tag is
/// `None`, which means "do not translate", not "guess".
///
/// `spike/nllb_bench.py` carries the same table, and
/// [`tests::the_bench_and_the_daemon_speak_the_same_languages`] asserts they
/// agree: a bench measuring `spa_Latn` while the daemon runs `por_Latn` would
/// be a table of numbers about nothing.
pub const CODES: &[(&str, &str)] = &[
    ("ar", "arb_Arab"),
    ("cs", "ces_Latn"),
    ("da", "dan_Latn"),
    ("de", "deu_Latn"),
    ("el", "ell_Grek"),
    ("en", "eng_Latn"),
    ("es", "spa_Latn"),
    ("fi", "fin_Latn"),
    ("fr", "fra_Latn"),
    ("it", "ita_Latn"),
    ("ja", "jpn_Jpan"),
    ("ko", "kor_Hang"),
    ("nl", "nld_Latn"),
    ("no", "nob_Latn"),
    ("pl", "pol_Latn"),
    ("pt", "por_Latn"),
    ("ru", "rus_Cyrl"),
    ("sv", "swe_Latn"),
    ("tr", "tur_Latn"),
    ("uk", "ukr_Cyrl"),
    ("zh", "zho_Hans"),
];

/// The NLLB code for one of our tags, or `None` for a language this model is
/// not being asked to attempt.
pub fn code_for(tag: &str) -> Option<&'static str> {
    let tag = tag.trim().to_ascii_lowercase();
    CODES.iter().find(|(k, _)| *k == tag).map(|(_, v)| *v)
}

/// The loaded translator: two sessions and a tokenizer.
pub struct Nllb {
    encoder: Session,
    decoder: Session,
    tokenizer: tokenizers::Tokenizer,
    model_id: String,
}

/// One layer's key/value pair, owned. The outputs of a `Session::run` borrow
/// the session, so every step copies what the next one needs out of them before
/// the borrow ends — which is also what lets the self-attention cache be kept
/// while the cross-attention one is not touched again.
#[derive(Clone)]
struct Kv {
    key: Vec<f32>,
    value: Vec<f32>,
    /// Length along the sequence axis; the other three are fixed.
    len: i64,
}

impl Kv {
    fn empty() -> Self {
        Self {
            key: Vec::new(),
            value: Vec::new(),
            len: 0,
        }
    }

    fn shape(&self) -> [i64; 4] {
        [1, HEADS, self.len, HEAD_DIM]
    }
}

impl Nllb {
    /// Load from a resolved [`crate::models::TranslatorModel`].
    pub fn load(m: &crate::models::TranslatorModel, threads: i32) -> Result<Self> {
        Self::load_at(&m.encoder, &m.decoder, &m.tokenizer, m.model_id(), threads)
    }

    pub fn load_at(
        encoder: &Path,
        decoder: &Path,
        tokenizer: &Path,
        model_id: String,
        threads: i32,
    ) -> Result<Self> {
        let session = |path: &Path| -> Result<Session> {
            Session::builder()
                .map_err(|e| anyhow::anyhow!("creating ONNX session builder: {e}"))?
                .with_intra_threads(threads.max(1) as usize)
                .map_err(|e| anyhow::anyhow!("configuring intra-op threads: {e}"))?
                .with_inter_threads(1)
                .map_err(|e| anyhow::anyhow!("configuring inter-op threads: {e}"))?
                .commit_from_file(path)
                .with_context(|| format!("loading the translator {}", path.display()))
        };
        let encoder = session(encoder)?;
        let decoder = session(decoder)?;

        // What the export must have for the loop below to be right. A model
        // file that does not is refused here rather than producing plausible
        // nonsense: a wrong layer count silently drops half the cache.
        let names: Vec<&str> = decoder.inputs().iter().map(|i| i.name()).collect();
        if !names.contains(&"use_cache_branch") {
            bail!(
                "this translator export is not the MERGED decoder — its inputs are {:?}. \
                 Only the merged export carries both the cache and no-cache paths, which \
                 is what this loop drives.",
                &names[..names.len().min(6)]
            );
        }
        for i in 0..LAYERS {
            for k in [
                "decoder.key",
                "decoder.value",
                "encoder.key",
                "encoder.value",
            ] {
                let want = format!("past_key_values.{i}.{k}");
                if !names.iter().any(|n| *n == want) {
                    bail!("the translator export has no {want}; it is not a {LAYERS}-layer NLLB");
                }
            }
        }

        let mut tokenizer = tokenizers::Tokenizer::from_file(tokenizer)
            .map_err(|e| anyhow::anyhow!("loading the translator's tokenizer: {e}"))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_SOURCE_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("configuring tokenizer truncation: {e}"))?;
        // The saved `tokenizer.json` carries a post-processor that hard-codes
        // `eng_Latn` as the source-language token, because that is whatever the
        // exporter's `src_lang` happened to be. Using it would mark every
        // Japanese line as English. The sequence is built by hand below
        // instead, and this is the check that says so out loud.
        if tokenizer.token_to_id("eng_Latn").is_none() {
            bail!("the translator's tokenizer has no language tokens; it is not NLLB's");
        }

        Ok(Self {
            encoder,
            decoder,
            tokenizer,
            model_id,
        })
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// One line, from `src` into `tgt`, both as our two-letter tags.
    ///
    /// Greedy. Beam search buys about a chrF point on FLEURS and costs the
    /// beam width in latency; a turn on a transcript row is not worth four
    /// times the compute, and the guards in [`crate::translate`] care about
    /// gross failures, which beams do not fix.
    pub fn translate(&mut self, text: &str, src: &str, tgt: &str) -> Result<String> {
        let (Some(src_code), Some(tgt_code)) = (code_for(src), code_for(tgt)) else {
            bail!("no NLLB language code for {src:?} -> {tgt:?}");
        };
        let src_id = self
            .tokenizer
            .token_to_id(src_code)
            .with_context(|| format!("the tokenizer has no {src_code} token"))?
            as i64;
        let tgt_id = self
            .tokenizer
            .token_to_id(tgt_code)
            .with_context(|| format!("the tokenizer has no {tgt_code} token"))?
            as i64;

        // `<src_lang> … </s>`, built here rather than by the post-processor:
        // see the note in `load_at`.
        let enc = self
            .tokenizer
            .encode(text, false)
            .map_err(|e| anyhow::anyhow!("tokenising for the translator: {e}"))?;
        let mut ids: Vec<i64> = Vec::with_capacity(enc.get_ids().len() + 2);
        ids.push(src_id);
        ids.extend(enc.get_ids().iter().map(|&i| i64::from(i)));
        ids.push(EOS);
        let n = ids.len() as i64;
        let mask = vec![1i64; ids.len()];

        // ---- the encoder, once ----
        let (hidden, hidden_shape) = {
            let out = self.encoder.run(vec![
                (
                    "input_ids",
                    SessionInputValue::from(TensorRef::from_array_view((
                        [1i64, n],
                        ids.as_slice(),
                    ))?),
                ),
                (
                    "attention_mask",
                    SessionInputValue::from(TensorRef::from_array_view((
                        [1i64, n],
                        mask.as_slice(),
                    ))?),
                ),
            ])?;
            let (shape, data) = out["last_hidden_state"].try_extract_tensor::<f32>()?;
            (data.to_vec(), shape.to_vec())
        };

        // ---- the decoder, one token at a time ----
        let names = past_names();
        let mut out_ids: Vec<i64> = vec![EOS, tgt_id];
        let mut self_kv: Vec<Kv> = vec![Kv::empty(); LAYERS];
        let mut cross_kv: Vec<Kv> = vec![Kv::empty(); LAYERS];
        let mut first = true;

        for _ in 0..MAX_NEW_TOKENS {
            // Every buffer a tensor view borrows has to outlive `inputs`, so
            // all of them are declared before it.
            let step_ids: Vec<i64> = if first {
                out_ids.clone()
            } else {
                vec![*out_ids.last().expect("two tokens are always present")]
            };
            let step_n = step_ids.len() as i64;
            let branch = [!first];
            let self_shapes: Vec<[i64; 4]> = self_kv.iter().map(Kv::shape).collect();
            let cross_shapes: Vec<[i64; 4]> = cross_kv.iter().map(Kv::shape).collect();

            let mut inputs: Vec<(&str, SessionInputValue)> = Vec::with_capacity(4 + LAYERS * 4);
            inputs.push((
                "encoder_attention_mask",
                TensorRef::from_array_view(([1i64, n], mask.as_slice()))?.into(),
            ));
            inputs.push((
                "encoder_hidden_states",
                TensorRef::from_array_view((hidden_shape.as_slice(), hidden.as_slice()))?.into(),
            ));
            inputs.push((
                "input_ids",
                TensorRef::from_array_view(([1i64, step_n], step_ids.as_slice()))?.into(),
            ));
            inputs.push((
                "use_cache_branch",
                TensorRef::from_array_view(([1i64], branch.as_slice()))?.into(),
            ));
            for i in 0..LAYERS {
                for (slot, kv, shape) in [
                    (0, &self_kv[i], &self_shapes[i]),
                    (2, &cross_kv[i], &cross_shapes[i]),
                ] {
                    inputs.push((
                        names[i][slot].as_str(),
                        TensorRef::from_array_view((shape.as_slice(), kv.key.as_slice()))?.into(),
                    ));
                    inputs.push((
                        names[i][slot + 1].as_str(),
                        TensorRef::from_array_view((shape.as_slice(), kv.value.as_slice()))?.into(),
                    ));
                }
            }

            let out = self.decoder.run(inputs)?;
            let (shape, logits) = out["logits"].try_extract_tensor::<f32>()?;
            if shape.len() != 3 || shape[0] != 1 {
                bail!("unexpected decoder output shape {shape:?}; expected [1, tokens, vocab]");
            }
            let vocab = shape[2] as usize;
            let next = argmax(&logits[logits.len() - vocab..]) as i64;

            // Copy the self-attention cache forward. The cross-attention one is
            // taken only on the first step: after that the merged export
            // returns dummies for it (see the module note), and re-feeding
            // those is a crash rather than a wrong answer, which is at least
            // honest of it.
            let mut next_self: Vec<Kv> = Vec::with_capacity(LAYERS);
            let mut next_cross: Vec<Kv> = Vec::with_capacity(LAYERS);
            for i in 0..LAYERS {
                let (ks, kd) =
                    out[format!("present.{i}.decoder.key").as_str()].try_extract_tensor::<f32>()?;
                let (_, vd) = out[format!("present.{i}.decoder.value").as_str()]
                    .try_extract_tensor::<f32>()?;
                next_self.push(Kv {
                    key: kd.to_vec(),
                    value: vd.to_vec(),
                    len: ks[2],
                });
                if first {
                    let (cs, ck) = out[format!("present.{i}.encoder.key").as_str()]
                        .try_extract_tensor::<f32>()?;
                    let (_, cv) = out[format!("present.{i}.encoder.value").as_str()]
                        .try_extract_tensor::<f32>()?;
                    next_cross.push(Kv {
                        key: ck.to_vec(),
                        value: cv.to_vec(),
                        len: cs[2],
                    });
                }
            }
            drop(out);
            self_kv = next_self;
            if first {
                cross_kv = next_cross;
            }
            first = false;
            if next == EOS {
                break;
            }
            out_ids.push(next);
        }

        // `out_ids[0]` is the decoder's start token and `[1]` is the target
        // language; neither is a word anybody said.
        let words: Vec<u32> = out_ids[2..].iter().map(|&i| i as u32).collect();
        let text = self
            .tokenizer
            .decode(&words, true)
            .map_err(|e| anyhow::anyhow!("detokenising the translation: {e}"))?;
        Ok(text.trim().to_string())
    }
}

fn argmax(v: &[f32]) -> usize {
    let mut best = 0usize;
    let mut best_v = f32::NEG_INFINITY;
    for (i, &x) in v.iter().enumerate() {
        if x > best_v {
            best_v = x;
            best = i;
        }
    }
    best
}

/// `past_key_values.<layer>.{decoder,encoder}.{key,value}`, in the order the
/// loop above indexes them: self key, self value, cross key, cross value.
fn past_names() -> Vec<[String; 4]> {
    (0..LAYERS)
        .map(|i| {
            [
                format!("past_key_values.{i}.decoder.key"),
                format!("past_key_values.{i}.decoder.value"),
                format!("past_key_values.{i}.encoder.key"),
                format!("past_key_values.{i}.encoder.value"),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn our_tags_map_to_a_language_and_a_script() {
        assert_eq!(code_for("de"), Some("deu_Latn"));
        assert_eq!(code_for("DE"), Some("deu_Latn"), "case-folded");
        assert_eq!(code_for(" ja "), Some("jpn_Jpan"));
        assert_eq!(code_for("ru"), Some("rus_Cyrl"));
        assert_eq!(code_for("el"), Some("ell_Grek"));
        assert_eq!(code_for("ko"), Some("kor_Hang"));
        // An unknown tag is not translated, and emphatically not guessed at:
        // there is no rule that turns a two-letter tag into a script.
        assert_eq!(code_for("zz"), None);
        assert_eq!(code_for(""), None);
    }

    /// Every language this daemon offers has a code here.
    ///
    /// Two lists, and both matter. `lang::GUESSABLE` is what
    /// `lang::guess_other` may stamp on a row — a tag it can produce and this
    /// table has never heard of is a turn that silently stops being translated.
    /// `lang::OFFERED` is what a person may choose as a TARGET, which is the
    /// other end of the same feature and includes the two the classifier reads
    /// plus Norwegian, which is offered as a target and never guessed.
    #[test]
    fn every_language_this_daemon_offers_can_be_translated() {
        for tag in crate::lang::GUESSABLE {
            assert!(
                code_for(tag).is_some(),
                "lang::guess_other can return {tag:?} and the translator has no code for it"
            );
        }
        for (tag, name) in crate::lang::OFFERED {
            assert!(
                code_for(tag).is_some(),
                "{name} ({tag}) can be chosen as a target and has no NLLB code"
            );
        }
    }

    #[test]
    fn the_table_is_sorted_and_has_no_duplicates() {
        let mut tags: Vec<&str> = CODES.iter().map(|(k, _)| *k).collect();
        let n = tags.len();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), n, "a duplicated tag");
        assert!(
            CODES.windows(2).all(|w| w[0].0 < w[1].0),
            "keep the table sorted so a missing language is visible"
        );
        for (_, code) in CODES {
            let (lang, script) = code.split_once('_').expect("lang_Script");
            assert_eq!(lang.len(), 3, "{code}");
            assert_eq!(script.len(), 4, "{code}");
        }
    }

    /// The bench's table and this one are the same table. A comparison run
    /// against `por_Latn` for a daemon that ships `spa_Latn` measures nothing.
    #[test]
    fn the_bench_and_the_daemon_speak_the_same_languages() {
        let bench = include_str!("../../../spike/nllb_bench.py");
        for (tag, code) in CODES {
            let want = format!("\"{tag}\": (");
            let line = bench
                .lines()
                .find(|l| l.trim_start().starts_with(&want))
                .unwrap_or_else(|| panic!("spike/nllb_bench.py has no entry for {tag:?}"));
            assert!(
                line.contains(code),
                "spike/nllb_bench.py maps {tag:?} to something other than {code}: {line}"
            );
        }
    }

    #[test]
    fn the_layer_geometry_is_the_distilled_600m() {
        // Not decoration: the loop feeds exactly `LAYERS` cache pairs, and a
        // model with a different depth would be driven with half a cache and
        // would still produce fluent-looking output.
        assert_eq!(LAYERS, 12);
        assert_eq!(HEADS * HEAD_DIM, 1024, "d_model");
        let kv = Kv::empty();
        assert_eq!(kv.shape(), [1, 16, 0, 64]);
    }

    #[test]
    fn argmax_picks_the_largest_and_the_first_of_a_tie() {
        assert_eq!(argmax(&[0.1, 0.9, 0.3]), 1);
        assert_eq!(argmax(&[1.0, 1.0]), 0);
        assert_eq!(argmax(&[f32::NEG_INFINITY, -1.0]), 1);
    }

    #[test]
    fn the_cache_input_names_are_the_ones_the_export_declares() {
        let names = past_names();
        assert_eq!(names.len(), LAYERS);
        assert_eq!(names[0][0], "past_key_values.0.decoder.key");
        assert_eq!(names[0][3], "past_key_values.0.encoder.value");
        assert_eq!(names[11][2], "past_key_values.11.encoder.key");
    }

    // ---- against the real model --------------------------------------------

    /// Gated on `NXR_TRANSLATOR_MODELS`, following the `NXR_GRAPH_MODELS`
    /// convention: absent, this skips and passes, because the export is a
    /// ~910 MB optional download and CI does not have one.
    fn staged() -> Option<Nllb> {
        let raw = std::env::var("NXR_TRANSLATOR_MODELS").ok()?;
        if raw.trim().is_empty() {
            return None;
        }
        let root = std::path::PathBuf::from(raw);
        assert!(
            root.is_absolute(),
            "NXR_TRANSLATOR_MODELS must be an absolute path, got {}",
            root.display()
        );
        let m = crate::models::TranslatorModel::resolve_at(root);
        assert!(
            m.present(),
            "NXR_TRANSLATOR_MODELS={} has no {} — `recalld models fetch --translator`",
            m.root.display(),
            crate::models::TRANSLATOR_DIR
        );
        Some(Nllb::load(&m, 4).expect("the translator loads"))
    }

    #[test]
    fn the_real_model_translates_a_german_and_a_japanese_line_into_english() {
        let Some(mut nllb) = staged() else {
            eprintln!("skipping the translator round trip: set NXR_TRANSLATOR_MODELS=<models dir>");
            return;
        };
        let de = nllb
            .translate("ich schicke dir morgen den Link", "de", "en")
            .expect("the German line");
        eprintln!("de -> en: {de:?}");
        assert!(de.to_lowercase().contains("link"), "{de:?}");
        assert_eq!(crate::lang::classify(&de), crate::lang::Lang::En, "{de:?}");

        // Japanese, which is the case that proves the language token is being
        // set by hand: the saved tokenizer's post-processor would have marked
        // this line `eng_Latn`.
        let ja = nllb
            .translate("明日リンクを送ります", "ja", "en")
            .expect("the Japanese line");
        eprintln!("ja -> en: {ja:?}");
        assert!(
            ja.to_lowercase().contains("link") || ja.to_lowercase().contains("send"),
            "{ja:?}"
        );
        assert!(
            ja.is_ascii(),
            "a Japanese answer to an English request: {ja:?}"
        );

        // …and the other direction, because the whole feature is two-way.
        let en = nllb
            .translate("i will send you the link tomorrow", "en", "de")
            .expect("the English line");
        eprintln!("en -> de: {en:?}");
        assert_eq!(crate::lang::classify(&en), crate::lang::Lang::De, "{en:?}");
    }

    #[test]
    fn a_language_the_table_does_not_have_is_an_error_not_a_guess() {
        let Some(mut nllb) = staged() else {
            return;
        };
        let err = nllb
            .translate("hello there", "en", "zz")
            .expect_err("an unknown target");
        assert!(err.to_string().contains("no NLLB language code"), "{err}");
    }
}
