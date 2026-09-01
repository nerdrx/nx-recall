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
    /// Files under `goldens/` that no `golden_samples` row claims. Removed.
    pub orphan_goldens: usize,
    /// `golden_samples` rows whose file is gone. **Removed**, unlike a dangling
    /// segment: a segment row without audio is still the transcript, which is
    /// the memory; a golden without its clip is nothing at all — it exists only
    /// to be re-embedded by a future model, and it cannot be.
    pub dangling_goldens: usize,
    /// Conversations left with no live turns, removed (`prune_empty_threads`).
    pub pruned_threads: usize,
    /// Things the sweep tried and could not do: an unlink that failed, an
    /// orphan it could not remove, a row it could not delete. Counted rather
    /// than only logged, because a sweep that quietly stops working is exactly
    /// the failure nobody notices (audit finding #22).
    pub errors: usize,
    pub vacuumed: bool,
}

impl SweepReport {
    /// The `last_sweep` block `status` carries and the `sweep` event repeats.
    pub fn to_json(&self, started_at_utc_ns: i64) -> Value {
        json!({
            "started_at_utc_ns": started_at_utc_ns.to_string(),
            "purged": self.purged_rows + self.purged_empty,
            "purged_rows": self.purged_rows,
            "purged_empty": self.purged_empty,
            "aged_audio": self.aged_audio,
            "unlinked_files": self.unlinked_files,
            "orphans_removed": self.orphan_files + self.orphan_goldens,
            "orphan_files": self.orphan_files,
            "orphan_goldens": self.orphan_goldens,
            "dangling": self.dangling_paths + self.dangling_goldens,
            "dangling_paths": self.dangling_paths,
            "dangling_goldens": self.dangling_goldens,
            "pruned_threads": self.pruned_threads,
            "errors": self.errors,
            "vacuumed": self.vacuumed,
        })
    }
}

/// How recently a file may have been written and still be treated as settled.
///
/// The reconciliation pass removes every `.wav` no row claims, and the pipeline
/// writes the file *before* it inserts the row — deliberately, and outside the
/// store lock, so audio never waits on a query. A sweep that snapshots the
/// table and then unlinks a file finished in the gap destroys a segment
/// mid-write and leaves the row pointing at nothing (audit finding #3). A
/// minute of grace is far more than that gap and far less than the age of any
/// real orphan, which is residue from a crash in a previous run.
const SETTLE: Duration = Duration::from_secs(60);

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
///
/// `now_ns` is the retention clock — what the undo window and the audio tier
/// are measured against, and a test moves it by days. The file-settling cutoff
/// is a *different* clock and cannot be derived from it: it is about wall-clock
/// mtimes on disk, so it comes from the real one. [`sweep_at`] is the same pass
/// with that cutoff given explicitly, which is the only way to test the
/// interleaving in finding #3.
pub fn sweep(
    cfg: &RetentionConfig,
    store: &Store,
    data_dir: &Path,
    now_ns: i64,
) -> Result<SweepReport> {
    let settled_before = std::time::SystemTime::now()
        .checked_sub(SETTLE)
        .unwrap_or(std::time::UNIX_EPOCH);
    sweep_at(cfg, store, data_dir, now_ns, settled_before)
}

/// [`sweep`], with the "a file this new might still be being written" cutoff
/// passed in. Files modified after `settled_before` are left alone by the
/// reconciliation pass whatever the database says about them.
pub fn sweep_at(
    cfg: &RetentionConfig,
    store: &Store,
    data_dir: &Path,
    now_ns: i64,
    settled_before: std::time::SystemTime,
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
                match unlink(data_dir, rel) {
                    Unlinked::Removed => report.unlinked_files += 1,
                    Unlinked::Absent => {}
                    Unlinked::Failed => report.errors += 1,
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
                match unlink(data_dir, rel) {
                    Unlinked::Removed => report.unlinked_files += 1,
                    Unlinked::Absent => {}
                    Unlinked::Failed => report.errors += 1,
                }
            }
        }

        let aged = store.audio_older_than(cutoff)?;
        if !aged.is_empty() {
            let ids: Vec<i64> = aged.iter().map(|(id, _)| *id).collect();
            report.aged_audio = store.forget_audio(&ids)?;
            for (_, rel) in &aged {
                match unlink(data_dir, rel) {
                    Unlinked::Removed => report.unlinked_files += 1,
                    Unlinked::Absent => {}
                    Unlinked::Failed => report.errors += 1,
                }
            }
        }
    }

    // 2b. Conversations the purges above emptied out. `prune_empty_threads`
    //     had no callers at all before this (audit finding #15), so a fully
    //     deleted conversation left a `threads` row behind forever.
    if report.purged_rows > 0 || report.purged_empty > 0 {
        report.pruned_threads = store.prune_empty_threads()?;
    }

    // 3. Loose files against the database, both trees and both directions.
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
            if expected.contains(&file) {
                continue;
            }
            // Written since the sweep's cutoff: this is a file the pipeline may
            // be finishing right now, whose row is not in the snapshot above
            // because it does not exist *yet*. Removing it would destroy a
            // segment mid-write (finding #3). It will still be here next sweep,
            // by which time it is either claimed or genuinely orphaned.
            if !settled(&file, settled_before) {
                debug!(path = %file.display(), "leaving a just-written file for the next sweep");
                continue;
            }
            match std::fs::remove_file(&file) {
                Ok(()) => {
                    report.orphan_files += 1;
                    info!(path = %file.display(), "removed an orphaned segment file");
                }
                Err(e) => {
                    report.errors += 1;
                    warn!(path = %file.display(), "could not remove an orphan: {e}");
                }
            }
        }
        prune_empty_dirs(&data_dir.join("segments"));

        // Goldens. Exempt from *retention* — they are what a future embedding
        // model gets re-enrolled from (DESIGN §5) — but nothing ever reconciled
        // them in either direction, while two paths write a golden file and its
        // row separately and a third deletes them separately (audit finding
        // #19). Both halves, and the exemption is untouched: nothing here looks
        // at a golden's age.
        let goldens = store.all_golden_paths()?;
        let mut expected_goldens: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();
        for (id, rel) in &goldens {
            let abs = data_dir.join(rel);
            if abs.exists() {
                expected_goldens.insert(abs);
                continue;
            }
            // Removed, not merely logged: a segment row without audio is still
            // the transcript, and the transcript is the memory. A golden row
            // without its clip is nothing — it exists only to be re-embedded,
            // and a row claiming a voiceprint it cannot produce is a lie the
            // migration would trip over.
            report.dangling_goldens += 1;
            match store.delete_golden_sample(*id) {
                Ok(_) => warn!(
                    golden = id,
                    path = %rel,
                    "a golden sample lost its audio; the row went with it"
                ),
                Err(e) => {
                    report.errors += 1;
                    warn!(golden = id, "could not remove a golden with no file: {e:#}");
                }
            }
        }
        for file in wav_files(&data_dir.join("goldens")) {
            if expected_goldens.contains(&file) || !settled(&file, settled_before) {
                continue;
            }
            match std::fs::remove_file(&file) {
                Ok(()) => {
                    report.orphan_goldens += 1;
                    info!(path = %file.display(), "removed an orphaned golden clip");
                }
                Err(e) => {
                    report.errors += 1;
                    warn!(path = %file.display(), "could not remove an orphaned golden: {e}");
                }
            }
        }
        prune_empty_dirs(&data_dir.join("goldens"));
    }

    if cfg.vacuum_after_purge && (report.purged_rows > 0 || report.purged_empty > 0) {
        store.vacuum()?;
        report.vacuumed = true;
    }
    Ok(report)
}

/// What happened to one file the sweep tried to remove. "Was not there" and
/// "could not be removed" are different facts and only the second is a problem.
enum Unlinked {
    Removed,
    Absent,
    Failed,
}

fn unlink(data_dir: &Path, rel: &str) -> Unlinked {
    if rel.is_empty() {
        return Unlinked::Absent;
    }
    let path = data_dir.join(rel);
    match std::fs::remove_file(&path) {
        Ok(()) => Unlinked::Removed,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Unlinked::Absent,
        Err(e) => {
            warn!(path = %path.display(), "could not remove segment audio: {e}");
            Unlinked::Failed
        }
    }
}

/// Has this file been still long enough to be judged against the database?
///
/// A file we cannot stat is treated as settled: an unreadable file is not
/// evidence of a write in progress, and refusing to ever clean it up would
/// leave residue forever.
fn settled(path: &Path, before: std::time::SystemTime) -> bool {
    match std::fs::metadata(path).and_then(|m| m.modified()) {
        Ok(mtime) => mtime <= before,
        Err(_) => true,
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
    bus: Arc<crate::bus::Bus>,
    stop: Arc<SweeperStop>,
) {
    let interval = Duration::from_secs(cfg.sweep_interval_s.max(60));
    loop {
        let started_at = utc_now_ns();
        // What the sweep did, in a shape a client can see. Everything the
        // sweeper does was warn!/info! only — so the two failures it exists to
        // catch (a file destroyed mid-write, a golden that lost its clip) would
        // have recurred entirely inside the log (audit finding #22). Cached for
        // `status` and pushed as its own event, which is what makes #3 and #19
        // visible if they ever come back.
        let outcome = {
            let guard = store.lock().unwrap_or_else(|p| p.into_inner());
            match sweep(cfg, &guard, &data_dir, started_at) {
                Ok(report) => {
                    if report != SweepReport::default() {
                        info!(
                            purged_rows = report.purged_rows,
                            purged_empty = report.purged_empty,
                            aged_audio = report.aged_audio,
                            unlinked_files = report.unlinked_files,
                            orphan_files = report.orphan_files,
                            dangling_paths = report.dangling_paths,
                            orphan_goldens = report.orphan_goldens,
                            dangling_goldens = report.dangling_goldens,
                            pruned_threads = report.pruned_threads,
                            errors = report.errors,
                            "retention sweep"
                        );
                    } else {
                        debug!("retention sweep: nothing to do");
                    }
                    report.to_json(started_at)
                }
                Err(e) => {
                    warn!("retention sweep failed: {e:#}");
                    let mut failed = SweepReport {
                        errors: 1,
                        ..Default::default()
                    }
                    .to_json(started_at);
                    failed["failed"] = json!(format!("{e:#}"));
                    failed
                }
            }
        };
        control.set_last_sweep(outcome.clone());
        bus.publish(crate::bus::Topic::Status, "sweep", outcome);
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

    /// "Everything currently on disk has finished being written." The cutoff
    /// the reconciliation pass judges files against; a second in the future so
    /// a fixture written microseconds ago counts as settled whatever the
    /// filesystem's timestamp granularity is.
    fn settled_now() -> std::time::SystemTime {
        std::time::SystemTime::now() + Duration::from_secs(1)
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
        let report = sweep_at(&cfg, &r.store, &r.dir, now, settled_now()).unwrap();
        assert_eq!(report.orphan_files, 1);
        assert_eq!(report.dangling_paths, 1);
        assert_eq!(report.errors, 0);
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
        let report = sweep_at(&cfg, &r.store, &r.dir, now + DAY_NS, settled_now()).unwrap();
        assert_eq!(report.orphan_files, 0);
        assert!(r.dir.join("segments/000001/a.wav").exists());
    }

    /// Audit finding #3. The pipeline writes a segment's WAV **before** it
    /// inserts the row, and does it outside the store lock so audio never waits
    /// on a query. The sweeper snapshots the table and then unlinks every file
    /// the snapshot did not name — so a file finished in that gap used to be
    /// destroyed while it was being written, and the row that arrived a
    /// millisecond later pointed at nothing.
    #[test]
    fn a_file_written_during_the_sweep_is_not_mistaken_for_an_orphan() {
        let r = rig("mid-write");
        let s = session(&r);
        let now = 100 * DAY_NS;
        let settled = segment(&r, s, now, "segments/000001/settled.wav");

        // The interleaving, exactly: the sweep's view of "what has finished
        // being written" is taken, and only then does the pipeline finish a
        // file whose row does not exist yet.
        let cutoff = std::time::SystemTime::now();
        std::thread::sleep(Duration::from_millis(20));
        let in_flight = r.dir.join("segments/000001/in-flight.wav");
        std::fs::write(&in_flight, vec![0u8; 64]).unwrap();

        let cfg = RetentionConfig {
            undo_window_days: 0,
            audio_days: 0,
            ..Default::default()
        };
        let report = sweep_at(&cfg, &r.store, &r.dir, now, cutoff).unwrap();
        assert_eq!(
            report.orphan_files, 0,
            "a file younger than the sweep's cutoff is left alone"
        );
        assert!(
            in_flight.is_file(),
            "the segment being written must survive its own sweep"
        );

        // The row lands, and the next sweep — by which time the file has
        // settled — agrees it belongs.
        r.store
            .insert_segment(s, now, now + 1_000, "segments/000001/in-flight.wav", now)
            .unwrap();
        let report = sweep_at(&cfg, &r.store, &r.dir, now, settled_now()).unwrap();
        assert_eq!(report.orphan_files, 0);
        assert_eq!(report.dangling_paths, 0);
        assert!(in_flight.is_file());
        assert!(r.dir.join("segments/000001/settled.wav").exists());
        let _ = settled;

        // And the guard is a grace period, not an amnesty: a file that really
        // is an orphan goes as soon as it has stopped moving.
        std::fs::write(r.dir.join("segments/000001/junk.wav"), b"x").unwrap();
        let report = sweep_at(&cfg, &r.store, &r.dir, now, settled_now()).unwrap();
        assert_eq!(report.orphan_files, 1);
        assert!(!r.dir.join("segments/000001/junk.wav").exists());
    }

    /// Audit finding #19. Goldens are exempt from *retention* — they are what a
    /// future embedding model gets re-enrolled from — which was read as exempt
    /// from *reconciliation*, so nothing ever compared `goldens/` against the
    /// table in either direction.
    #[test]
    fn goldens_are_reconciled_in_both_directions_without_ever_ageing_out() {
        let r = rig("goldens");
        let now = 100 * DAY_NS;
        let spk = r.store.mint_speaker(0).unwrap();
        std::fs::create_dir_all(r.dir.join("goldens/000001")).unwrap();

        // One good golden, one row whose file went, one file no row claims.
        for name in ["kept.wav", "lost.wav", "stray.wav"] {
            std::fs::write(r.dir.join("goldens/000001").join(name), vec![0u8; 64]).unwrap();
        }
        r.store
            .add_golden_sample(spk, "goldens/000001/kept.wav", 4.0)
            .unwrap();
        let lost = r
            .store
            .add_golden_sample(spk, "goldens/000001/lost.wav", 3.0)
            .unwrap();
        std::fs::remove_file(r.dir.join("goldens/000001/lost.wav")).unwrap();

        let cfg = RetentionConfig {
            undo_window_days: 0,
            audio_days: 1,
            ..Default::default()
        };
        // A year in the future: if goldens were on the audio clock at all, this
        // is the sweep that would take them.
        let report = sweep_at(&cfg, &r.store, &r.dir, now + 400 * DAY_NS, settled_now()).unwrap();

        assert_eq!(report.orphan_goldens, 1, "the file no row claims goes");
        assert!(!r.dir.join("goldens/000001/stray.wav").exists());
        assert_eq!(report.dangling_goldens, 1, "the row with no file goes too");
        assert_eq!(report.errors, 0);

        let rows = r.store.golden_samples_for(spk).unwrap();
        assert_eq!(rows.len(), 1, "one golden left, and it is the whole one");
        assert_eq!(rows[0].audio_path, "goldens/000001/kept.wav");
        assert!(
            r.dir.join("goldens/000001/kept.wav").is_file(),
            "a golden with a row and a file never ages out, whatever the date"
        );
        assert!(!rows.iter().any(|g| g.id == lost));

        // Idempotent: with the two halves agreeing there is nothing to do.
        let again = sweep_at(&cfg, &r.store, &r.dir, now + 800 * DAY_NS, settled_now()).unwrap();
        assert_eq!(again.orphan_goldens, 0);
        assert_eq!(again.dangling_goldens, 0);
        assert!(r.dir.join("goldens/000001/kept.wav").is_file());
    }

    /// Audit finding #15. `prune_empty_threads` had no callers at all.
    ///
    /// `purge_segments` already dropped a thread whose every *row* was gone;
    /// what nothing covered is the thread left holding only rows that are no
    /// longer in any read path. It is not a conversation with nothing in it —
    /// it is not a conversation, and `thread.get` answering it with
    /// `segments: []` was the visible half of the same bug.
    #[test]
    fn a_conversation_whose_turns_all_left_the_read_paths_leaves_no_thread_behind() {
        let r = rig("threads");
        let s = session(&r);
        let now = 100 * DAY_NS;

        // One turn with words, deleted five minutes ago — still undoable, so
        // its row is still there and the thread cannot go on its account.
        let deleted = segment(&r, s, now, "segments/000001/gone.wav");
        r.store.soft_delete_segments(&[deleted], now).unwrap();
        // One turn with neither words nor a voice, old enough to expire: this
        // is the purge that empties the thread out.
        std::fs::write(r.dir.join("segments/000001/empty.wav"), vec![0u8; 64]).unwrap();
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

        let thread = r.store.create_thread(s, now - 40 * DAY_NS, now).unwrap();
        for seg in [deleted, empty] {
            r.store.set_segment_thread(seg, thread, now).unwrap();
        }
        assert!(r.store.thread_summary(thread).unwrap().is_some());

        let cfg = RetentionConfig {
            reconcile: false,
            ..Default::default()
        };
        let report = sweep(&cfg, &r.store, &r.dir, now).unwrap();
        assert_eq!(report.purged_empty, 1);
        assert_eq!(report.pruned_threads, 1);
        assert!(
            r.store.thread_summary(thread).unwrap().is_none(),
            "the row went with the last turn anybody could read"
        );
    }

    /// Audit finding #22. Everything the sweeper does was `warn!`/`info!` only,
    /// so the two failures it exists to catch would have recurred entirely
    /// inside the log. The report is cached for `status` and pushed as an
    /// event, which is what makes #3 and #19 visible if they ever come back.
    #[test]
    fn a_sweep_reports_itself_to_every_client_and_to_status() {
        let r = rig("last-sweep");
        let s = session(&r);
        let now = 100 * DAY_NS;
        let seg = segment(&r, s, now - 40 * DAY_NS, "segments/000001/old.wav");
        std::fs::write(r.dir.join("segments/000001/orphan.wav"), b"x").unwrap();
        let _ = seg;

        let report = sweep_at(
            &RetentionConfig::default(),
            &r.store,
            &r.dir,
            now,
            settled_now(),
        )
        .unwrap();
        let json = report.to_json(now);
        assert_eq!(
            json["started_at_utc_ns"],
            serde_json::json!(now.to_string())
        );
        assert_eq!(json["orphans_removed"], serde_json::json!(1));
        assert_eq!(json["aged_audio"], serde_json::json!(1));
        assert_eq!(json["errors"], serde_json::json!(0));
        assert!(json.get("dangling").is_some());
        assert!(json.get("purged").is_some());

        let control = crate::control::Control::new(
            r.dir.clone(),
            None,
            &crate::allowlist::Allowlist::from_rules([("x", false)]),
        );
        assert_eq!(
            control.last_sweep_json(),
            Value::Null,
            "never swept is null, not a block of zeroes claiming a clean sweep"
        );
        control.set_last_sweep(json.clone());
        assert_eq!(control.last_sweep_json(), json);
    }

    /// The sweeper thread's own contract: one pass, then a `sweep` event on the
    /// `status` topic, then it waits. Nobody had to be able to see a sweep
    /// before 0.7.5 and so nobody could.
    #[test]
    fn the_sweeper_publishes_what_it_did() {
        let r = rig("sweeper-event");
        let s = session(&r);
        let now = 100 * DAY_NS;
        segment(&r, s, now - 40 * DAY_NS, "segments/000001/old.wav");

        let store = Arc::new(std::sync::Mutex::new(Store::open(&r.dir).unwrap()));
        let control = crate::control::Control::new(
            r.dir.clone(),
            None,
            &crate::allowlist::Allowlist::from_rules([("x", false)]),
        );
        let bus = crate::bus::Bus::new(16, 16);
        let (client, rx) = bus.attach(None);
        client.subscribe(&[crate::bus::Topic::Status]);
        let stop = Arc::new(SweeperStop::default());

        let cfg = RetentionConfig::default();
        let handle = {
            let (store, control, bus, stop) = (
                Arc::clone(&store),
                Arc::clone(&control),
                Arc::clone(&bus),
                Arc::clone(&stop),
            );
            let dir = r.dir.clone();
            let cfg = cfg.clone();
            std::thread::spawn(move || run(&cfg, store, dir, None, control, bus, stop))
        };

        let mut seen = None;
        for _ in 0..200 {
            if let Ok(line) = rx.try_recv() {
                let ev: Value = serde_json::from_slice(&line).unwrap();
                if ev["ev"] == "sweep" {
                    seen = Some(ev);
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        stop.stop();
        handle.join().unwrap();

        let ev = seen.expect("the sweeper must announce what it did");
        assert_eq!(ev["data"]["aged_audio"], serde_json::json!(1));
        assert_eq!(ev["data"]["errors"], serde_json::json!(0));
        assert_eq!(
            control.last_sweep_json()["aged_audio"],
            serde_json::json!(1),
            "and `status` serves the same block it published"
        );
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
