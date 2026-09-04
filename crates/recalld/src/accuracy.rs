//! How wrong the transcripts are, measured from the corrections
//! (PROTOCOL 0.8.0, `accuracy.summary`).
//!
//! There is no reference transcript on this machine and there never will be —
//! nobody is going to hand-label an evening of VRChat. But every time a person
//! fixes a line they produce one: the corrected text is the truth, the text it
//! replaced is what the model heard, and the distance between them is a word
//! error rate on a sample of exactly the turns that were worth fixing.
//!
//! That sample is **biased and the number must be read as such**: people
//! correct the lines that matter and the lines that are wrong, so this is the
//! error rate *of the turns somebody bothered about*, not of the corpus. It is
//! still the only real measurement available, and it is the one that moves when
//! the vocabulary, the window length or the model changes — which is what it is
//! for.
//!
//! ### Where the original text comes from
//!
//! `segments.correct` already logs it. The operations row is
//!
//! ```json
//! {"op": "segments.correct",
//!  "target_ids": "[42]",
//!  "prior_state": {"segment_id": 42, "text": "the belt holds"}}
//! ```
//!
//! — `prior_state.text` is the pre-correction transcript, written by
//! `Service::segments_correct` from `Store::segment_state` before the row is
//! rewritten. Nothing had to be extended; this module reads history that was
//! already being kept. (`prior_state.text` is `null` on a turn that had no
//! transcript at all, and those are skipped: an insertion from nothing is not
//! an error rate.)
//!
//! The *corrected* text is not in the log, because a log records what was
//! replaced rather than what replaced it. It is recovered by reading the
//! corrections of one segment in order: each correction's result is the next
//! one's `prior_state.text`, and the last one's result is the row as it now
//! stands. A turn corrected twice therefore contributes two measurements, which
//! is right — the second correction is evidence the first transcript was wrong
//! too.

use anyhow::Result;
use serde_json::{Value, json};

use crate::clock::ns_to_ms;
use crate::store::Store;

/// The operation this whole module reads.
pub const OP: &str = "segments.correct";

/// How many corrections one summary walks. A person who has corrected more
/// than this has a better estimate than they need.
const MAX_CORRECTIONS: usize = 10_000;

/// One measurement: what the model heard, and what it should have heard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    pub segment_id: i64,
    pub original: String,
    pub corrected: String,
    pub at_utc_ns: i64,
}

impl Correction {
    /// Word errors over reference words. Unbounded above, like every WER: a
    /// one-word transcript corrected into eight words scores 7.0.
    pub fn wer(&self) -> Option<f64> {
        wer(&self.original, &self.corrected)
    }

    /// The two numbers the summary actually adds up (0.10.1): word edits, and
    /// the longer of the two word counts. Summed across corrections and then
    /// divided, that is a rate between 0 and 1 — the share of words in the
    /// fixed lines that had to change. Averaging per-line WERs was the
    /// mistake the first version made: one two-word mishearing retyped as ten
    /// words is 400% on its own and dragged a thirteen-line average to 113%,
    /// a figure that means nothing to the person reading it.
    pub fn edits(&self) -> Option<(usize, usize)> {
        let a: Vec<&str> = self.original.split_whitespace().collect();
        let b: Vec<&str> = self.corrected.split_whitespace().collect();
        if b.is_empty() {
            return None;
        }
        Some((edit_distance(&a, &b), a.len().max(b.len())))
    }
}

/// Word-level error rate of `heard` against `truth`. `None` when the reference
/// has no words to divide by — an empty correction is a deletion, not a
/// measurement.
pub fn wer(heard: &str, truth: &str) -> Option<f64> {
    let a: Vec<&str> = heard.split_whitespace().collect();
    let b: Vec<&str> = truth.split_whitespace().collect();
    if b.is_empty() {
        return None;
    }
    Some(edit_distance(&a, &b) as f64 / b.len() as f64)
}

/// Levenshtein over whole words, two rows at a time.
fn edit_distance(a: &[&str], b: &[&str]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for (i, x) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(x != y);
            cur[j + 1] = sub.min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Every correction in the log, paired with the text that replaced it.
pub fn corrections(store: &Store) -> Result<Vec<Correction>> {
    let ops = store.operations_of(OP, MAX_CORRECTIONS)?;
    // Oldest first per segment, which `operations_of` already guarantees.
    let mut by_segment: std::collections::HashMap<i64, Vec<(i64, String)>> = Default::default();
    let mut order: Vec<i64> = Vec::new();
    for op in &ops {
        let Ok(prior) = serde_json::from_str::<Value>(&op.prior_state) else {
            continue;
        };
        let Some(segment_id) = prior["segment_id"].as_i64() else {
            continue;
        };
        // A null `text` is a turn that had no transcript before the edit.
        let Some(text) = prior["text"].as_str() else {
            continue;
        };
        let slot = by_segment.entry(segment_id).or_default();
        if slot.is_empty() {
            order.push(segment_id);
        }
        slot.push((op.at_utc_ns, text.to_string()));
    }

    let mut out = Vec::new();
    for segment_id in order {
        let entries = &by_segment[&segment_id];
        // The last correction's result is the row as it stands now. A purged
        // segment has none, and its final correction is dropped rather than
        // measured against nothing.
        let current = store
            .segment_row(segment_id)?
            .and_then(|r| r.text)
            .unwrap_or_default();
        for (i, (at, original)) in entries.iter().enumerate() {
            let corrected = match entries.get(i + 1) {
                Some((_, next)) => next.clone(),
                None => current.clone(),
            };
            out.push(Correction {
                segment_id,
                original: original.clone(),
                corrected,
                at_utc_ns: *at,
            });
        }
    }
    out.sort_by_key(|c| (c.at_utc_ns, c.segment_id));
    Ok(out)
}

/// `accuracy.summary`'s whole reply.
pub fn summary(store: &Store) -> Result<Value> {
    let corrections = corrections(store)?;
    let mut overall = Bucket::default();
    let mut by_source: Vec<(String, Bucket)> = Vec::new();
    let mut by_speaker: Vec<(Option<i64>, Bucket)> = Vec::new();
    let mut since: Option<i64> = None;

    for c in &corrections {
        let Some((edits, words)) = c.edits() else {
            continue;
        };
        since = Some(since.map_or(c.at_utc_ns, |s: i64| s.min(c.at_utc_ns)));
        overall.add(edits, words);
        // The row is read now, not from the log: a segment reassigned since
        // the correction belongs to the voice it belongs to today, which is
        // the same rule every other retroactive change in this daemon follows.
        let Some(row) = store.segment_row(c.segment_id)? else {
            continue;
        };
        bucket(&mut by_source, row.source.clone()).add(edits, words);
        bucket(&mut by_speaker, row.speaker_id).add(edits, words);
    }

    // The unbiased half (0.10.1): the second decoder's verdict exists on every
    // checked row, fixed or not, so its shaky share says something about the
    // whole transcript rather than about the lines somebody chose to retype.
    for cc in store.confidence_counts()? {
        let (solid, shaky) = match cc.confidence.as_deref() {
            Some("solid") => (cc.n, 0),
            Some("shaky") => (0, cc.n),
            _ => continue,
        };
        overall.solid += solid;
        overall.shaky += shaky;
        let b = bucket(&mut by_source, cc.source.clone());
        b.solid += solid;
        b.shaky += shaky;
        let b = bucket(&mut by_speaker, cc.speaker_id);
        b.solid += solid;
        b.shaky += shaky;
    }

    // Most evidence first: corrections, then checked rows, then the name.
    by_source.sort_by(|a, b| {
        b.1.n
            .cmp(&a.1.n)
            .then(b.1.checked().cmp(&a.1.checked()))
            .then(a.0.cmp(&b.0))
    });
    by_speaker.sort_by(|a, b| {
        b.1.n
            .cmp(&a.1.n)
            .then(b.1.checked().cmp(&a.1.checked()))
            .then(a.0.cmp(&b.0))
    });

    Ok(json!({
        "corrections": overall.n,
        // Kept under its old key for old clients; the meaning is now the
        // bounded corpus edit rate described on `Bucket::rate`.
        "estimated_wer": overall.rate(),
        "edit_rate": overall.rate(),
        "cross_check": overall.cross_check_json(),
        "by_source": by_source
            .iter()
            .map(|(source, b)| json!({
                "source": source,
                "corrections": b.n,
                "estimated_wer": b.rate(),
                "edit_rate": b.rate(),
                "cross_check": b.cross_check_json(),
            }))
            .collect::<Vec<_>>(),
        "by_speaker": by_speaker
            .iter()
            .map(|(speaker_id, b)| json!({
                "speaker_id": speaker_id,
                "corrections": b.n,
                "estimated_wer": b.rate(),
                "edit_rate": b.rate(),
                "cross_check": b.cross_check_json(),
            }))
            .collect::<Vec<_>>(),
        // The oldest correction counted: the window this estimate is over.
        // Null when nobody has corrected anything, which is not an error rate
        // of zero and must not be rendered as one.
        "since_ns": since.map(|ns| ns.to_string()),
        "since_ms": since.map(ns_to_ms),
        // 0.12.4: what those same corrections have taught the daemon about
        // which decoder to believe. Additive; a client that has never heard of
        // it ignores it. See `crate::text_truth`.
        "learned": crate::text_truth::summary_json(store)?,
    }))
}

#[derive(Debug, Default, Clone, Copy)]
struct Bucket {
    n: i64,
    edits: usize,
    words: usize,
    /// The cross-check's verdicts over EVERY row in this bucket (0.10.1), not
    /// only the corrected ones: `solid`, `shaky`, and how many were checked.
    solid: i64,
    shaky: i64,
}

impl Bucket {
    fn add(&mut self, edits: usize, words: usize) {
        self.n += 1;
        self.edits += edits;
        self.words += words;
    }
    /// Words changed over words present, summed across the bucket's
    /// corrections before dividing, so the figure is a share between 0 and 1.
    /// 0.10.1: the first version averaged per-line WERs, which is unbounded
    /// (a two-word line retyped as ten is 400%) and put "112.9% error" on the
    /// card — see `Correction::edits`.
    fn rate(&self) -> Option<f64> {
        (self.words > 0).then(|| self.edits as f64 / self.words as f64)
    }
    fn checked(&self) -> i64 {
        self.solid + self.shaky
    }
    fn shaky_share(&self) -> Option<f64> {
        (self.checked() > 0).then(|| self.shaky as f64 / self.checked() as f64)
    }
    fn cross_check_json(&self) -> Value {
        json!({
            "checked": self.checked(),
            "solid": self.solid,
            "shaky": self.shaky,
            "shaky_share": self.shaky_share(),
        })
    }
}

fn bucket<K: PartialEq>(buckets: &mut Vec<(K, Bucket)>, key: K) -> &mut Bucket {
    if let Some(i) = buckets.iter().position(|(k, _)| *k == key) {
        return &mut buckets[i].1;
    }
    buckets.push((key, Bucket::default()));
    &mut buckets.last_mut().expect("just pushed").1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SegmentAnalysis;
    use serde_json::json;

    #[test]
    fn word_error_rate_counts_words_not_characters() {
        // One substitution in four words.
        assert_eq!(
            wer("the belt holds the line", "the bell holds the line"),
            Some(0.2)
        );
        // A deletion and an insertion.
        assert_eq!(wer("the bell", "the bell tolls"), Some(1.0 / 3.0));
        assert_eq!(wer("the bell tolls now", "the bell tolls"), Some(1.0 / 3.0));
        // Identical is zero, not "no measurement".
        assert_eq!(wer("the bell", "the bell"), Some(0.0));
        // Nothing to divide by.
        assert_eq!(wer("the bell", "   "), None);
    }

    struct Seed {
        store: Store,
        session: i64,
    }

    fn seed(source: &str) -> Seed {
        let store = Store::open_in_memory().unwrap();
        let src = store.upsert_source(source, source, 0).unwrap();
        let session = store.begin_session(src, 0).unwrap();
        Seed { store, session }
    }

    fn a_turn(s: &Seed, t: i64, speaker: Option<i64>, text: &str) -> i64 {
        let id = s
            .store
            .insert_segment(s.session, t, t + 1_000, "segments/a.wav", 0)
            .unwrap();
        s.store
            .set_segment_analysis(
                id,
                &SegmentAnalysis {
                    text: Some(text.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        if let Some(spk) = speaker {
            s.store
                .set_segment_speaker(id, Some(spk), Some(0.9))
                .unwrap();
        }
        id
    }

    /// Exactly what `Service::segments_correct` writes.
    fn correct(s: &Seed, segment_id: i64, was: Option<&str>, now: &str, at: i64) {
        s.store.correct_segment_text(segment_id, now).unwrap();
        s.store
            .log_operation(
                OP,
                &json!([segment_id]).to_string(),
                &json!({"segment_id": segment_id, "text": was}).to_string(),
                at,
            )
            .unwrap();
    }

    #[test]
    fn nothing_corrected_is_not_an_error_rate_of_zero() {
        let s = seed("VRChat.exe");
        a_turn(&s, 1_000, None, "the bell tolls");
        let out = summary(&s.store).unwrap();
        assert_eq!(out["corrections"], json!(0));
        assert_eq!(out["estimated_wer"], Value::Null);
        assert_eq!(out["since_ns"], Value::Null);
        assert_eq!(out["by_source"], json!([]));
    }

    #[test]
    fn one_correction_is_one_measurement_split_by_source_and_speaker() {
        let s = seed("VRChat.exe");
        let spk = s.store.create_speaker("Aspen", 0).unwrap();
        let seg = a_turn(&s, 1_000, Some(spk), "the belt holds the line");
        correct(
            &s,
            seg,
            Some("the belt holds the line"),
            "the bell holds the line",
            77,
        );

        let out = summary(&s.store).unwrap();
        assert_eq!(out["corrections"], json!(1));
        assert_eq!(out["estimated_wer"], json!(0.2));
        assert_eq!(out["since_ns"], json!("77"));
        assert_eq!(out["by_source"][0]["source"], json!("VRChat.exe"));
        assert_eq!(out["by_source"][0]["corrections"], json!(1));
        assert_eq!(out["by_source"][0]["estimated_wer"], json!(0.2));
        assert_eq!(out["by_speaker"][0]["speaker_id"], json!(spk));
        assert_eq!(out["by_speaker"][0]["estimated_wer"], json!(0.2));
    }

    #[test]
    fn a_turn_corrected_twice_contributes_both_measurements() {
        let s = seed("VRChat.exe");
        let seg = a_turn(&s, 1_000, None, "a b c d");
        correct(&s, seg, Some("a b c d"), "a b c e", 10);
        correct(&s, seg, Some("a b c e"), "a b c f", 20);

        let all = corrections(&s.store).unwrap();
        assert_eq!(all.len(), 2);
        // The first correction is measured against what replaced it, which is
        // the SECOND correction's prior state — not against the row as it now
        // stands, two edits later.
        assert_eq!(all[0].original, "a b c d");
        assert_eq!(all[0].corrected, "a b c e");
        // The last one is measured against the row as it now stands.
        assert_eq!(all[1].original, "a b c e");
        assert_eq!(all[1].corrected, "a b c f");
        let out = summary(&s.store).unwrap();
        assert_eq!(out["corrections"], json!(2));
        assert_eq!(out["estimated_wer"], json!(0.25));
        assert_eq!(
            out["since_ns"],
            json!("10"),
            "the window opens at the first"
        );
    }

    #[test]
    fn a_correction_of_a_turn_that_had_no_words_is_not_a_measurement() {
        let s = seed("VRChat.exe");
        let seg = s
            .store
            .insert_segment(s.session, 1_000, 2_000, "segments/a.wav", 0)
            .unwrap();
        correct(&s, seg, None, "what she actually said", 5);
        let out = summary(&s.store).unwrap();
        assert_eq!(out["corrections"], json!(0));
        assert_eq!(out["estimated_wer"], Value::Null);
    }

    #[test]
    fn two_sources_are_reported_apart() {
        let a = seed("VRChat.exe");
        let other = a
            .store
            .upsert_source("Discord.exe", "Discord.exe", 0)
            .unwrap();
        let s2 = a.store.begin_session(other, 0).unwrap();

        let one = a_turn(&a, 1_000, None, "a b c d");
        correct(&a, one, Some("a b c d"), "a b c e", 10);

        let two = a
            .store
            .insert_segment(s2, 2_000, 3_000, "segments/b.wav", 0)
            .unwrap();
        a.store
            .set_segment_analysis(
                two,
                &SegmentAnalysis {
                    text: Some("x y".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        correct(&a, two, Some("x y"), "q r", 20);

        let out = summary(&a.store).unwrap();
        assert_eq!(out["corrections"], json!(2));
        let sources: Vec<&str> = out["by_source"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["source"].as_str().unwrap())
            .collect();
        assert_eq!(sources, vec!["Discord.exe", "VRChat.exe"]);
        // 0.25 for the first, 1.0 for the second, and the overall is their
        // mean rather than a corpus ratio.
        // Pooled, not averaged (0.10.1): 1 edit of 4 words + 4 of 6 = 5/10.
        assert_eq!(out["estimated_wer"], json!(0.5));
    }
}
