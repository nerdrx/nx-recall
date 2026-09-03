//! Taking back the rows the audio-language route should never have rewritten
//! (0.12.0).
//!
//! ## What there is to take back
//!
//! 45 rows, on one real install, measured the night 0.12.0's sweep landed
//! (FINDINGS §31). The route had rewritten them as Japanese (32), Chinese (12)
//! and French (1); **37 of the 45 belong to speaker 26 — the user's own
//! microphone, a voice declared `["de", "en"]`.** The shape of the damage is
//! the same every time:
//!
//! ```text
//! "Mm-hmm."                    3.15 s  →  うん
//! "Okay, yeah."                3.89 s  →  ok看嗯
//! "Right."                     2.32 s  →  可以嗯
//! "Yeah."                      2.38 s  →  没
//! "Yeah, Gott was zu trinken." 2.32 s  →  よしじゃあ。
//! "Uh"                         2.00 s  →  Au revoir.
//! ```
//!
//! Every one of those cleared every guard the routes shipped with. The three
//! new guards ([`crate::asr_cjk::pre_route`]'s declaration and back-channel
//! tests, [`crate::asr_cjk::weak_output`]'s evidence tests) refuse **all 45**,
//! and this module is what does something about the rows that were written
//! before those guards existed.
//!
//! ## The rule, and why it is not "revert everything"
//!
//! A row is reverted when **the code as it stands today would not have written
//! it**, decided by running the shipped guards again — never by a list of ids,
//! and never by "it looks Japanese". That has three consequences worth stating,
//! because each of them is a promise:
//!
//! * A row the new guards still accept is **not touched**. If a later round
//!   loosens a guard, this pass reverts fewer rows without being edited.
//! * The decision reads the **current** database. A speaker who is declared
//!   `["de","en","ja"]` by the time this runs clears the declaration guard, and
//!   their rows are then judged on the evidence alone. That is deliberate and
//!   it is also a trap: measured on this install's rows, guards 2 and 3 without
//!   guard 1 keep **10 of the 45 rewrites and only 2 of them are right**
//!   (FINDINGS §31), so the order to run these two operations in is **repair
//!   first, declare afterwards**.
//! * It is itself reversible. Every revert writes a `segments.unroute`
//!   operation carrying the routed text and the language stamp it discarded.
//!
//! ## The audio, when it is still there
//!
//! With the identifier installed the pass adds a fourth test: run
//! [`crate::lid`] again, at the shipped `[asr].lid_windows`, and revert a row
//! whose reading no longer names a language anything routes. This can only ever
//! *add* reverts — a row that fails it was already going to be judged on its
//! text — so a run on a machine with no models is a strict subset of a run
//! with them, and neither can revert a row the guards accept.

use std::path::Path;

use anyhow::Result;
use tracing::info;

use crate::asr_cjk::{self, Cjk, Pre};
use crate::config::{AsrConfig, SAMPLE_RATE};
use crate::lang;
use crate::store::{RoutedRow, Store};

/// What this pass would do to one routed row, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The new guards would not have routed it. `why` is the guard that
    /// refused, in the words a report prints.
    Revert(&'static str),
    /// The new guards still accept it. Nothing is written.
    Keep,
    /// There is no `segments.redecode` operation for this row, so the words the
    /// route replaced are not recorded anywhere and there is nothing to restore.
    /// Reported and left alone: a repair that invented a prior transcript would
    /// be worse than the row it was fixing.
    NoPrior,
}

/// Would the route, as it stands today, have rewritten this row?
///
/// Pure over what the database holds, so the whole rule can be run across an
/// archive without a model in the process — which is what
/// `examples/lang_route_bench.rs` does over the live 45.
///
/// The tests, in the order the live path applies them:
///
/// 1. the row has a prior state to go back to at all;
/// 2. [`crate::asr_cjk::pre_route`] over the **prior** text and the speaker's
///    declaration — the two inputs the live route had, not the ones it
///    produced;
/// 3. the identifier floor, `[asr].lid_min_s`;
/// 4. [`crate::asr_cjk::weak_output`] over the text the route wrote, which is
///    the judge's new half applied to a decode that has already happened.
pub fn verdict(row: &RoutedRow, cfg: &AsrConfig) -> Verdict {
    if !row.has_prior {
        return Verdict::NoPrior;
    }
    let prior = row.prior_text.as_deref();
    if asr_cjk::pre_route(row.declared.as_ref(), prior, cfg) == Pre::Nothing {
        // Which of `pre_route`'s refusals it was. Re-derived by asking the same
        // function the same question **without** the declaration, rather than
        // by returning a reason from `pre_route`: the router itself has no use
        // for the distinction and only a report does, and a second enum on the
        // live path would be a second thing to keep in step.
        let text_alone = asr_cjk::pre_route(None, prior, cfg) == Pre::Nothing;
        let why = if !text_alone {
            "the voice declared its languages and none of them routes here"
        } else if prior.is_some_and(|t| {
            !t.trim().is_empty() && lang::content_word_count(t) < asr_cjk::MIN_CONTENT_WORDS
        }) {
            "the turn was a back-channel"
        } else {
            "the transcript already read as something"
        };
        return Verdict::Revert(why);
    }
    if row.duration_s < cfg.lid_min_s {
        return Verdict::Revert("the turn is under the identifier's floor");
    }
    match row.text.as_deref().unwrap_or("") {
        // A route that wrote nothing is not a route that succeeded.
        "" => Verdict::Revert("the route left the row empty"),
        text => match asr_cjk::weak_output(text, row.duration_s) {
            Some(why) => Verdict::Revert(why),
            None => Verdict::Keep,
        },
    }
}

/// The audio half, for a row the text half would have kept.
///
/// `None` when there is nothing to say — no identifier, no clip, or the reading
/// still names a routable language. `Some(why)` is a fourth reason to revert.
pub fn audio_verdict(
    cjk: &mut Cjk,
    data_dir: &Path,
    row: &RoutedRow,
    cfg: &AsrConfig,
) -> Option<&'static str> {
    if row.audio_path.is_empty() {
        return None;
    }
    let samples = crate::ingest::read_wav(&data_dir.join(&row.audio_path)).ok()?;
    if samples.is_empty() || (samples.len() as f32 / SAMPLE_RATE as f32) < cfg.lid_min_s {
        return None;
    }
    // `Some(None)` is a reading that cost a model pass and found nothing;
    // `None` is an identifier that never ran, and an identifier that never ran
    // is not evidence against a row.
    let reading = cjk.identify(&samples)?;
    let routes = asr_cjk::post_route(reading.as_ref(), cfg).is_some()
        || crate::polyglot::post_route(reading.as_ref(), cfg).is_some();
    (!routes).then_some("the identifier no longer names a language anything routes")
}

/// One row's line in the report.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: i64,
    pub duration_s: f32,
    pub lang: String,
    /// What the row says now, and what it would say again.
    pub now: String,
    pub before: String,
    pub verdict: Verdict,
    /// Set when the audio test is what decided it, so a report can say that the
    /// identifier was consulted rather than only the text.
    pub by_audio: bool,
}

/// What one run did, or would do.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub rows: Vec<Row>,
    pub reverted: usize,
    pub kept: usize,
    pub no_prior: usize,
}

impl Report {
    pub fn looked_at(&self) -> usize {
        self.rows.len()
    }
}

/// Walk every row the route settled and, with `apply`, put back the ones the
/// guards refuse.
///
/// Not batched and not bounded, unlike the sweep, and that is a property of the
/// work rather than an omission: the list is every row with
/// `lang_via = 'lid'`, which is 45 on the install this was written for and
/// cannot grow faster than the live route routes. The store is locked per row.
pub fn run(
    store: &Store,
    cjk: Option<&mut Cjk>,
    data_dir: &Path,
    cfg: &AsrConfig,
    apply: bool,
    now_utc_ns: i64,
) -> Result<Report> {
    let mut report = Report::default();
    let mut cjk = cjk;
    for row in store.segments_routed_by_lid()? {
        let mut by_audio = false;
        let mut v = verdict(&row, cfg);
        if v == Verdict::Keep
            && let Some(cjk) = cjk.as_deref_mut()
            && let Some(why) = audio_verdict(cjk, data_dir, &row, cfg)
        {
            v = Verdict::Revert(why);
            by_audio = true;
        }
        match &v {
            Verdict::Revert(why) => {
                if apply && store.unroute_segment(&row, now_utc_ns)? {
                    info!(
                        segment_id = row.id,
                        why,
                        restored = row.prior_text.as_deref().unwrap_or(""),
                        "took back a turn the audio route should not have rewritten"
                    );
                }
                report.reverted += 1;
            }
            Verdict::Keep => report.kept += 1,
            Verdict::NoPrior => report.no_prior += 1,
        }
        report.rows.push(Row {
            id: row.id,
            duration_s: row.duration_s,
            lang: row.lang.clone().unwrap_or_default(),
            now: row.text.clone().unwrap_or_default(),
            before: row.prior_text.clone().unwrap_or_default(),
            verdict: v,
            by_audio,
        });
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn routed(
        duration_s: f32,
        prior: Option<&str>,
        now: &str,
        declared: Option<&[&str]>,
    ) -> RoutedRow {
        RoutedRow {
            id: 1,
            duration_s,
            audio_path: String::new(),
            text: Some(now.to_string()),
            lang: Some("ja".into()),
            declared: declared.map(tags),
            prior_text: prior.map(str::to_string),
            prior_model: Some("sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8@1".into()),
            prior_via: Some("live".into()),
            has_prior: true,
        }
    }

    #[test]
    fn every_one_of_the_forty_five_live_rows_is_taken_back() {
        // The table from FINDINGS §31, as data: `(seconds, prior text, what the
        // route wrote, the speaker's declaration)`. Speaker 26 is the user's own
        // microphone and is declared de+en; the rows with `None` are voices with
        // no declaration at all, which is the harder half.
        let cfg = AsrConfig::default();
        let deen: Option<&[&str]> = Some(&["de", "en"]);
        let de: Option<&[&str]> = Some(&["de"]);
        /// One live row: `(seconds, what the live decoder said, what the route
        /// wrote, the speaker's declaration)`.
        type Damaged<'a> = (f32, Option<&'a str>, &'a str, Option<&'a [&'a str]>);
        let rows: &[Damaged<'_>] = &[
            (
                2.16,
                Some("Stop us. Psalm does. He"),
                "ふタを伸ばすプタを伸ばす。",
                deen,
            ),
            (
                1.84,
                Some("Um Wata Wata Wata Wata."),
                "あまたタ待タ待タ待タ。",
                deen,
            ),
            (1.62, Some("Yeah."), "どるな", None),
            (1.71, Some("Um"), "ガンとあたって", deen),
            (
                2.80,
                Some("Yeah good, yeah, good."),
                "やユージャいくと。",
                deen,
            ),
            (1.74, None, "フフフフフフフ", deen),
            (2.35, Some("Ah."), "なるほどあっ", deen),
            (2.70, None, "ババに、バカに", deen),
            (2.26, Some("Oh"), "あっ", deen),
            (1.78, Some("Mm-hmm."), "うん", deen),
            (1.71, Some("Mm."), "うん", deen),
            (5.26, Some("Mm-hmm."), "うん", deen),
            (1.52, Some("Mm s."), "うんそれ?", deen),
            (1.62, None, "宇中嗯嗯", deen),
            (3.89, Some("Okay, yeah."), "ok看嗯", deen),
            (3.79, None, "すいません", deen),
            (1.62, None, "こうすな", deen),
            (3.15, Some("Mm-hmm."), "うん", deen),
            (2.32, Some("Right."), "可以嗯", deen),
            (1.55, Some("Mm, mm-hmm."), "嗯嗯嗯来", deen),
            (1.55, None, "聞いてみますかねちょっと", deen),
            (2.38, Some("Yeah."), "没", deen),
            (2.54, Some("Uh"), "いただきります", deen),
            (6.67, Some("Uh special."), "そうしました", deen),
            (2.00, Some("Uh"), "Au revoir.", deen),
            (4.43, None, "そそ在ので you", deen),
            (
                2.32,
                Some("Yeah, Gott was zu trinken."),
                "よしじゃあ。",
                deen,
            ),
            (2.13, None, "寝ます弾けてます", deen),
            (2.26, Some("Uh"), "啊头分不", deen),
            (1.84, Some("H"), "はいやや", deen),
            (1.81, Some("Uh"), "うんね", de),
            (1.81, Some("Uh."), "啊 嗯", de),
            (1.58, Some("Uh"), "ち次", deen),
            (1.74, None, "そ be丈夫", deen),
            (2.83, Some("Oh"), "うん", deen),
            (3.41, Some("Hopp! Oh danke!"), "오빠どなか", deen),
            (2.13, Some("M G X. Minus"), "mkエクスプションビナス", deen),
            (1.74, Some("Mm."), "うんまあ", deen),
            (2.38, Some("Locky hunt"), "ロキオティ", deen),
            (
                3.86,
                Some("Oh my ooh ooh oh okay."),
                "おーまおあおあけ",
                None,
            ),
            (1.90, Some("Katastro."), "だたそ", deen),
            (
                4.94,
                None,
                "没没没不是说说说说说说说说说说说没没没没没",
                None,
            ),
            (1.68, Some("Uh yeah."), "嗯嗯", None),
            (1.84, Some("Mm."), "ねがね。", None),
            (1.78, Some("Uh Alter."), "哎呀", None),
        ];
        assert_eq!(rows.len(), 45, "the archive damage, in full");
        for (duration_s, prior, now, declared) in rows {
            let row = routed(*duration_s, *prior, now, *declared);
            assert!(
                matches!(verdict(&row, &cfg), Verdict::Revert(_)),
                "{prior:?} -> {now:?} would still be routed"
            );
        }
    }

    #[test]
    fn a_row_the_new_guards_still_accept_is_left_exactly_where_it_is() {
        // The rule is "would the code as it stands write this", not "was it
        // written by the old code": a correct Japanese re-decode of a real
        // Japanese turn, on a voice that declared Japanese, is untouched.
        let cfg = AsrConfig::default();
        let ja: Option<&[&str]> = Some(&["de", "en", "ja"]);
        let good = routed(
            3.0,
            Some("Sima Sen Okenki Deska."),
            "すみません、お元気ですか",
            ja,
        );
        assert_eq!(verdict(&good, &cfg), Verdict::Keep);
        // …and with no declaration at all, which is most of a lobby.
        let anyone = routed(
            3.0,
            Some("Sima Sen Okenki Deska."),
            "すみません、お元気ですか",
            None,
        );
        assert_eq!(verdict(&anyone, &cfg), Verdict::Keep);

        // The same row on the de/en voice is reverted, and THAT is the trap
        // this pass has to be run before the declaration is widened, not after.
        let deen: Option<&[&str]> = Some(&["de", "en"]);
        let same = routed(
            3.0,
            Some("Sima Sen Okenki Deska."),
            "すみません、お元気ですか",
            deen,
        );
        assert!(matches!(verdict(&same, &cfg), Verdict::Revert(_)));
    }

    #[test]
    fn a_row_with_no_recorded_prior_is_reported_and_never_guessed_at() {
        let cfg = AsrConfig::default();
        let mut row = routed(2.0, Some("Mm-hmm."), "うん", None);
        row.has_prior = false;
        row.prior_text = None;
        assert_eq!(verdict(&row, &cfg), Verdict::NoPrior);
    }

    #[test]
    fn the_revert_restores_the_words_clears_the_stamp_and_can_itself_be_undone() {
        let store = Store::open_in_memory().expect("a store");
        let source = store
            .upsert_source("VRChat.exe", "VRChat.exe", 1)
            .expect("a source");
        let session = store.begin_session(source, 0).expect("a session");
        let id = store
            .insert_segment(session, 0, 3_150_000_000, "segments/000001/a.wav", 0)
            .expect("a segment");
        // The live decode, then the route over the top of it — the real order,
        // through the real calls, so the `segments.redecode` operation this
        // pass reads is the one the route actually writes.
        store
            .set_segment_text_via(id, "Mm-hmm.", "v3@1", crate::store::text_via::LIVE, 1)
            .expect("live text");
        store
            .set_segment_text_via(id, "うん", "ja@1", crate::store::text_via::LID, 2)
            .expect("the route");
        store
            .set_segment_language(id, "ja", asr_cjk::LANG_VIA_LID)
            .expect("the stamp");

        let rows = store.segments_routed_by_lid().expect("the work list");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].prior_text.as_deref(), Some("Mm-hmm."));
        assert_eq!(rows[0].prior_via.as_deref(), Some("live"));
        assert!(matches!(
            verdict(&rows[0], &AsrConfig::default()),
            Verdict::Revert(_)
        ));

        let report = run(
            &store,
            None,
            Path::new("/nonexistent"),
            &AsrConfig::default(),
            true,
            3,
        )
        .expect("a run");
        assert_eq!((report.reverted, report.kept), (1, 0));

        // The words are back, with their own model and route…
        let back = store.segment_fields(id).expect("the row");
        assert_eq!(back["text"].as_deref(), Some("Mm-hmm."));
        assert_eq!(back["asr_model_id"].as_deref(), Some("v3@1"));
        // …the language stamp is gone entirely, which puts the row back on the
        // sweep's list rather than marking it answered…
        assert_eq!(back["lang"], None);
        assert_eq!(back["lang_via"], None);
        assert!(store.segments_for_lang_sweep(1.0, 10).unwrap().len() <= 1);
        // …the row is no longer on this pass's list…
        assert!(store.segments_routed_by_lid().unwrap().is_empty());
        // …and what was thrown away is written down, so the repair is undoable
        // in its turn.
        let ops = store.operations(10).expect("the log");
        let undo = ops
            .iter()
            .find(|o| o.op == "segments.unroute")
            .expect("a reversible record");
        assert!(undo.prior_state.contains("うん"), "{}", undo.prior_state);
        assert!(undo.prior_state.contains("\"lang\":\"ja\""));

        // Running again is a no-op rather than a second revert.
        let again = run(
            &store,
            None,
            Path::new("/nonexistent"),
            &AsrConfig::default(),
            true,
            4,
        )
        .expect("a second run");
        assert_eq!(again.looked_at(), 0);
    }

    #[test]
    fn a_preview_writes_nothing() {
        let store = Store::open_in_memory().expect("a store");
        let source = store
            .upsert_source("VRChat.exe", "VRChat.exe", 1)
            .expect("a source");
        let session = store.begin_session(source, 0).expect("a session");
        let id = store
            .insert_segment(session, 0, 2_000_000_000, "a.wav", 0)
            .expect("a segment");
        store
            .set_segment_text_via(id, "Yeah.", "v3@1", crate::store::text_via::LIVE, 1)
            .expect("live text");
        store
            .set_segment_text_via(id, "没", "sv@1", crate::store::text_via::LID, 2)
            .expect("the route");
        store
            .set_segment_language(id, "zh", asr_cjk::LANG_VIA_LID)
            .expect("the stamp");
        let report = run(
            &store,
            None,
            Path::new("/nonexistent"),
            &AsrConfig::default(),
            false,
            3,
        )
        .expect("a preview");
        assert_eq!(report.reverted, 1, "it says what it would do");
        let still = store.segment_fields(id).expect("the row");
        assert_eq!(still["text"].as_deref(), Some("没"), "and does none of it");
        assert_eq!(still["lang"].as_deref(), Some("zh"));
    }
}
