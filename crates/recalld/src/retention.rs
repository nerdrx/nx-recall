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

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::clock::utc_now_ns;
use crate::config::RetentionConfig;
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
    pub vacuumed: bool,
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

    if cfg.vacuum_after_purge && report.purged_rows > 0 {
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
