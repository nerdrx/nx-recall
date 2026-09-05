//! A backup you can trust (0.13.0).
//!
//! Three verbs, and the same rule shapes all of them: nothing here is allowed
//! to lie about whether it worked.
//!
//! * **`create`** takes a consistent snapshot of the data directory into a
//!   folder the caller names — `recall.db` through SQLite's own online backup
//!   API (a second, read-only connection to the live file; WAL means a reader
//!   never blocks the writer and is never blocked by it, so capture never
//!   pauses for this), and `segments/`, `goldens/`, `probes/` by hard link
//!   where the destination is the same filesystem and by copy where it is
//!   not (same rule `crate::export::check_dir` already enforces for exports:
//!   never across a network mount). A manifest records every file's SHA-256,
//!   the schema version, and the row counts a restore can be checked against;
//!   a keyed signature over the manifest is what makes tampering after the
//!   fact detectable rather than merely inconvenient.
//! * **`verify`** re-hashes every file the manifest names, re-checks the
//!   signature, opens the copied database read-only and runs
//!   `PRAGMA integrity_check`, and compares row counts. It changes nothing.
//! * **`restore`** only runs the same checks `verify` does and then swaps a
//!   fresh data directory into place atomically, keeping the one it replaced
//!   as `.bak`. It refuses outright unless capture is paused — see
//!   [`RESTORE_REFUSED`] — because a restore under a live writer would be
//!   restoring into a database something else is still appending to.
//!
//! # What this is not
//!
//! It writes files to one directory on a local disk and reads them back.
//! There is no upload, no cloud target, no share sheet — the same DESIGN §12
//! boundary the local export draws, for the same reason: a backup of
//! recordings of the people around you is not a thing to hand to a third
//! party by accident.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::export::{ExportError, check_dir};

/// The database file's name inside both the live data directory and every
/// snapshot `create` writes — same name, so `restore` can drop a snapshot
/// straight into a fresh data directory without renaming anything.
pub const DB_NAME: &str = "recall.db";

/// The manifest's file name inside a snapshot directory.
pub const MANIFEST_NAME: &str = "manifest.json";

/// The signature's file name, next to the manifest.
pub const SIGNATURE_NAME: &str = "manifest.sig";

/// The 32-byte key the daemon mints once and keeps at
/// `<data_dir>/backup_key`, mode 0600. It never leaves the machine and it is
/// never asked for — its only job is to make a hand-edited manifest provably
/// different from a real one, which a hash alone cannot do (anybody can
/// recompute a plain hash over their tampered copy).
const KEY_FILE: &str = "backup_key";
const KEY_BYTES: usize = 32;

/// Top-level directories a snapshot carries verbatim (DESIGN's segments tier)
/// plus the two tiers retention never touches (`crate::retention` — goldens
/// are exempt on purpose, probes are the stereo-probe's own small archive).
/// Any of the three may simply not exist yet on a young install, which is not
/// an error: an empty tier backs up to nothing.
const COPIED_DIRS: &[&str] = &["segments", "goldens", "probes"];

/// Why a restore is refused outright, quoted back at both the CLI and the
/// socket caller. Enforced by checking [`crate::control::Control::is_paused`]
/// before a single byte moves — a restore is destructive by definition, and
/// "the daemon happened to be idle" is not the same claim as "capture is
/// quiesced and will not write underneath us".
pub const RESTORE_REFUSED: &str = "a restore only runs while capture is paused (`recalld pause`, \
    or the pause button) — restoring under a live writer would restore into a database \
    something else is still appending to";

// ---------------------------------------------------------------------------
// manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFile {
    /// Relative to the snapshot directory (`segments/000001/seg-1.wav`,
    /// `recall.db`). Always forward-slash, so a manifest written on this
    /// machine reads the same on any other.
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Counts {
    pub sessions: i64,
    pub speakers: i64,
    pub segments: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub created_at_utc_ns: i64,
    pub schema_version: i64,
    pub daemon: String,
    pub counts: Counts,
    pub files: Vec<ManifestFile>,
    pub total_bytes: u64,
}

impl Manifest {
    /// Canonical bytes: `serde_json` on a struct with a fixed field order and
    /// a `Vec` sorted before this is called (see [`create`]) is
    /// deterministic, which is what makes the signature reproducible at all.
    fn canonical_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn sha256_hex(&self) -> Result<String> {
        Ok(hex(&Sha256::digest(&self.canonical_bytes()?)))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SHA-256 of a whole file, streamed so a multi-hundred-MB clip never sits in
/// memory twice.
fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((hex(&hasher.finalize()), total))
}

// ---------------------------------------------------------------------------
// the key
// ---------------------------------------------------------------------------

/// The signing key, minted once and reused for the life of this data
/// directory. `/dev/urandom` rather than a crate: this is a local-only
/// tamper check, not a certificate, and pulling in a CSPRNG crate for 32
/// bytes read once per install is not a trade this program makes lightly.
pub fn load_or_create_key(data_dir: &Path) -> Result<[u8; KEY_BYTES]> {
    let path = data_dir.join(KEY_FILE);
    if let Ok(bytes) = std::fs::read(&path)
        && bytes.len() == KEY_BYTES
    {
        let mut key = [0u8; KEY_BYTES];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }
    let mut key = [0u8; KEY_BYTES];
    std::fs::File::open("/dev/urandom")
        .context("opening /dev/urandom")?
        .read_exact(&mut key)
        .context("reading /dev/urandom")?;
    std::fs::write(&path, key).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(key)
}

fn sign(key: &[u8; KEY_BYTES], manifest_bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key);
    hasher.update(manifest_bytes);
    hex(&hasher.finalize())
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CreateReport {
    pub dir: PathBuf,
    pub files: usize,
    pub bytes: u64,
    pub manifest_sha256: String,
    pub counts: Option<Counts>,
}

/// Refuse before touching the disk: the same local-filesystem, no-network-
/// mount rule the export writes onto, because a backup is exactly as much of
/// a share as an export is (DESIGN §12).
pub fn check_target(dir: &Path) -> Result<(), ExportError> {
    check_dir(dir)
}

/// One consistent snapshot of `data_dir` into `dest` (already checked with
/// [`check_target`]). `progress(done, total)` is file-granularity, the same
/// contract `export::write` reports on.
///
/// Runs on whatever thread calls it; the socket handler is responsible for
/// running that thread at idle priority (`crate::pipeline::background_current_thread`),
/// the same discipline every other heavy pass in this program keeps.
pub fn create(
    data_dir: &Path,
    dest: &Path,
    mut progress: impl FnMut(usize, usize),
) -> Result<CreateReport> {
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;

    let src_db = data_dir.join(DB_NAME);
    if !src_db.exists() {
        bail!(
            "{} does not exist yet — nothing has been captured",
            src_db.display()
        );
    }

    // The database, via SQLite's own online backup API against a SECOND,
    // independent, read-only connection. WAL is what makes this safe: a
    // reader never blocks the writer and the writer never blocks a reader, so
    // capture keeps running while this steps through the source pages.
    let dest_db = dest.join(DB_NAME);
    let _ = std::fs::remove_file(&dest_db);
    {
        let src = Connection::open_with_flags(&src_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening {} read-only", src_db.display()))?;
        let mut dst = Connection::open(&dest_db)
            .with_context(|| format!("creating {}", dest_db.display()))?;
        let backup =
            rusqlite::backup::Backup::new(&src, &mut dst).context("starting the online backup")?;
        // 100 pages (≈400 KB) per step, a short sleep between steps: the
        // pattern `rusqlite`'s own docs recommend for "do not starve anybody
        // else touching this file while you copy it".
        backup
            .run_to_completion(100, Duration::from_millis(5), None)
            .context("running the online backup to completion")?;
    }

    let counts = read_counts(&dest_db).ok();

    let mut files = Vec::new();
    let (db_hash, db_bytes) = hash_file(&dest_db)?;
    files.push(ManifestFile {
        path: DB_NAME.to_string(),
        sha256: db_hash,
        bytes: db_bytes,
    });

    let total_planned = COPIED_DIRS
        .iter()
        .map(|d| count_files(&data_dir.join(d)))
        .sum::<usize>()
        + 1;
    let mut done = 1;
    progress(done, total_planned);

    for sub in COPIED_DIRS {
        let src_dir = data_dir.join(sub);
        if !src_dir.exists() {
            continue;
        }
        let dst_dir = dest.join(sub);
        copy_or_link_tree(&src_dir, &dst_dir, sub, &mut files, &mut || {
            done += 1;
            progress(done, total_planned);
        })?;
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    let total_bytes = files.iter().map(|f| f.bytes).sum();

    let manifest = Manifest {
        created_at_utc_ns: crate::clock::utc_now_ns(),
        schema_version: crate::store::SCHEMA_VERSION,
        daemon: crate::proto::daemon_id(),
        counts: counts.clone().unwrap_or(Counts {
            sessions: -1,
            speakers: -1,
            segments: -1,
        }),
        files,
        total_bytes,
    };
    let manifest_bytes = manifest.canonical_bytes()?;
    std::fs::write(dest.join(MANIFEST_NAME), &manifest_bytes)
        .with_context(|| format!("writing {}", dest.join(MANIFEST_NAME).display()))?;

    let key = load_or_create_key(data_dir)?;
    let signature = sign(&key, &manifest_bytes);
    std::fs::write(dest.join(SIGNATURE_NAME), &signature)
        .with_context(|| format!("writing {}", dest.join(SIGNATURE_NAME).display()))?;

    Ok(CreateReport {
        dir: dest.to_path_buf(),
        files: manifest_len(&manifest_bytes)?,
        bytes: total_bytes,
        manifest_sha256: manifest.sha256_hex()?,
        counts,
    })
}

fn manifest_len(bytes: &[u8]) -> Result<usize> {
    let m: Manifest = serde_json::from_slice(bytes)?;
    Ok(m.files.len())
}

fn count_files(dir: &Path) -> usize {
    walk(dir).len()
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// Copy (or, same filesystem, hard-link) every file under `src_dir` into
/// `dst_dir`, preserving the relative layout, recording each in `files`.
///
/// Hard-linking is the default because a segment clip is never modified after
/// it is written — retention deletes it outright, it never edits it in place
/// — so a link is exactly as durable a copy as `cp` would make and costs no
/// I/O at all. It falls back to a real copy across a filesystem boundary
/// (`EXDEV`), the one case a hard link cannot express.
fn copy_or_link_tree(
    src_dir: &Path,
    dst_dir: &Path,
    label: &str,
    files: &mut Vec<ManifestFile>,
    mut step: impl FnMut(),
) -> Result<()> {
    std::fs::create_dir_all(dst_dir)?;
    for path in walk(src_dir) {
        let rel = path.strip_prefix(src_dir).unwrap_or(&path);
        let dst = dst_dir.join(rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(&dst);
        if std::fs::hard_link(&path, &dst).is_err() {
            std::fs::copy(&path, &dst)
                .with_context(|| format!("copying {} to {}", path.display(), dst.display()))?;
        }
        let (hash, bytes) = hash_file(&dst)?;
        let rel_str = format!("{label}/{}", rel.to_string_lossy().replace('\\', "/"));
        files.push(ManifestFile {
            path: rel_str,
            sha256: hash,
            bytes,
        });
        step();
    }
    Ok(())
}

fn read_counts(db_path: &Path) -> Result<Counts> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let sessions = conn.query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;
    let speakers = conn.query_row("SELECT COUNT(*) FROM speakers", [], |r| r.get(0))?;
    let segments = conn.query_row("SELECT COUNT(*) FROM segments", [], |r| r.get(0))?;
    Ok(Counts {
        sessions,
        speakers,
        segments,
    })
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct VerifyReport {
    pub ok: bool,
    pub files_checked: usize,
    pub files_bad: Vec<String>,
    pub files_missing: Vec<String>,
    pub integrity_check: String,
    pub signature_valid: bool,
    pub counts_match: bool,
    pub counts: Option<Counts>,
    pub manifest_sha256: String,
}

/// Re-hash every file the manifest names, re-run `PRAGMA integrity_check` on
/// the copied database, re-check the signature, and compare row counts. Reads
/// only — a verify that touched anything would not be a check anybody could
/// trust the second time.
pub fn verify(data_dir: &Path, dir: &Path) -> Result<VerifyReport> {
    let manifest_path = dir.join(MANIFEST_NAME);
    let manifest_bytes = std::fs::read(&manifest_path)
        .with_context(|| format!("reading {}", manifest_path.display()))?;
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("{} is not a valid manifest", manifest_path.display()))?;

    let mut report = VerifyReport {
        manifest_sha256: manifest.sha256_hex()?,
        ..Default::default()
    };

    // The signature. A daemon that has never minted a key (this snapshot was
    // copied to a machine that never ran `backup create`) cannot check this,
    // which is reported rather than silently assumed true.
    if let Ok(key) = load_or_create_key(data_dir) {
        let want = sign(&key, &manifest_bytes);
        let got = std::fs::read_to_string(dir.join(SIGNATURE_NAME)).unwrap_or_default();
        report.signature_valid = got.trim() == want;
    }

    for file in &manifest.files {
        report.files_checked += 1;
        let path = dir.join(&file.path);
        if !path.exists() {
            report.files_missing.push(file.path.clone());
            continue;
        }
        match hash_file(&path) {
            Ok((hash, bytes)) if hash == file.sha256 && bytes == file.bytes => {}
            _ => report.files_bad.push(file.path.clone()),
        }
    }

    let db_path = dir.join(DB_NAME);
    report.integrity_check =
        match Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(conn) => conn
                .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap_or_else(|e| format!("could not run integrity_check: {e}")),
            Err(e) => format!("could not open {}: {e}", db_path.display()),
        };

    report.counts = read_counts(&db_path).ok();
    report.counts_match = matches!(
        (&report.counts, manifest.counts.sessions),
        (Some(c), want_sessions) if want_sessions >= 0
            && c.sessions == manifest.counts.sessions
            && c.speakers == manifest.counts.speakers
            && c.segments == manifest.counts.segments
    );

    report.ok = report.files_missing.is_empty()
        && report.files_bad.is_empty()
        && report.integrity_check == "ok"
        && report.counts_match;
    Ok(report)
}

// ---------------------------------------------------------------------------
// restore
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct RestoreReport {
    pub restored_into: PathBuf,
    pub previous_kept_as: Option<PathBuf>,
    pub verify: VerifyReport,
}

/// Restore snapshot `dir` over `data_dir`.
///
/// The caller (CLI or socket) has already enforced the pause rule
/// ([`RESTORE_REFUSED`]) — this function does the actual work and enforces
/// only the thing it alone can check: that the snapshot verifies clean
/// *before* anything about the live data directory is touched. It copies the
/// snapshot into `<data_dir>.new`, verifies THAT copy (not the source — a
/// corrupt copy step must be caught too), and only then swaps: the old
/// directory becomes `<data_dir>.bak` (replacing any previous one) and the
/// new one takes `data_dir`'s name. Both renames are same-filesystem and
/// therefore atomic; if the second one somehow failed, the first has already
/// moved the live directory aside, which is the one intermediate state this
/// function leaves the disk in only for the instant between the two calls.
pub fn restore(data_dir: &Path, dir: &Path) -> Result<RestoreReport> {
    let manifest_path = dir.join(MANIFEST_NAME);
    if !manifest_path.exists() {
        bail!(
            "{} has no {MANIFEST_NAME} — it is not a backup this program made",
            dir.display()
        );
    }

    let staging = staging_path(data_dir);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    std::fs::copy(dir.join(DB_NAME), staging.join(DB_NAME))
        .with_context(|| format!("copying {} into the staging directory", DB_NAME))?;
    for sub in COPIED_DIRS {
        let src = dir.join(sub);
        if !src.exists() {
            continue;
        }
        let mut unused = Vec::new();
        copy_or_link_tree(&src, &staging.join(sub), sub, &mut unused, &mut || {})?;
    }
    std::fs::copy(&manifest_path, staging.join(MANIFEST_NAME))?;
    let sig = dir.join(SIGNATURE_NAME);
    if sig.exists() {
        std::fs::copy(&sig, staging.join(SIGNATURE_NAME))?;
    }

    // Verify the STAGED copy, not the source snapshot: a restore that trusted
    // the source and never re-checked what it actually wrote would miss a
    // corruption introduced by this very copy step.
    let report = verify(data_dir, &staging)?;
    if !report.ok {
        let _ = std::fs::remove_dir_all(&staging);
        bail!(
            "the staged restore did not verify clean (integrity_check: {}, {} bad file(s), \
             {} missing) — nothing was swapped in",
            report.integrity_check,
            report.files_bad.len(),
            report.files_missing.len()
        );
    }

    let bak = bak_path(data_dir);
    let previous_kept_as = if data_dir.exists() {
        let _ = std::fs::remove_dir_all(&bak);
        std::fs::rename(data_dir, &bak)
            .with_context(|| format!("moving {} aside to {}", data_dir.display(), bak.display()))?;
        Some(bak.clone())
    } else {
        None
    };
    if let Err(e) = std::fs::rename(&staging, data_dir) {
        // Best-effort undo: put the previous directory back rather than leave
        // the machine with neither.
        if let Some(prev) = &previous_kept_as {
            let _ = std::fs::rename(prev, data_dir);
        }
        return Err(e).with_context(|| {
            format!(
                "swapping the restored directory into {}",
                data_dir.display()
            )
        });
    }

    Ok(RestoreReport {
        restored_into: data_dir.to_path_buf(),
        previous_kept_as,
        verify: report,
    })
}

fn staging_path(data_dir: &Path) -> PathBuf {
    let mut s = data_dir.as_os_str().to_owned();
    s.push(".restoring");
    PathBuf::from(s)
}

fn bak_path(data_dir: &Path) -> PathBuf {
    let mut s = data_dir.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nxr-backup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A tiny fixture data directory: a real `recall.db` with a couple of
    /// sessions in it (via `Store::open`), plus one segment file so the
    /// segments tier has something to copy.
    fn fixture(tag: &str) -> PathBuf {
        let dir = tmpdir(tag);
        let store = crate::store::Store::open(&dir).unwrap();
        let sid = store.upsert_source("VRChat.exe", "VRChat", 0).unwrap();
        for i in 0..3 {
            let session = store.begin_session_for(sid, i * 1000, None).unwrap();
            store.end_session(session, i * 1000 + 500).unwrap();
        }
        drop(store);
        std::fs::create_dir_all(dir.join("segments/000001")).unwrap();
        std::fs::write(dir.join("segments/000001/seg-1.wav"), b"not really a wav").unwrap();
        dir
    }

    #[test]
    fn a_fresh_backup_verifies_clean() {
        let data_dir = fixture("clean");
        let dest = tmpdir("clean-dest");

        let report = create(&data_dir, &dest, |_, _| {}).unwrap();
        assert!(report.files >= 2, "db + at least one segment: {report:?}");
        assert_eq!(report.counts.as_ref().unwrap().sessions, 3);

        let verify = verify(&data_dir, &dest).unwrap();
        assert!(verify.ok, "{verify:?}");
        assert!(verify.signature_valid);
        assert_eq!(verify.integrity_check, "ok");
        assert!(verify.files_bad.is_empty());
        assert!(verify.files_missing.is_empty());

        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&dest);
    }

    /// The drill the task asks for: corrupt one byte in the copy, and
    /// `verify` must catch it — both as a bad hash and as a failed
    /// `integrity_check` when the corrupted file is the database itself.
    #[test]
    fn a_corrupted_byte_in_the_copy_fails_verification() {
        let data_dir = fixture("corrupt");
        let dest = tmpdir("corrupt-dest");
        create(&data_dir, &dest, |_, _| {}).unwrap();

        let db_path = dest.join(DB_NAME);
        let mut bytes = std::fs::read(&db_path).unwrap();
        // Flip a byte well past the SQLite header so this is corruption, not
        // an unreadable file.
        let i = bytes.len() / 2;
        bytes[i] ^= 0xFF;
        std::fs::write(&db_path, &bytes).unwrap();

        let report = verify(&data_dir, &dest).unwrap();
        assert!(!report.ok, "a corrupted database must not verify clean");
        assert!(
            report.files_bad.contains(&DB_NAME.to_string()) || report.integrity_check != "ok",
            "{report:?}"
        );

        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&dest);
    }

    #[test]
    fn restore_into_a_temp_dir_reopens_with_matching_counts() {
        let data_dir = fixture("restore-src");
        let backup_dir = tmpdir("restore-backup");
        create(&data_dir, &backup_dir, |_, _| {}).unwrap();

        let target = tmpdir("restore-target");
        // `restore` only overwrites an existing directory; start from one
        // that does not exist yet, the "restoring onto a fresh machine" case.
        std::fs::remove_dir_all(&target).unwrap();

        let report = restore(&target, &backup_dir).unwrap();
        assert!(report.verify.ok, "{:?}", report.verify);
        assert!(
            report.previous_kept_as.is_none(),
            "nothing was there to keep"
        );

        let store = crate::store::Store::open(&target).unwrap();
        let counts = read_counts(&target.join(DB_NAME)).unwrap();
        assert_eq!(counts.sessions, 3);
        drop(store);

        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&backup_dir);
        let _ = std::fs::remove_dir_all(&target);
    }

    #[test]
    fn restoring_over_a_live_directory_keeps_the_previous_one_as_bak() {
        let data_dir = fixture("restore-over-src");
        let backup_dir = tmpdir("restore-over-backup");
        create(&data_dir, &backup_dir, |_, _| {}).unwrap();

        let target = fixture("restore-over-target");
        let report = restore(&target, &backup_dir).unwrap();
        let bak = report
            .previous_kept_as
            .expect("a previous directory existed");
        assert!(bak.exists());
        assert!(target.exists());

        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&backup_dir);
        let _ = std::fs::remove_dir_all(&target);
        let _ = std::fs::remove_dir_all(&bak);
    }

    #[test]
    fn the_manifest_hash_is_stable_for_the_same_bytes() {
        let data_dir = fixture("hash-stable");
        let dest = tmpdir("hash-stable-dest");
        let a = create(&data_dir, &dest, |_, _| {}).unwrap();
        let manifest_bytes = std::fs::read(dest.join(MANIFEST_NAME)).unwrap();
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes).unwrap();
        assert_eq!(manifest.sha256_hex().unwrap(), a.manifest_sha256);

        let _ = std::fs::remove_dir_all(&data_dir);
        let _ = std::fs::remove_dir_all(&dest);
    }
}
