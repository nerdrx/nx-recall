//! The audio-language route against the rows it actually got wrong (0.11.10).
//!
//! Run against a **copy** of a real database — never the live one:
//!
//! ```text
//! sqlite3 "file:$HOME/.local/share/nx-recall/recall.db?mode=ro" \
//!         ".backup /path/to/scratch/recall.db"
//! cargo run -p recalld --release --example lang_route_bench -- \
//!     /path/to/scratch/recall.db $HOME/.local/share/nx-recall
//! ```
//!
//! # The question
//!
//! §30 measured the route over the archive it had *not* seen and shipped the
//! sweep with its rewriting half off. What it did not do is look at the rows the
//! **live** route had already rewritten. There are 45 of them on this install,
//! and 37 belong to one voice: the user's own microphone, declared
//! `["de","en"]`. So this bench asks three things the earlier one could not:
//!
//! 1. **Which guard catches what.** The old route, the new declaration guard,
//!    the new back-channel guard and the new evidence guards, each over the same
//!    45 rows plus a matched set of German/English back-channels from the same
//!    voice — the negatives.
//! 2. **Is `lid_windows = 3` worth making the live default?** Measured here with
//!    the declaration guard deliberately **off**, because with it on this
//!    install has no rows left for the identifier to be wrong about and the
//!    question would answer itself. Four arms: one window and three, at floors
//!    of 1.0 s and 1.5 s.
//! 3. **What the guards cost.** The two rows on this install that look like
//!    genuine Japanese are the whole of the positive set, and an arm that loses
//!    them is buying its precision with the feature.
//!
//! Every decision below is made by shipped code — `asr_cjk::pre_route`,
//! `Lid::identify`, both `post_route`s, `asr_cjk::judge`, `unroute::verdict` —
//! for §28's reason: a second copy of a rule in a bench is a rule that drifts
//! from the shipped one the day after it is written.
//!
//! Numbers live in `spike/FINDINGS.md` §31.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

use recalld::asr_cjk::{self, Pre};
use recalld::config::{AsrConfig, LangConfig, ModelsConfig, SAMPLE_RATE};
use recalld::lid::Lid;
use recalld::models::ModelSet;
use recalld::polyglot;

/// One row of either set, in the shape the route saw it.
struct Row {
    id: i64,
    duration_s: f32,
    audio_path: String,
    /// What the **live** decoder produced — for the routed rows that is the
    /// prior text out of `segments.redecode`, not what the row says now.
    text: Option<String>,
    declared: Option<Vec<String>>,
    /// The routed rows only: what the route wrote.
    wrote: Option<String>,
    /// …and the tag it stamped, kept so a reader of the raw set can tell the
    /// Japanese arm's rows from the Chinese one's.
    #[allow(dead_code)]
    routed_as: Option<String>,
    /// Hand-checked. `Some(true)` is a rewrite that looks right.
    genuine: Option<bool>,
}

/// The two rows of the 45 that a reader of Japanese would keep. Both are turns
/// the live decoder gave up on entirely, which is exactly the shape the route
/// was built for — and the reason the back-channel guard lets an empty
/// transcript through.
const GENUINE: &[i64] = &[14983, 15175];

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let db = args.next().unwrap_or_else(|| {
        eprintln!("usage: lang_route_bench <db> <data-dir> [--negatives N]");
        std::process::exit(2);
    });
    let data_dir =
        std::path::PathBuf::from(args.next().unwrap_or_else(|| {
            format!("{}/.local/share/nx-recall", std::env::var("HOME").unwrap())
        }));
    let mut negatives = 200usize;
    let rest: Vec<String> = args.collect();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--negatives" => negatives = it.next().and_then(|n| n.parse().ok()).unwrap_or(200),
            other => eprintln!("ignoring {other}"),
        }
    }

    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {db}"))?;

    // ---- the two sets ------------------------------------------------------
    let routed = read_routed(&conn)?;
    let control = read_control(&conn, negatives)?;
    println!(
        "{} rows the live route rewrote; {} German/English back-channels from the \
         same voice as a control.\n",
        routed.len(),
        control.len()
    );

    // ---- stage one: which guard catches what -------------------------------
    //
    // Pure, no models. Each column is the shipped rule run alone, so the table
    // says what each guard is FOR rather than only what they do together.
    let live = AsrConfig::default();
    let no_declaration = |row: &Row| asr_cjk::pre_route(None, row.text.as_deref(), &live);
    // The old gate: everything `pre_route` did before 0.11.10 — a SOLE
    // declaration, then the two text readings. Reconstructed by asking the
    // shipped function with the two new tests disarmed: no declaration handed
    // in, and a text padded past the content-word floor so it cannot bite.
    let old_gate = |row: &Row| {
        let t = row.text.as_deref().unwrap_or("");
        let padded = format!("{t} qwertzuiop asdfghjkl");
        asr_cjk::pre_route(None, Some(padded.as_str()), &live) != Pre::Nothing
    };
    // Each new guard alone, against that gate. The declaration one is measured
    // on padded text for the same reason: a column that says "declaration"
    // must not be quietly counting the back-channel rule as well.
    let declaration_alone = |row: &Row| {
        let t = row.text.as_deref().unwrap_or("");
        let padded = format!("{t} qwertzuiop asdfghjkl");
        asr_cjk::pre_route(row.declared.as_ref(), Some(padded.as_str()), &live) != Pre::Nothing
    };
    println!("## Which guard refuses which rows\n");
    println!(
        "| set | n | the old gate asks | declaration alone | back-channel alone | both | \
         and the evidence guards |"
    );
    println!(
        "|-----|--:|------------------:|------------------:|-------------------:|-----:|\
         ------------------------:|"
    );
    for (name, set) in [("the 45 rewrites", &routed), ("the control", &control)] {
        let n = set.len();
        let old = set.iter().filter(|r| old_gate(r)).count();
        let declared = set.iter().filter(|r| declaration_alone(r)).count();
        let back_channel = set
            .iter()
            .filter(|r| no_declaration(r) != Pre::Nothing)
            .count();
        let both = set
            .iter()
            .filter(|r| {
                asr_cjk::pre_route(r.declared.as_ref(), r.text.as_deref(), &live) != Pre::Nothing
            })
            .count();
        // …and the evidence guards on top, over the text the route wrote. The
        // control never reached a decoder, so it has no output to judge and
        // says so rather than printing a zero it did not earn.
        let evidence = set
            .iter()
            .filter(|r| {
                asr_cjk::pre_route(r.declared.as_ref(), r.text.as_deref(), &live) != Pre::Nothing
                    && r.wrote
                        .as_deref()
                        .is_some_and(|w| asr_cjk::weak_output(w, r.duration_s).is_none())
            })
            .count();
        println!(
            "| {name} | {n} | {old} | {declared} | {back_channel} | {both} | {} |",
            if set.iter().any(|r| r.wrote.is_some()) {
                evidence.to_string()
            } else {
                "n/a — nothing was written".to_string()
            }
        );
    }
    println!("\n(Every column is rows STILL ASKED ABOUT: lower is stricter.)\n");

    // ---- stage two: what the evidence guards do on their own ---------------
    //
    // The number that decides the operating advice, because the declaration
    // guard is one `recalld languages` away from being switched off by the
    // person it protects.
    let mut kept_without_declaration = Vec::new();
    for r in &routed {
        if no_declaration(r) == Pre::Nothing {
            continue;
        }
        let Some(wrote) = r.wrote.as_deref() else {
            continue;
        };
        if asr_cjk::weak_output(wrote, r.duration_s).is_none() {
            kept_without_declaration.push(r);
        }
    }
    println!("## If the voice declares `ja`, what do guards 2 and 3 keep on their own\n");
    println!("| id | seconds | live text | the route wrote | right? |");
    println!("|----|--------:|-----------|-----------------|--------|");
    for r in &kept_without_declaration {
        println!(
            "| {} | {:.2} | `{}` | `{}` | {} |",
            r.id,
            r.duration_s,
            r.text.as_deref().unwrap_or("*(empty)*"),
            r.wrote.as_deref().unwrap_or(""),
            match r.genuine {
                Some(true) => "yes",
                _ => "no",
            }
        );
    }
    let right = kept_without_declaration
        .iter()
        .filter(|r| r.genuine == Some(true))
        .count();
    println!(
        "\n**{} of {} kept** without the declaration guard, {right} of them right.\n",
        kept_without_declaration.len(),
        routed.len()
    );

    // ---- stage three: the arms --------------------------------------------
    let models = ModelSet::resolve_at(data_dir.join("models"), &ModelsConfig::default());
    anyhow::ensure!(models.lid().present(), "the identifier is not installed");
    let lang_cfg = LangConfig::default();

    println!("## `lid_windows` and the floor, with the declaration guard OFF\n");
    println!(
        "Every row of both sets that the back-channel guard still lets through, \
         through the real identifier and the real decoders. A **false positive** is any \
         rewrite that survives on a German/English voice; the **true positives** are the \
         two rows of the 45 a reader of Japanese would keep.\n"
    );
    println!(
        "| windows | floor | asked | rewrites kept | false positives | of {} genuine | FP rate |",
        GENUINE.len()
    );
    println!(
        "|--------:|------:|------:|--------------:|----------------:|--------------:|--------:|"
    );
    let mut spot: Vec<String> = Vec::new();
    for windows in [1usize, 3] {
        for floor in [1.0f32, 1.5] {
            let cfg = AsrConfig {
                lid_windows: windows,
                lid_min_s: floor,
                ..AsrConfig::default()
            };
            let mut lid = Lid::load(&models.lid(), 4, windows)?;
            let mut cjk = asr_cjk::Cjk::new(&models, &cfg);
            let (mut asked, mut kept, mut fp, mut tp) = (0usize, 0usize, 0usize, 0usize);
            for r in routed.iter().chain(control.iter()) {
                // The live chain, with rule 3 of `pre_route` deliberately out:
                // this arm is about what the IDENTIFIER can be trusted with.
                if no_declaration(r) == Pre::Nothing {
                    continue;
                }
                let Ok(samples) = recalld::ingest::read_wav(&data_dir.join(&r.audio_path)) else {
                    continue;
                };
                if samples.is_empty() || (samples.len() as f32 / SAMPLE_RATE as f32) < floor {
                    continue;
                }
                asked += 1;
                let reading = lid.identify(&samples);
                let Some(want) = asr_cjk::post_route(reading.as_ref(), &cfg) else {
                    // The French arm needs the night shift's GPU decoder; it is
                    // counted as routed and never as kept when that is absent,
                    // which is the honest reading of "this machine cannot".
                    continue;
                };
                if let asr_cjk::Rerouted::Replaced { text, .. } =
                    cjk.redecode(want, &samples, &lang_cfg)
                {
                    kept += 1;
                    if r.genuine == Some(true) {
                        tp += 1;
                    } else {
                        fp += 1;
                    }
                    if windows == 3 && floor == 1.5 {
                        spot.push(format!(
                            "| {} | {:.2} | `{}` | `{text}` | {} |",
                            r.id,
                            r.duration_s,
                            r.text.as_deref().unwrap_or("*(empty)*"),
                            if r.genuine == Some(true) { "yes" } else { "no" }
                        ));
                    }
                }
            }
            println!(
                "| {windows} | {floor} | {asked} | {kept} | {fp} | {tp} | {:.2}% |",
                100.0 * fp as f64 / asked.max(1) as f64
            );
        }
    }
    if !spot.is_empty() {
        println!("\n### What the strictest arm still keeps\n");
        println!("| id | seconds | live text | re-decode | right? |");
        println!("|----|--------:|-----------|-----------|--------|");
        for line in spot {
            println!("{line}");
        }
    }
    // Unused elsewhere, but the polyglot half has to be *reachable* from this
    // file or the bench is quietly only measuring one of the two routes.
    let _ = polyglot::is_routable("fr");
    Ok(())
}

/// The rows the live route rewrote, with the words it wrote over.
fn read_routed(conn: &Connection) -> Result<Vec<Row>> {
    let mut stmt = conn.prepare(
        "SELECT g.id, (g.t_end_ns - g.t_start_ns) / 1e9, g.audio_path, g.text, g.lang,
                (SELECT s.languages FROM speakers s
                 JOIN speaker_resolved r ON r.canonical_id = s.id
                 WHERE r.id = g.speaker_id),
                (SELECT o.prior_state FROM operations o
                  WHERE o.op = 'segments.redecode'
                    AND o.target_ids = '[' || g.id || ']'
                  ORDER BY o.at_utc_ns DESC, o.id DESC LIMIT 1)
         FROM segments g
         WHERE g.deleted_at IS NULL AND g.lang_via = 'lid'
         ORDER BY g.id",
    )?;
    Ok(stmt
        .query_map([], |r| {
            let id: i64 = r.get(0)?;
            let languages: Option<String> = r.get(5)?;
            let prior: Option<String> = r.get(6)?;
            let parsed: Option<serde_json::Value> =
                prior.as_deref().and_then(|s| serde_json::from_str(s).ok());
            Ok(Row {
                id,
                duration_s: r.get::<_, f64>(1)? as f32,
                audio_path: r.get(2)?,
                text: parsed
                    .as_ref()
                    .and_then(|v| v.get("text"))
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                declared: recalld::lang::parse_languages(languages.as_deref()),
                wrote: r.get(3)?,
                routed_as: r.get(4)?,
                genuine: Some(GENUINE.contains(&id)),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// The control: back-channels from the same voice, which nobody claims are
/// Japanese.
///
/// Deliberately the *same population* the 37 came out of — speaker 26, no
/// language ever settled, audio still on disk — rather than a random sample of
/// the archive, because a control made of readable German would be measuring
/// `lang::classify` rather than the identifier.
fn read_control(conn: &Connection, limit: usize) -> Result<Vec<Row>> {
    let mut stmt = conn.prepare(
        "SELECT g.id, (g.t_end_ns - g.t_start_ns) / 1e9, g.audio_path, g.text,
                (SELECT s.languages FROM speakers s
                 JOIN speaker_resolved r ON r.canonical_id = s.id
                 WHERE r.id = g.speaker_id)
         FROM segments g
         JOIN speaker_resolved sr ON sr.id = g.speaker_id
         JOIN speakers sp ON sp.id = sr.canonical_id
         WHERE g.deleted_at IS NULL
           AND (g.lang_via IS NULL OR g.lang_via <> 'lid')
           AND g.audio_path <> ''
           AND (g.t_end_ns - g.t_start_ns) / 1e9 >= 1.0
           AND sp.languages = '[\"de\",\"en\"]'
         ORDER BY g.t_start_ns DESC
         LIMIT ?1",
    )?;
    let all: Vec<Row> = stmt
        .query_map([limit as i64 * 8], |r| {
            let languages: Option<String> = r.get(4)?;
            Ok(Row {
                id: r.get(0)?,
                duration_s: r.get::<_, f64>(1)? as f32,
                audio_path: r.get(2)?,
                text: r.get(3)?,
                declared: recalld::lang::parse_languages(languages.as_deref()),
                wrote: None,
                routed_as: None,
                genuine: Some(false),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // Only the ones the OLD route would have asked about — a control of rows
    // nothing ever reached would put a zero in every column for free.
    let live = AsrConfig::default();
    Ok(all
        .into_iter()
        .filter(|r| {
            let t = r.text.as_deref().unwrap_or("");
            let padded = format!("{t} qwertzuiop asdfghjkl");
            asr_cjk::pre_route(None, Some(padded.as_str()), &live) != Pre::Nothing
        })
        .take(limit)
        .collect())
}
