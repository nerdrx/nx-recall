//! What is in the archive that the live route never saw? (0.11.9)
//!
//! Run against a **copy** of a real database — never the live one:
//!
//! ```text
//! sqlite3 "file:$HOME/.local/share/nx-recall/recall.db?mode=ro" \
//!         ".backup /path/to/scratch/recall.db"
//! cargo run -p recalld --release --example lang_sweep_bench -- \
//!     /path/to/scratch/recall.db $HOME/.local/share/nx-recall
//! ```
//!
//! # The question
//!
//! 0.11.8 lowered `[asr].lid_min_s` from 1.5 s to 1.0 s and added the French
//! re-read. Everything captured before that was decided under the old floor and
//! by the old routes, so the archive holds rows the identifier was never asked
//! about. This bench answers, on **real rows rather than on FLEURS**, three
//! things before any of it is shipped as a default:
//!
//! 1. how many untagged rows the live [`recalld::asr_cjk::pre_route`] would
//!    even hand to the identifier — the gate that §28 showed is most of the
//!    safety argument;
//! 2. what the identifier says about them, as a histogram by duration bucket,
//!    because the whole reason the floor moved is that short rows are different;
//! 3. how many of those readings would actually **route** — ja/ko/zh through
//!    [`recalld::asr_cjk::post_route`] and fr through
//!    [`recalld::polyglot::post_route`] — which is the number the sweep's cost
//!    and its risk are both proportional to.
//!
//! The false-positive gate the routes shipped under (FINDINGS §22, §28) is
//! **≤ 1% of de/en heard as a routed language**, and this bench cannot measure
//! it directly: the archive has no ground truth. What it can measure, and does,
//! is the *upper bound* — every routed reading on a row whose speaker is
//! declared de/en-only but whose transcript still reached the identifier, and
//! every routed reading whose re-decode the judge then threw away.
//!
//! With `--redecode N` it goes on to run the real decoders over the first `N`
//! routed rows and prints the live text against the re-decode, which is the
//! spot-check §29 quotes. That stage loads a 655 MB or 239 MB sherpa session
//! and, for `fr`, spawns `whisper-cli` on the GPU — one model at a time.
//!
//! Numbers live in `spike/FINDINGS.md` §29.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

use recalld::asr_cjk::{self, Pre};
use recalld::config::{
    AsrConfig, LangConfig, ModelsConfig, NightConfig, RuntimeConfig, SAMPLE_RATE,
};
use recalld::lid::{Lid, Reading};
use recalld::models::ModelSet;
use recalld::polyglot;

/// One untagged archive row, with everything `pre_route` needs.
struct Row {
    id: i64,
    duration_s: f32,
    audio_path: String,
    text: Option<String>,
    declared: Option<Vec<String>>,
}

/// The duration buckets the report is cut by. The first is the band 0.11.8
/// opened and the archive was never asked about; the rest are there so a
/// reading that only happens at the floor cannot hide inside an average.
fn bucket(duration_s: f32) -> &'static str {
    match duration_s {
        d if d < 1.25 => "1.00-1.25",
        d if d < 1.50 => "1.25-1.50",
        d if d < 2.00 => "1.50-2.00",
        d if d < 3.00 => "2.00-3.00",
        _ => "3.00+",
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let db = args.next().unwrap_or_else(|| {
        eprintln!("usage: lang_sweep_bench <db> <data-dir> [--redecode N] [--limit N]");
        std::process::exit(2);
    });
    let data_dir =
        std::path::PathBuf::from(args.next().unwrap_or_else(|| {
            format!("{}/.local/share/nx-recall", std::env::var("HOME").unwrap())
        }));
    let mut redecode = 0usize;
    let mut limit = usize::MAX;
    // The two narrowings §29 has to choose between, as flags, so the arms are
    // one command apart and the table can carry all of them.
    let mut windows = AsrConfig::default().lid_windows;
    let mut floor = AsrConfig::default().lid_min_s;
    let rest: Vec<String> = args.collect();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--redecode" => redecode = it.next().and_then(|n| n.parse().ok()).unwrap_or(20),
            "--limit" => limit = it.next().and_then(|n| n.parse().ok()).unwrap_or(usize::MAX),
            "--windows" => windows = it.next().and_then(|n| n.parse().ok()).unwrap_or(1),
            "--floor" => floor = it.next().and_then(|n| n.parse().ok()).unwrap_or(1.0),
            other => eprintln!("ignoring {other}"),
        }
    }

    let conn = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {db}"))?;
    let asr_cfg = AsrConfig {
        lid_windows: windows,
        lid_min_s: floor,
        ..AsrConfig::default()
    };
    let lang_cfg = LangConfig::default();
    println!(
        "arm: `lid_windows = {windows}`, floor {floor} s, \
         `lid_min_confidence = {}`\n",
        asr_cfg.lid_min_confidence
    );

    // The sweep's own work list, in the shape the shipped query has: not
    // deleted, no language, audio still on disk, at least `lid_min_s` long, and
    // never a row a person corrected by hand.
    let mut stmt = conn.prepare(
        "SELECT g.id, (g.t_end_ns - g.t_start_ns) / 1e9, g.audio_path, g.text,
                (SELECT s.languages FROM speakers s
                 JOIN speaker_resolved r ON r.canonical_id = s.id
                 WHERE r.id = g.speaker_id)
         FROM segments g
         WHERE g.deleted_at IS NULL
           AND g.lang IS NULL
           AND g.audio_path <> ''
           AND (g.t_end_ns - g.t_start_ns) / 1e9 >= ?1
           AND NOT EXISTS (
               SELECT 1 FROM operations o
               WHERE o.op = 'segments.correct'
                 AND o.target_ids = '[' || g.id || ']')
         ORDER BY g.t_start_ns",
    )?;
    let rows: Vec<Row> = stmt
        .query_map([asr_cfg.lid_min_s as f64], |r| {
            let languages: Option<String> = r.get(4)?;
            Ok(Row {
                id: r.get(0)?,
                duration_s: r.get::<_, f64>(1)? as f32,
                audio_path: r.get(2)?,
                text: r.get(3)?,
                declared: recalld::lang::parse_languages(languages.as_deref()),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    println!(
        "{} untagged rows at or above {} s\n",
        rows.len(),
        asr_cfg.lid_min_s
    );

    // ---- stage one: who is even asked ------------------------------------
    //
    // `pre_route` is the live gate and it is reused rather than re-implemented,
    // for §28's reason: a second copy in a bench drifts from the first the day
    // after it is written.
    let mut asked: Vec<&Row> = Vec::new();
    let mut pre: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for row in &rows {
        let p = asr_cjk::pre_route(row.declared.as_ref(), row.text.as_deref());
        let name = match p {
            Pre::Nothing => "nothing",
            Pre::Direct(_) => "declared",
            Pre::AskLid => "ask",
        };
        *pre.entry((bucket(row.duration_s), name)).or_default() += 1;
        if p != Pre::Nothing {
            asked.push(row);
        }
    }
    println!("## What `pre_route` does with the archive\n");
    println!("| bucket | nothing | declared | asks the identifier |");
    println!("|--------|--------:|---------:|--------------------:|");
    for b in ["1.00-1.25", "1.25-1.50", "1.50-2.00", "2.00-3.00", "3.00+"] {
        let g = |k: &str| pre.get(&(b, k)).copied().unwrap_or(0);
        println!(
            "| {b} | {} | {} | {} |",
            g("nothing"),
            g("declared"),
            g("ask")
        );
    }
    println!("\n{} rows reach the identifier.\n", asked.len());

    // ---- stage two: the identifier ---------------------------------------
    let models = ModelSet::resolve_at(data_dir.join("models"), &ModelsConfig::default());
    let lid_model = models.lid();
    anyhow::ensure!(lid_model.present(), "the identifier is not installed");
    let mut lid = Lid::load(&lid_model, 4, asr_cfg.lid_windows)?;

    /// One row's outcome, kept for the report and for the spot-check.
    struct Heard<'a> {
        row: &'a Row,
        reading: Option<Reading>,
        routed: Option<String>,
    }
    let mut heard: Vec<Heard<'_>> = Vec::new();
    let mut no_audio = 0usize;
    let started = std::time::Instant::now();
    let mut audio_s = 0.0f64;
    for (n, row) in asked.iter().take(limit).enumerate() {
        if n % 200 == 0 {
            eprint!("\r  {n}/{}\x1b[K", asked.len().min(limit));
        }
        let Ok(samples) = recalld::ingest::read_wav(&data_dir.join(&row.audio_path)) else {
            no_audio += 1;
            continue;
        };
        if samples.is_empty() {
            no_audio += 1;
            continue;
        }
        audio_s += samples.len() as f64 / SAMPLE_RATE as f64;
        let reading = lid.identify(&samples);
        // Exactly the live decision, both halves, in the live order: the CJK
        // route runs first and `polyglot::post_route` refuses `ja`.
        let routed = asr_cjk::post_route(reading.as_ref(), &asr_cfg)
            .map(str::to_string)
            .or_else(|| polyglot::post_route(reading.as_ref(), &asr_cfg));
        heard.push(Heard {
            row,
            reading,
            routed,
        });
    }
    eprintln!("\r\x1b[K");
    let wall = started.elapsed().as_secs_f64();
    println!(
        "identified {} rows ({:.0} s of audio) in {:.0} s — RTF {:.3}; {no_audio} had no audio\n",
        heard.len(),
        audio_s,
        wall,
        wall / audio_s.max(1.0),
    );

    // ---- the histogram ----------------------------------------------------
    let mut hist: BTreeMap<(&str, String), usize> = BTreeMap::new();
    let mut langs: BTreeMap<String, usize> = BTreeMap::new();
    for h in &heard {
        let tag = h
            .reading
            .as_ref()
            .map(|r| r.lang.clone())
            .unwrap_or_else(|| "(none)".into());
        *hist
            .entry((bucket(h.row.duration_s), tag.clone()))
            .or_default() += 1;
        *langs.entry(tag).or_default() += 1;
    }
    let mut top: Vec<(&String, &usize)> = langs.iter().collect();
    top.sort_by(|a, b| b.1.cmp(a.1));
    println!("## What the identifier heard, by duration\n");
    print!("| bucket | n |");
    let columns: Vec<&String> = top.iter().take(12).map(|(l, _)| *l).collect();
    for l in &columns {
        print!(" {l} |");
    }
    println!();
    print!("|--------|--:|");
    for _ in &columns {
        print!("---:|");
    }
    println!();
    for b in ["1.00-1.25", "1.25-1.50", "1.50-2.00", "2.00-3.00", "3.00+"] {
        let n: usize = columns
            .iter()
            .map(|l| hist.get(&(b, (*l).clone())).copied().unwrap_or(0))
            .sum();
        print!("| {b} | {n} |");
        for l in &columns {
            print!(" {} |", hist.get(&(b, (*l).clone())).copied().unwrap_or(0));
        }
        println!();
    }
    println!("\n(the tail: {} distinct readings in all)\n", langs.len());

    // ---- what would route -------------------------------------------------
    let mut routed_by: BTreeMap<(&str, String), usize> = BTreeMap::new();
    let mut declared_deen_routed = 0usize;
    for h in &heard {
        let Some(tag) = h.routed.as_ref() else {
            continue;
        };
        *routed_by
            .entry((bucket(h.row.duration_s), tag.clone()))
            .or_default() += 1;
        // The upper bound on the false-positive rate this archive can show: a
        // voice the user declared German- or English-only, heard as something
        // else. Not proof of an error — a German speaker's French turn is not a
        // mistake, which is exactly why `pre_route` only pins a SOLE tag — but
        // it is the only negative control the archive has.
        if h.row
            .declared
            .as_ref()
            .is_some_and(|d| d.iter().all(|l| l == "de" || l == "en"))
        {
            declared_deen_routed += 1;
        }
    }
    println!("## What would route\n");
    println!("| bucket | ja | ko | zh | fr | total |");
    println!("|--------|---:|---:|---:|---:|------:|");
    let mut totals = [0usize; 4];
    for b in ["1.00-1.25", "1.25-1.50", "1.50-2.00", "2.00-3.00", "3.00+"] {
        let g = |k: &str| routed_by.get(&(b, k.to_string())).copied().unwrap_or(0);
        let (ja, ko, zh, fr) = (g("ja"), g("ko"), g("zh"), g("fr"));
        totals[0] += ja;
        totals[1] += ko;
        totals[2] += zh;
        totals[3] += fr;
        println!(
            "| {b} | {ja} | {ko} | {zh} | {fr} | {} |",
            ja + ko + zh + fr
        );
    }
    println!(
        "| **all** | {} | {} | {} | {} | **{}** |",
        totals[0],
        totals[1],
        totals[2],
        totals[3],
        totals.iter().sum::<usize>()
    );
    let routed_n: usize = totals.iter().sum();
    println!(
        "\n{routed_n} of {} identified rows would route ({:.1}%). {declared_deen_routed} of them \
         belong to a voice declared de/en-only — the archive's only negative control, and an \
         UPPER bound: a German speaker's French turn is not an error.\n",
        heard.len(),
        100.0 * routed_n as f64 / heard.len().max(1) as f64,
    );

    // ---- stage three: the judge, over every routed row --------------------
    //
    // The number that decides whether the sweep ships at all. §22 and §28
    // shipped the two routes under a **1% of de/en into a routed language**
    // gate, and the stage-two table above is the raw column §28 already warned
    // is the wrong denominator. The right one is measured here: run the real
    // decoder, apply the real judge, and count what survives.
    if redecode == 0 {
        return Ok(());
    }
    /// `(routed-as, bucket)` → `(kept, touched, kept on a de/en-declared voice)`.
    type Cell = (usize, usize, usize);
    let mut kept: BTreeMap<(String, &str), Cell> = BTreeMap::new();
    let mut examples: Vec<String> = Vec::new();
    let deen = |row: &Row| {
        row.declared
            .as_ref()
            .is_some_and(|d| d.iter().all(|l| l == "de" || l == "en"))
    };
    let declared_of = |row: &Row| {
        row.declared
            .as_ref()
            .map(|d| d.join("+"))
            .unwrap_or_else(|| "-".into())
    };

    // One model in RAM at a time: every CJK row through the sherpa sessions
    // first, then the GPU rows through `whisper-cli`.
    let mut cjk = asr_cjk::Cjk::new(&models, &asr_cfg);
    let cjk_rows: Vec<&Heard<'_>> = heard
        .iter()
        .filter(|h| {
            h.routed
                .as_deref()
                .is_some_and(|t| asr_cjk::CJK.contains(&t))
        })
        .collect();
    for (n, h) in cjk_rows.iter().enumerate() {
        eprint!("\r  cjk {n}/{}\x1b[K", cjk_rows.len());
        let want = h.routed.as_deref().unwrap();
        let Ok(samples) = recalld::ingest::read_wav(&data_dir.join(&h.row.audio_path)) else {
            continue;
        };
        let out = cjk.redecode(want, &samples, &lang_cfg);
        let e = kept
            .entry((want.to_string(), bucket(h.row.duration_s)))
            .or_default();
        e.1 += 1;
        if let asr_cjk::Rerouted::Replaced { text, lang, .. } = &out {
            e.0 += 1;
            if deen(h.row) {
                e.2 += 1;
            }
            examples.push(format!(
                "- **{}** ({:.2} s, heard `{want}`, voice declared `{}`)\n  - live: `{}`\n  \
                 - re-decode (`{lang}`): `{text}`",
                h.row.id,
                h.row.duration_s,
                declared_of(h.row),
                h.row.text.as_deref().unwrap_or(""),
            ));
        }
    }
    eprintln!("\r\x1b[K");
    drop(cjk);

    let mut poly = polyglot::Polyglot::new(
        &models,
        &asr_cfg,
        &NightConfig::default(),
        &RuntimeConfig::default(),
    );
    let poly_rows: Vec<&Heard<'_>> = heard
        .iter()
        .filter(|h| {
            h.routed
                .as_deref()
                .is_some_and(|t| !asr_cjk::CJK.contains(&t))
        })
        .collect();
    for (n, h) in poly_rows.iter().enumerate() {
        eprint!("\r  poly {n}/{}\x1b[K", poly_rows.len());
        let want = h.routed.as_deref().unwrap();
        let Ok(samples) = recalld::ingest::read_wav(&data_dir.join(&h.row.audio_path)) else {
            continue;
        };
        let prior = h.row.text.as_deref().unwrap_or("");
        let out = poly.redecode(want, &samples, prior, &lang_cfg, &asr_cfg);
        let e = kept
            .entry((want.to_string(), bucket(h.row.duration_s)))
            .or_default();
        e.1 += 1;
        if let polyglot::Rerouted::Replaced { text, .. } = &out {
            e.0 += 1;
            if deen(h.row) {
                e.2 += 1;
            }
            examples.push(format!(
                "- **{}** ({:.2} s, heard `{want}`, voice declared `{}`)\n  - live: `{prior}`\n  \
                 - re-decode (`{want}`): `{text}`",
                h.row.id,
                h.row.duration_s,
                declared_of(h.row),
            ));
        }
    }
    eprintln!("\r\x1b[K");

    println!("## Of the rows that route, how many survive the judge\n");
    println!("| routed as | bucket | touched | kept | rate | kept on a de/en voice |");
    println!("|-----------|--------|--------:|-----:|-----:|----------------------:|");
    let (mut all_t, mut all_k, mut all_d) = (0usize, 0usize, 0usize);
    for ((tag, b), (k, t, d)) in &kept {
        all_t += t;
        all_k += k;
        all_d += d;
        println!(
            "| {tag} | {b} | {t} | {k} | {:.1}% | {d} |",
            100.0 * *k as f64 / (*t).max(1) as f64
        );
    }
    println!(
        "| **all** | | {all_t} | {all_k} | {:.1}% | {all_d} |",
        100.0 * all_k as f64 / all_t.max(1) as f64
    );
    let deen_asked = heard.iter().filter(|h| deen(h.row)).count();
    println!(
        "\n{all_k} rows would be rewritten. {all_d} of them are on a voice declared de/en-only, \
         which against the {deen_asked} de/en-declared rows that reached the identifier is \
         {:.2}% — the gate is 1%.\n",
        100.0 * all_d as f64 / deen_asked.max(1) as f64,
    );

    println!("## The spot-check: live text against the re-decode\n");
    for line in examples.iter().take(redecode) {
        println!("{line}");
    }
    Ok(())
}
