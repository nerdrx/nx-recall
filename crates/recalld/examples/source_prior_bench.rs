//! Does knowing where the audio came from make the voicebank righter? (0.11.0)
//!
//! Run against a **copy** of a real database — never the live one:
//!
//! ```text
//! sqlite3 "file:$HOME/.local/share/nx-recall/recall.db?mode=ro" \
//!         ".backup /path/to/scratch/recall.db"
//! cargo run -p recalld --example source_prior_bench -- /path/to/scratch/recall.db
//! ```
//!
//! # What it does
//!
//! Two arms, because the two questions have different evidence behind them.
//!
//! **Arm 1 — the truth arm.** Every Discord-sourced turn Discord itself
//! labelled `single` with a linked account is a turn where the right answer is
//! known. The bench re-ranks the stored embedding against the voicebank and
//! replays [`recalld::identity::decide`] twice: once as 0.10.0 ran it, once
//! with the prior in front. Precision, recall and the wrong-label count, both
//! ways.
//!
//! Two rules of hygiene, both load-bearing:
//!
//! * A prototype that came from the very segment being judged is dropped from
//!   the bank for that judgement. Otherwise the row scores 1.0 against itself
//!   and the whole measurement is a memory test.
//! * The prior's standings are computed **chronologically** — only turns that
//!   came before the row being judged count as history. Using today's totals
//!   would let a row's own label vouch for it.
//!
//! And one caveat the bench prints rather than hides: the hard presence rule
//! reads `truth_speaking`, which is where this arm's ground truth also comes
//! from. Scoring it here would be circular, so it is reported separately and
//! marked, and the headline number is the **soft rules only**.
//!
//! **Arm 2 — the VRChat arm.** There is no ground truth in VRChat, so the
//! question is the one the user actually asked: how many labels point at voices
//! with no prior history on that source, and what would the prior have done
//! with them? Counted by the same chronological replay
//! (`recalld identity audit` uses it too), and then re-decided where the
//! embedding is still on disk.
//!
//! Numbers live in `spike/FINDINGS.md` §17.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};

use recalld::config::IdentityConfig;
use recalld::embed::Embedding;
use recalld::identity::{self, Decision};
use recalld::identity_prior::{self, SourceFamily, Sources, Standing};

/// Turns shorter than this are excluded, matching `recalld::truth`: a
/// sub-second turn is a grunt the voicebank refuses anyway, and scoring one
/// measures the floor rather than the model.
const MIN_DURATION_S: f64 = 1.0;

struct Row {
    id: i64,
    speaker_id: Option<i64>,
    source_id: i64,
    t_start_ns: i64,
    t_end_ns: i64,
    overlap_frac: f32,
    words: usize,
    truth_user: Option<String>,
    truth_verdict: Option<String>,
}

struct Proto {
    speaker_id: i64,
    vector: Embedding,
    source_segment_id: Option<i64>,
}

/// One row the prior moved: `(segment, source, before, after, truth)`.
type Changed = (i64, String, Option<i64>, Option<i64>, i64);

#[derive(Default, Debug, Clone, Copy)]
struct Score {
    n: i64,
    correct: i64,
    wrong: i64,
    declined: i64,
}

impl Score {
    fn add(&mut self, labelled: Option<i64>, truth: i64) {
        self.n += 1;
        match labelled {
            Some(id) if id == truth => self.correct += 1,
            Some(_) => self.wrong += 1,
            None => self.declined += 1,
        }
    }
    fn precision(&self) -> f64 {
        let d = self.correct + self.wrong;
        if d == 0 {
            f64::NAN
        } else {
            self.correct as f64 / d as f64
        }
    }
    fn recall(&self) -> f64 {
        if self.n == 0 {
            f64::NAN
        } else {
            self.correct as f64 / self.n as f64
        }
    }
}

fn pct(v: f64) -> String {
    if v.is_nan() {
        "—".into()
    } else {
        format!("{:.1}%", v * 100.0)
    }
}

fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("usage: source_prior_bench <copy-of-recall.db>")?;
    let mut cfg = IdentityConfig {
        source_prior: true,
        ..IdentityConfig::default()
    };
    let mut args = std::env::args().skip(2);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--margin" => cfg.foreign_source_margin = args.next().unwrap().parse()?,
            "--after" => cfg.foreign_after_segments = args.next().unwrap().parse()?,
            other => anyhow::bail!("unknown flag {other}"),
        }
    }
    // Read-only, and said out loud: this bench must never be the thing that
    // migrates or writes to a database somebody cares about.
    let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {path} read-only"))?;

    let discord = vec!["discord".to_string(), "vesktop".to_string()];
    let vrchat = vec!["vrchat".to_string()];
    let sources = Sources {
        discord: &discord,
        vrchat: &vrchat,
    };

    // ---- what the database holds -----------------------------------------

    let source_meta: HashMap<i64, (String, String, SourceFamily)> = db
        .prepare("SELECT id, match_key, display_name FROM sources")?
        .query_map([], |r| {
            let id: i64 = r.get(0)?;
            let key: String = r.get(1)?;
            let name: String = r.get(2)?;
            let fam = identity_prior::family_of(&key, &name, sources.discord, sources.vrchat);
            Ok((id, (key, name, fam)))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let you: Option<i64> = db
        .query_row(
            "SELECT value FROM settings WHERE key = 'you_speaker_id'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse().ok());

    let linked: HashMap<String, i64> = db
        .prepare("SELECT user_id, speaker_id FROM discord_users WHERE speaker_id IS NOT NULL")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;

    let protos: Vec<Proto> = db
        .prepare(
            "SELECT p.speaker_id, p.vector, p.embed_model_id, p.source_segment_id
             FROM speaker_prototypes p
             JOIN speakers s ON s.id = p.speaker_id
             WHERE s.merged_into IS NULL",
        )?
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(speaker_id, blob, model, src)| {
            Ok(Proto {
                speaker_id,
                vector: Embedding::from_blob(model, &blob)?,
                source_segment_id: src,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let rows: Vec<Row> = db
        .prepare(
            "SELECT g.id, g.speaker_id, ss.source_id, g.t_start_ns, g.t_end_ns,
                    COALESCE(g.overlap_frac, 0.0), COALESCE(g.text, ''),
                    g.truth_user_id, g.truth_verdict
             FROM segments g
             JOIN sessions ss ON ss.id = g.session_id
             WHERE g.deleted_at IS NULL
             ORDER BY g.t_start_ns ASC, g.id ASC",
        )?
        .query_map([], |r| {
            let text: String = r.get(6)?;
            Ok(Row {
                id: r.get(0)?,
                speaker_id: r.get(1)?,
                source_id: r.get(2)?,
                t_start_ns: r.get(3)?,
                t_end_ns: r.get(4)?,
                overlap_frac: r.get::<_, f64>(5)? as f32,
                words: text.split_whitespace().count(),
                truth_user: r.get(7)?,
                truth_verdict: r.get(8)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;

    // ---- the chronological standings -------------------------------------
    //
    // Walked once, in time order: at the moment each row is judged, `seen` and
    // `total` hold only what came before it.
    let mut seen: HashMap<(i64, i64), i64> = HashMap::new();
    let mut total: HashMap<i64, i64> = HashMap::new();

    // ---- arm 1: the truth arm --------------------------------------------

    let mut base = Score::default();
    let mut soft = Score::default();
    let mut hard = Score::default();
    let mut per_source: HashMap<String, (Score, Score)> = HashMap::new();
    let mut changed: Vec<Changed> = Vec::new();
    // Candidates the two arms took off the list. Zero here is the whole story
    // when the labels do not move: the rule never fired, rather than firing and
    // making no difference.
    let mut dropped_soft = 0i64;
    let mut dropped_hard = 0i64;
    // The same two arms with one class of row removed: verdicts naming the
    // user's OWN account. A `single` verdict says "only this person was
    // speaking on Discord"; on audio captured from the user's own Discord
    // client that person is the one voice the stream cannot contain, because a
    // client does not play your microphone back to you. Those rows are not
    // ground truth about this audio, and scoring them measures the bridge's
    // blind spot rather than the voicebank.
    let mut base_clean = Score::default();
    let mut soft_clean = Score::default();

    // ---- arm 2: the VRChat arm -------------------------------------------

    let mut vr_labels = 0i64;
    let mut vr_foreign = 0i64;
    let mut vr_foreign_redecided: Vec<(i64, i64, Option<i64>)> = Vec::new();

    for row in &rows {
        let (src_key, _, family) = source_meta.get(&row.source_id).cloned().unwrap_or_else(|| {
            (
                format!("source {}", row.source_id),
                String::new(),
                SourceFamily::Other,
            )
        });
        let duration_s = (row.t_end_ns - row.t_start_ns) as f64 / 1e9;

        // -- the standings this row would have been judged against ----------
        let standing_for =
            |ids: &[i64], seen: &HashMap<(i64, i64), i64>, total: &HashMap<i64, i64>| {
                ids.iter()
                    .map(|&id| Standing {
                        speaker_id: id,
                        on_source: *seen.get(&(id, row.source_id)).unwrap_or(&0),
                        total: *total.get(&id).unwrap_or(&0),
                        is_you: you == Some(id),
                        discord_absent: false,
                        roster_absent: false,
                    })
                    .collect::<Vec<_>>()
            };

        // -- arm 2 ----------------------------------------------------------
        if family == SourceFamily::VrChat
            && let Some(sp) = row.speaker_id
        {
            vr_labels += 1;
            let st = standing_for(&[sp], &seen, &total);
            if identity_prior::is_foreign(&cfg, &st[0]) {
                vr_foreign += 1;
                if let Some(emb) = embedding_of(&db, row.id)? {
                    let bank = bank_for(&protos, row.id, &emb.model_id);
                    let ranked = identity::rank(&emb, &bank)?;
                    let ids: Vec<i64> = ranked.iter().map(|c| c.speaker_id).collect();
                    let kept =
                        identity_prior::apply(&cfg, &standing_for(&ids, &seen, &total), &ranked);
                    let after = label_of(&identity::decide(
                        &cfg,
                        row.overlap_frac,
                        duration_s as f32,
                        row.words,
                        &kept.kept,
                    ));
                    vr_foreign_redecided.push((row.id, sp, after));
                }
            }
        }

        // -- arm 1 ----------------------------------------------------------
        let truth_speaker = row
            .truth_verdict
            .as_deref()
            .filter(|v| *v == "single")
            .and(row.truth_user.as_deref())
            .and_then(|u| linked.get(u).copied());
        if let Some(truth) = truth_speaker
            && family == SourceFamily::Discord
            && duration_s >= MIN_DURATION_S
            && let Some(emb) = embedding_of(&db, row.id)?
        {
            let bank = bank_for(&protos, row.id, &emb.model_id);
            let ranked = identity::rank(&emb, &bank)?;
            let ids: Vec<i64> = ranked.iter().map(|c| c.speaker_id).collect();
            let base_label = label_of(&identity::decide(
                &cfg,
                row.overlap_frac,
                duration_s as f32,
                row.words,
                &ranked,
            ));

            let soft_st = standing_for(&ids, &seen, &total);
            let soft_kept = identity_prior::apply(&cfg, &soft_st, &ranked);
            let soft_label = label_of(&identity::decide(
                &cfg,
                row.overlap_frac,
                duration_s as f32,
                row.words,
                &soft_kept.kept,
            ));

            // The circular arm, reported and marked: Discord's speaking events
            // are also where `truth` came from.
            let spoke = discord_speaking_between(
                &db,
                row.t_start_ns - identity_prior::PRESENCE_REACH_NS,
                row.t_end_ns + identity_prior::PRESENCE_REACH_NS,
            )?;
            let hard_st = soft_st
                .iter()
                .map(|s| Standing {
                    discord_absent: !spoke.is_empty() && !s.is_you && {
                        let mine = linked
                            .iter()
                            .filter(|(_, sp)| **sp == s.speaker_id)
                            .map(|(u, _)| u.clone())
                            .collect::<Vec<_>>();
                        !mine.is_empty() && !mine.iter().any(|u| spoke.contains(u))
                    },
                    ..s.clone()
                })
                .collect::<Vec<_>>();
            let hard_kept = identity_prior::apply(&cfg, &hard_st, &ranked);
            let hard_label = label_of(&identity::decide(
                &cfg,
                row.overlap_frac,
                duration_s as f32,
                row.words,
                &hard_kept.kept,
            ));

            dropped_soft += soft_kept.dropped.len() as i64;
            dropped_hard += hard_kept.dropped.len() as i64;
            base.add(base_label, truth);
            soft.add(soft_label, truth);
            if Some(truth) != you {
                base_clean.add(base_label, truth);
                soft_clean.add(soft_label, truth);
            }
            hard.add(hard_label, truth);
            let e = per_source.entry(src_key.clone()).or_default();
            e.0.add(base_label, truth);
            e.1.add(soft_label, truth);
            if base_label != soft_label {
                changed.push((row.id, src_key.clone(), base_label, soft_label, truth));
            }
        }

        // -- advance history -------------------------------------------------
        if let Some(sp) = row.speaker_id {
            *seen.entry((sp, row.source_id)).or_insert(0) += 1;
            *total.entry(sp).or_insert(0) += 1;
        }
    }

    // ---- report ------------------------------------------------------------

    println!("source-aware identity prior — bench");
    println!("  database          {path}");
    println!(
        "  operating point   label {:.2}, foreign {:.2} (+{:.2}), foreign after {} turn(s), margin {:.2}",
        cfg.label_threshold,
        cfg.label_threshold + cfg.foreign_source_margin,
        cfg.foreign_source_margin,
        cfg.foreign_after_segments,
        cfg.enroll_margin,
    );
    println!("  live segments     {}", rows.len());

    println!("\n=== arm 1: Discord turns with Discord's own ground truth ===");
    if base.n == 0 {
        println!("  no scorable rows: nothing has both a `single` verdict, a linked account,");
        println!("  a stored embedding and a second of audio.");
    } else {
        println!(
            "  {:<24}{:>6}{:>10}{:>8}{:>10}{:>12}{:>10}",
            "arm", "n", "correct", "wrong", "declined", "precision", "recall"
        );
        for (name, s) in [
            ("0.10.0 (no prior)", base),
            ("prior, soft rules", soft),
            ("prior + hard (CIRCULAR)", hard),
            ("no prior, own excluded", base_clean),
            ("prior soft, own excluded", soft_clean),
        ] {
            println!(
                "  {:<24}{:>6}{:>10}{:>8}{:>10}{:>12}{:>10}",
                name,
                s.n,
                s.correct,
                s.wrong,
                s.declined,
                pct(s.precision()),
                pct(s.recall())
            );
        }
        println!("\n  by source");
        let mut keys: Vec<_> = per_source.keys().cloned().collect();
        keys.sort();
        for k in keys {
            let (b, s) = per_source[&k];
            println!(
                "    {:<14}n={:<5} before: {} ok / {} wrong    after: {} ok / {} wrong",
                k, b.n, b.correct, b.wrong, s.correct, s.wrong
            );
        }
        println!("\n  candidates removed   soft {dropped_soft}   soft+hard {dropped_hard}");
        println!("  rows the prior changed: {}", changed.len());
        for (id, src, b, a, t) in changed.iter().take(20) {
            println!(
                "    segment {id:>7} on {src:<12} {:?} -> {:?}   (truth {t})",
                b, a
            );
        }
    }

    println!("\n=== arm 2: VRChat turns, where there is no ground truth ===");
    println!("  labelled VRChat turns                        {vr_labels}");
    println!("  ... whose voice had NO prior VRChat history  {vr_foreign}");
    println!(
        "  ... still embeddable, and re-decided             {}",
        vr_foreign_redecided.len()
    );
    for (id, was, now) in vr_foreign_redecided.iter().take(20) {
        println!("    segment {id:>7}  {was} -> {now:?}");
    }
    Ok(())
}

/// The bank for one judgement: every prototype except the ones this very
/// segment produced. Without this a row scores 1.0 against itself.
fn bank_for(protos: &[Proto], segment_id: i64, model_id: &str) -> Vec<(i64, Embedding)> {
    protos
        .iter()
        .filter(|p| p.source_segment_id != Some(segment_id) && p.vector.model_id == model_id)
        .map(|p| (p.speaker_id, p.vector.clone()))
        .collect()
}

fn label_of(d: &Decision) -> Option<i64> {
    match d {
        Decision::Matched { speaker_id, .. } | Decision::Pinned { speaker_id } => Some(*speaker_id),
        // A mint is not a label of any existing voice, and against ground truth
        // it is a decline rather than a wrong answer: the ladder said "nobody
        // in the bank", which is a different claim from "this person".
        _ => None,
    }
}

fn embedding_of(db: &Connection, segment_id: i64) -> Result<Option<Embedding>> {
    let row: Option<(Vec<u8>, String)> = db
        .query_row(
            "SELECT vector, embed_model_id FROM embeddings WHERE segment_id = ?1",
            params![segment_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    match row {
        Some((blob, model)) => Ok(Some(Embedding::from_blob(model, &blob)?)),
        None => Ok(None),
    }
}

fn discord_speaking_between(db: &Connection, from_ns: i64, to_ns: i64) -> Result<HashSet<String>> {
    let mut stmt = db.prepare(
        "SELECT DISTINCT user_id FROM truth_speaking
         WHERE t_start_ns < ?2 AND COALESCE(t_end_ns, ?2) > ?1",
    )?;
    Ok(stmt
        .query_map(params![from_ns, to_ns], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<HashSet<_>>>()?)
}
