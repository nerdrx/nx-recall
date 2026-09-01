//! Tiered retention and the reconciliation sweep (DESIGN §6, §8).
//!
//! Three jobs, all periodic, all boring on purpose:
//!
//! 1. **Finalise soft deletes.** A deleted row is hidden immediately and really
//!    removed once the undo window closes. That is when "deletion means
//!    deletion" is honoured: rows purged, audio unlinked, `VACUUM`.
//! 2. **Age out audio.** Audio is the heavy, short-lived tier; the transcript
//!    and the identity are the light, long-lived ones. Passing the audio limit
//!    drops the WAV and keeps the words.
//! 3. **Reconcile.** A crash between the database write and the file write
//!    leaves residue in both directions: files with no row (removed) and rows
//!    with no file (logged — the row is still the only record that the words
//!    were ever said).
//! 4. **Measure.** How much disk this program is actually using, broken down
//!    into the parts that behave differently: a capped audio tier, an unbounded
//!    transcript, and a fixed model set. It is measured *here*, once a sweep,
//!    because it costs a directory walk and the 3 s status poll must never pay
//!    for one.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::clock::utc_now_ns;
use crate::config::RetentionConfig;
use crate::control::Control;
use crate::store::Store;

const DAY_NS: i64 = 86_400 * 1_000_000_000;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    /// Soft-deleted rows whose undo window closed, now really gone.
    pub purged_rows: usize,
    /// Segment audio removed, either with its row or because it aged out.
    pub unlinked_files: usize,
    /// Rows that kept their transcript but lost their audio to the age limit.
    pub aged_audio: usize,
    /// Files under `segments/` that no row claims. Removed.
    pub orphan_files: usize,
    /// Rows whose audio file is missing. Logged, never deleted.
    pub dangling_paths: usize,
    /// Rows purged outright because they carried no memory value: no words and
    /// no speaker, past the audio window. See `sweep`.
    pub purged_empty: usize,
    pub vacuumed: bool,
}

/// What NX Recall is using on disk, in the four parts that behave differently.
///
/// The split is the point. Audio is heavy, capped by `[retention].audio_days`
/// and therefore self-limiting; the database is light and grows forever, which
/// is the trade DESIGN §8's tiers were chosen to make; goldens are exempt from
/// retention on purpose (§5 — they are what a future model gets re-enrolled
/// from); and the models are a fixed one-off that no setting will ever shrink.
/// A single "total" number would hide every one of those facts.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StorageUsage {
    /// `recall.db` plus its WAL and shared-memory files.
    pub db_bytes: u64,
    pub audio_bytes: u64,
    pub audio_files: u64,
    pub goldens_bytes: u64,
    pub models_bytes: u64,
    pub total_bytes: u64,
    pub measured_at_utc_ns: i64,
}

impl StorageUsage {
    pub fn to_json(self) -> Value {
        json!({
            "db_bytes": self.db_bytes,
            "audio_bytes": self.audio_bytes,
            "audio_files": self.audio_files,
            "goldens_bytes": self.goldens_bytes,
            "models_bytes": self.models_bytes,
            "total_bytes": self.total_bytes,
            "measured_at_utc_ns": self.measured_at_utc_ns.to_string(),
        })
    }
}

/// Walk the data directory and the model root. One `stat` per file, no
/// database access — so it can run while another thread holds the store.
pub fn measure(data_dir: &Path, models_dir: Option<&Path>, now_ns: i64) -> StorageUsage {
    let mut usage = StorageUsage {
        measured_at_utc_ns: now_ns,
        ..Default::default()
    };
    for name in ["recall.db", "recall.db-wal", "recall.db-shm"] {
        usage.db_bytes += file_len(&data_dir.join(name));
    }
    let (audio_bytes, audio_files) = dir_size(&data_dir.join("segments"));
    usage.audio_bytes = audio_bytes;
    usage.audio_files = audio_files;
    usage.goldens_bytes = dir_size(&data_dir.join("goldens")).0;
    // The model root is usually inside the data dir but does not have to be
    // (`[models].dir`), so it is passed in rather than assumed.
    usage.models_bytes = models_dir.map(|d| dir_size(d).0).unwrap_or(0);
    usage.total_bytes =
        usage.db_bytes + usage.audio_bytes + usage.goldens_bytes + usage.models_bytes;
    usage
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// `(bytes, files)` under `root`, recursively. A directory that is not there
/// weighs nothing, which is the right answer for a models root nobody fetched.
fn dir_size(root: &Path) -> (u64, u64) {
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => stack.push(entry.path()),
                Ok(t) if t.is_file() => {
                    files += 1;
                    bytes += entry.metadata().map(|m| m.len()).unwrap_or(0);
                }
                // Symlinks are not followed: a link into somebody else's tree
                // is not this program's disk usage.
                _ => {}
            }
        }
    }
    (bytes, files)
}

#[derive(Default)]
pub struct SweeperStop(AtomicBool);

impl SweeperStop {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    fn stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// One pass. Separated from the loop so it is testable without waiting hours.
pub fn sweep(
    cfg: &RetentionConfig,
    store: &Store,
    data_dir: &Path,
    now_ns: i64,
) -> Result<SweepReport> {
    let mut report = SweepReport::default();

    // 1. Soft deletes past the undo window.
    if cfg.undo_window_days > 0 {
        let cutoff = now_ns - cfg.undo_window_days as i64 * DAY_NS;
        let expired = store.expired_soft_deletes(cutoff)?;
        if !expired.is_empty() {
            let ids: Vec<i64> = expired.iter().map(|(id, _)| *id).collect();
            report.purged_rows = store.purge_segments(&ids)?;
            for (_, rel) in &expired {
                if unlink(data_dir, rel) {
                    report.unlinked_files += 1;
                }
            }
        }
    }

    // 2. Audio older than the audio tier. The transcript stays.
    if cfg.audio_days > 0 {
        let cutoff = now_ns - cfg.audio_days as i64 * DAY_NS;

        // 2a. Rows that are BOTH text-empty and speaker-NULL go entirely, on
        //     the *audio* clock rather than the transcript's.
        //
        //     The tiers exist because a transcript is cheap and is the memory,
        //     while audio is heavy and is only evidence (DESIGN §8). A row with
        //     neither words nor a name is neither: it is a door closing, a
        //     cough, a fragment the ASR had nothing to say about and the
        //     voicebank could not place. Keeping it forever would grow the
        //     light tier with rows that answer no question anybody can ask —
        //     they are not searchable (no words) and not attributable (no
        //     voice). So they expire with the audio they were evidence of,
        //     which is the only clock they were ever on.
        let empty = store.empty_unlabelled_older_than(cutoff)?;
        if !empty.is_empty() {
            let ids: Vec<i64> = empty.iter().map(|(id, _)| *id).collect();
            report.purged_empty = store.purge_segments(&ids)?;
            for (_, rel) in &empty {
                if unlink(data_dir, rel) {
                    report.unlinked_files += 1;
                }
            }
        }

        let aged = store.audio_older_than(cutoff)?;
        if !aged.is_empty() {
            let ids: Vec<i64> = aged.iter().map(|(id, _)| *id).collect();
            report.aged_audio = store.forget_audio(&ids)?;
            for (_, rel) in &aged {
                if unlink(data_dir, rel) {
                    report.unlinked_files += 1;
                }
            }
        }
    }

    // 3. Loose files against the database.
    if cfg.reconcile {
        let known = store.all_audio_paths()?;
        let mut expected: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for (id, rel) in &known {
            let abs = data_dir.join(rel);
            if abs.exists() {
                expected.insert(abs);
            } else {
                report.dangling_paths += 1;
                warn!(
                    segment = id,
                    path = %rel,
                    "segment audio is missing; the transcript is kept and the row left alone"
                );
            }
        }
        for file in wav_files(&data_dir.join("segments")) {
            if !expected.contains(&file) {
                match std::fs::remove_file(&file) {
                    Ok(()) => {
                        report.orphan_files += 1;
                        info!(path = %file.display(), "removed an orphaned segment file");
                    }
                    Err(e) => warn!(path = %file.display(), "could not remove an orphan: {e}"),
                }
            }
        }
        prune_empty_dirs(&data_dir.join("segments"));
    }

    if cfg.vacuum_after_purge && (report.purged_rows > 0 || report.purged_empty > 0) {
        store.vacuum()?;
        report.vacuumed = true;
    }
    Ok(report)
}

fn unlink(data_dir: &Path, rel: &str) -> bool {
    if rel.is_empty() {
        return false;
    }
    let path = data_dir.join(rel);
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            warn!(path = %path.display(), "could not remove segment audio: {e}");
            false
        }
    }
}

/// Every `.wav` under `root`, one level of session directories deep or more.
fn wav_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "wav") {
                out.push(path);
            }
        }
    }
    out
}

fn prune_empty_dirs(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && std::fs::read_dir(&path).is_ok_and(|mut d| d.next().is_none()) {
            let _ = std::fs::remove_dir(&path);
        }
    }
}

/// The periodic task. Sweeps once at start-up — a daemon that was off for a
/// week should not wait another interval before honouring retention — and then
/// on the configured interval.
pub fn run(
    cfg: &RetentionConfig,
    store: Arc<std::sync::Mutex<Store>>,
    data_dir: PathBuf,
    models_dir: Option<PathBuf>,
    control: Arc<Control>,
    stop: Arc<SweeperStop>,
) {
    let interval = Duration::from_secs(cfg.sweep_interval_s.max(60));
    loop {
        {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            match sweep(cfg, &guard, &data_dir, utc_now_ns()) {
                Ok(report) if report != SweepReport::default() => {
                    info!(
                        purged_rows = report.purged_rows,
                        purged_empty = report.purged_empty,
                        aged_audio = report.aged_audio,
                        unlinked_files = report.unlinked_files,
                        orphan_files = report.orphan_files,
                        dangling_paths = report.dangling_paths,
                        "retention sweep"
                    );
                }
                Ok(_) => debug!("retention sweep: nothing to do"),
                Err(e) => warn!("retention sweep failed: {e:#}"),
            }
        }
        // Measured after the sweep, so what `status` reports is what is on disk
        // now rather than what was there before the sweeper freed it. Outside
        // the store lock: this is a directory walk, not a query.
        control.set_storage(measure(&data_dir, models_dir.as_deref(), utc_now_ns()));
        let step = Duration::from_millis(200);
        let mut slept = Duration::ZERO;
        while slept < interval {
            if stop.stopped() {
                debug!("retention sweeper stopped");
                return;
            }
            std::thread::sleep(step);
            slept += step;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SegmentAnalysis;

    struct Rig {
        dir: PathBuf,
        store: Store,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn rig(name: &str) -> Rig {
        let dir =
            std::env::temp_dir().join(format!("nx-recall-retention-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        Rig { dir, store }
    }

    /// A segment with a real file behind it.
    fn segment(rig: &Rig, session: i64, t_start: i64, rel: &str) -> i64 {
        let path = rig.dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![0u8; 64]).unwrap();
        let id = rig
            .store
            .insert_segment(session, t_start, t_start + 1_000, rel, t_start)
            .unwrap();
        rig.store
            .set_segment_analysis(
                id,
                &SegmentAnalysis {
                    text: Some(format!("segment {id}")),
                    ..Default::default()
                },
            )
            .unwrap();
        id
    }

    fn session(rig: &Rig) -> i64 {
        let src = rig
            .store
            .upsert_source("VRChat.exe", "VRChat.exe", 0)
            .unwrap();
        rig.store.begin_session(src, 0).unwrap()
    }

    #[test]
    fn a_soft_delete_survives_its_undo_window_and_not_a_day_longer() {
        let r = rig("undo");
        let s = session(&r);
        let now = 100 * DAY_NS;
        let seg = segment(&r, s, now, "segments/000001/a.wav");
        r.store.soft_delete_segments(&[seg], now).unwrap();

        let cfg = RetentionConfig {
            audio_days: 0,
            reconcile: false,
            ..Default::default()
        };
        // Six days in: still undoable, file still there.
        let report = sweep(&cfg, &r.store, &r.dir, now + 6 * DAY_NS).unwrap();
        assert_eq!(report.purged_rows, 0);
        assert!(r.dir.join("segments/000001/a.wav").exists());
        assert_eq!(r.store.all_audio_paths().unwrap().len(), 1);

        // Eight days in: gone, for real.
        let report = sweep(&cfg, &r.store, &r.dir, now + 8 * DAY_NS).unwrap();
        assert_eq!(report.purged_rows, 1);
        assert_eq!(report.unlinked_files, 1);
        assert!(report.vacuumed);
        assert!(!r.dir.join("segments/000001/a.wav").exists());
        assert!(r.store.all_audio_paths().unwrap().is_empty());
    }

    #[test]
    fn audio_ages_out_while_the_transcript_stays() {
        let r = rig("audio-age");
        let s = session(&r);
        let now = 100 * DAY_NS;
        let old = segment(&r, s, now - 40 * DAY_NS, "segments/000001/old.wav");
        let fresh = segment(&r, s, now - DAY_NS, "segments/000001/new.wav");

        let cfg = RetentionConfig {
            reconcile: false,
            ..Default::default()
        };
        let report = sweep(&cfg, &r.store, &r.dir, now).unwrap();
        assert_eq!(report.aged_audio, 1);
        assert_eq!(report.unlinked_files, 1);
        assert!(!r.dir.join("segments/000001/old.wav").exists());
        assert!(r.dir.join("segments/000001/new.wav").exists());

        // The words outlive the audio: still searchable, still in the transcript.
        assert_eq!(r.store.search("segment", 10).unwrap().len(), 2);
        assert_eq!(r.store.transcript(None, None).unwrap().len(), 2);
        assert_eq!(
            r.store.all_audio_paths().unwrap(),
            vec![(fresh, "segments/000001/new.wav".into())]
        );
        let _ = old;

        // Idempotent: a second pass finds nothing left to age.
        assert_eq!(sweep(&cfg, &r.store, &r.dir, now).unwrap().aged_audio, 0);
    }

    #[test]
    fn reconciliation_removes_orphans_and_only_logs_dangling_paths() {
        let r = rig("reconcile");
        let s = session(&r);
        let now = 100 * DAY_NS;
        let kept = segment(&r, s, now, "segments/000001/kept.wav");
        let dangling = segment(&r, s, now, "segments/000001/gone.wav");
        // A crash between the row and the file, both directions.
        std::fs::remove_file(r.dir.join("segments/000001/gone.wav")).unwrap();
        std::fs::write(r.dir.join("segments/000001/orphan.wav"), b"x").unwrap();
        std::fs::create_dir_all(r.dir.join("segments/000009")).unwrap();

        let cfg = RetentionConfig {
            undo_window_days: 0,
            audio_days: 0,
            ..Default::default()
        };
        let report = sweep(&cfg, &r.store, &r.dir, now).unwrap();
        assert_eq!(report.orphan_files, 1);
        assert_eq!(report.dangling_paths, 1);
        assert!(!r.dir.join("segments/000001/orphan.wav").exists());
        assert!(r.dir.join("segments/000001/kept.wav").exists());
        // The dangling row is kept: it is the only record of what was said.
        assert_eq!(r.store.transcript(None, None).unwrap().len(), 2);
        assert!(
            !r.dir.join("segments/000009").exists(),
            "empty dirs are pruned"
        );
        let _ = (kept, dangling);
    }

    #[test]
    fn a_soft_deleted_rows_audio_is_not_an_orphan() {
        // The undo window is worth nothing if the sweeper removes the audio the
        // moment the row leaves the read paths.
        let r = rig("soft-not-orphan");
        let s = session(&r);
        let now = 100 * DAY_NS;
        let seg = segment(&r, s, now, "segments/000001/a.wav");
        r.store.soft_delete_segments(&[seg], now).unwrap();

        let cfg = RetentionConfig {
            audio_days: 0,
            ..Default::default()
        };
        let report = sweep(&cfg, &r.store, &r.dir, now + DAY_NS).unwrap();
        assert_eq!(report.orphan_files, 0);
        assert!(r.dir.join("segments/000001/a.wav").exists());
    }

    /// A row with no words and no voice is neither searchable nor
    /// attributable: it expires with the audio it was evidence of, rather than
    /// sitting in the light tier forever answering no question anybody can ask.
    #[test]
    fn a_row_with_neither_words_nor_a_voice_ages_out_with_its_audio() {
        let r = rig("empty-rows");
        let s = session(&r);
        let now = 100 * DAY_NS;

        // Old and empty: gone entirely, row and file.
        let path = r.dir.join("segments/000001/empty.wav");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, vec![0u8; 64]).unwrap();
        let empty = r
            .store
            .insert_segment(
                s,
                now - 40 * DAY_NS,
                now - 40 * DAY_NS + 1_000,
                "segments/000001/empty.wav",
                0,
            )
            .unwrap();

        // Old, but it has words: the transcript is the memory, so it stays and
        // only loses its audio.
        let worded = segment(&r, s, now - 40 * DAY_NS, "segments/000001/worded.wav");
        // Old and empty, but somebody's voice is on it: a name is a reason to
        // keep a row even with nothing written down.
        let spk = r.store.mint_speaker(0).unwrap();
        let voiced_path = "segments/000001/voiced.wav";
        std::fs::write(r.dir.join(voiced_path), vec![0u8; 64]).unwrap();
        let voiced = r
            .store
            .insert_segment(
                s,
                now - 40 * DAY_NS,
                now - 40 * DAY_NS + 1_000,
                voiced_path,
                0,
            )
            .unwrap();
        r.store
            .set_segment_speaker(voiced, Some(spk), Some(0.8))
            .unwrap();
        // Fresh and empty: inside the window, so nothing happens to it yet.
        let fresh_path = "segments/000001/fresh.wav";
        std::fs::write(r.dir.join(fresh_path), vec![0u8; 64]).unwrap();
        let fresh = r
            .store
            .insert_segment(s, now - DAY_NS, now - DAY_NS + 1_000, fresh_path, 0)
            .unwrap();

        let cfg = RetentionConfig {
            reconcile: false,
            ..Default::default()
        };
        let report = sweep(&cfg, &r.store, &r.dir, now).unwrap();
        assert_eq!(report.purged_empty, 1);
        assert!(!path.exists(), "the empty row's audio went with the row");

        let live: Vec<i64> = r
            .store
            .transcript(None, None)
            .unwrap()
            .into_iter()
            .map(|t| t.segment_id)
            .collect();
        assert!(!live.contains(&empty), "the empty row is gone for real");
        assert!(live.contains(&worded), "words are the memory; they stay");
        assert!(
            live.contains(&voiced),
            "a named voice is a reason to keep it"
        );
        assert!(live.contains(&fresh), "still inside the audio window");
        // The two survivors that were old lost their audio, as before.
        assert_eq!(report.aged_audio, 2);
        assert!(r.dir.join(fresh_path).exists());

        // Idempotent: a second pass has nothing left to purge.
        assert_eq!(sweep(&cfg, &r.store, &r.dir, now).unwrap().purged_empty, 0);
    }

    #[test]
    fn storage_is_measured_in_the_parts_that_behave_differently() {
        let r = rig("storage");
        let s = session(&r);
        segment(&r, s, 0, "segments/000001/a.wav");
        segment(&r, s, 1_000, "segments/000001/b.wav");
        std::fs::create_dir_all(r.dir.join("goldens/000001")).unwrap();
        std::fs::write(r.dir.join("goldens/000001/g.wav"), vec![0u8; 128]).unwrap();
        let models = r.dir.join("models");
        std::fs::create_dir_all(models.join("asr")).unwrap();
        std::fs::write(models.join("asr/encoder.onnx"), vec![0u8; 4096]).unwrap();

        let usage = measure(&r.dir, Some(&models), 7);
        assert_eq!(usage.audio_files, 2);
        assert_eq!(usage.audio_bytes, 128, "two 64-byte fixtures");
        assert_eq!(usage.goldens_bytes, 128);
        assert_eq!(usage.models_bytes, 4096);
        assert!(usage.db_bytes > 0, "recall.db is on disk and is not empty");
        assert_eq!(
            usage.total_bytes,
            usage.db_bytes + usage.audio_bytes + usage.goldens_bytes + usage.models_bytes
        );
        assert_eq!(usage.measured_at_utc_ns, 7);

        // Goldens live outside `segments/` precisely so retention never walks
        // them — and so they are counted apart from the audio tier here.
        assert!(usage.audio_bytes < usage.goldens_bytes + usage.audio_bytes);

        // A models root nobody fetched weighs nothing rather than erroring.
        let none = measure(&r.dir, Some(&r.dir.join("nowhere")), 8);
        assert_eq!(none.models_bytes, 0);
        assert_eq!(measure(&r.dir, None, 9).models_bytes, 0);
    }

    /// The number `status` serves has to be the one from *after* the sweep, or
    /// the first thing a user sees post-cleanup is the size it used to be.
    #[test]
    fn the_cached_measurement_follows_a_sweep() {
        let r = rig("storage-freshness");
        let s = session(&r);
        let now = 100 * DAY_NS;
        segment(&r, s, now - 40 * DAY_NS, "segments/000001/old.wav");
        segment(&r, s, now - DAY_NS, "segments/000001/new.wav");

        let before = measure(&r.dir, None, now);
        assert_eq!(before.audio_files, 2);

        let cfg = RetentionConfig {
            reconcile: false,
            ..Default::default()
        };
        sweep(&cfg, &r.store, &r.dir, now).unwrap();

        let after = measure(&r.dir, None, now + 1);
        assert_eq!(after.audio_files, 1, "the aged-out WAV is really gone");
        assert!(after.audio_bytes < before.audio_bytes);

        // And the cache carries exactly what was measured, so `status` cannot
        // disagree with the disk it was read from.
        let control = crate::control::Control::new(
            r.dir.clone(),
            None,
            &crate::allowlist::Allowlist::from_rules([("x", false)]),
        );
        assert_eq!(control.storage(), None, "nothing measured yet is not zero");
        assert_eq!(control.storage_json(), serde_json::Value::Null);
        control.set_storage(after);
        assert_eq!(control.storage(), Some(after));
        let json = control.storage_json();
        assert_eq!(json["audio_files"], serde_json::json!(1));
        assert_eq!(
            json["total_bytes"],
            serde_json::json!(after.total_bytes),
            "one number, reported once, from one measurement"
        );
    }

    #[test]
    fn a_disabled_tier_does_nothing() {
        let r = rig("disabled");
        let s = session(&r);
        let now = 1_000 * DAY_NS;
        let seg = segment(&r, s, 0, "segments/000001/ancient.wav");
        r.store.soft_delete_segments(&[seg], 0).unwrap();

        let cfg = RetentionConfig {
            undo_window_days: 0,
            audio_days: 0,
            reconcile: false,
            ..Default::default()
        };
        assert_eq!(
            sweep(&cfg, &r.store, &r.dir, now).unwrap(),
            SweepReport::default()
        );
        assert!(r.dir.join("segments/000001/ancient.wav").exists());
    }
}
