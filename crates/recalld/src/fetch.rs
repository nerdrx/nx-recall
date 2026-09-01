//! `recalld models fetch` — the one place in this program that opens a socket
//! to the internet.
//!
//! DESIGN §4/§10: the analysis models are downloaded on first run into the data
//! dir, and nothing is ever fetched at inference time. Keeping the fetch in its
//! own module and its own subcommand is what makes that auditable: no other
//! module imports `ureq`, and `recalld run` never calls anything in here.
//!
//! The catalogue (URLs, sizes, layout) lives in [`crate::models`], shared with
//! `models status`, so "present" means the same thing to both commands.
//!
//! Everything here is idempotent. A file that is already on disk at exactly the
//! catalogued size is left alone; a partial download is a `.part` file that is
//! never renamed into place; an archive is verified before it is opened.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::ModelsConfig;
use crate::models::{
    EntryState, Install, ModelSet, REMOTE_ASSETS, RemoteAsset, SEMANTIC_ROLE,
    SEMANTIC_TOKENIZER_ROLE, SemanticModel,
};

/// Read timeout for a single chunk. The whole download has no deadline — a
/// 130 MB model on a slow line is not an error — but a stalled connection is.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

pub struct FetchOptions {
    /// Re-download everything, even files that are already the right size.
    pub force: bool,
    /// Also install the optional English-only ASR export. Off by default: the
    /// multilingual set beats it at English too, and it only exists here so a
    /// machine that wants the small model can still get it.
    pub fallback_asr: bool,
    /// Also install the optional text-embedding model for semantic search
    /// (135 MB). Off by default: keyword search works without it, and this is
    /// the one asset that buys a *feature* rather than correctness.
    pub semantic: bool,
}

impl FetchOptions {
    /// Is this non-default asset one the caller asked for?
    fn wants(&self, asset: &RemoteAsset) -> bool {
        match asset.role {
            "asr-fallback" => self.fallback_asr,
            SEMANTIC_ROLE | SEMANTIC_TOKENIZER_ROLE => self.semantic,
            _ => false,
        }
    }
}

#[derive(Debug, Default)]
pub struct FetchReport {
    pub downloaded: usize,
    pub skipped: usize,
    pub bytes: u64,
}

/// Bring `root` up to the full default model set.
pub fn fetch_models(root: &Path, cfg: &ModelsConfig, opts: &FetchOptions) -> Result<FetchReport> {
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;

    let mut report = FetchReport::default();
    for asset in REMOTE_ASSETS {
        // An asset already on disk at the catalogued size is reported whether
        // it is part of the default set or not — the old English-only export
        // does not vanish from an existing install just because it stopped
        // being the default.
        if !opts.force && asset_satisfied(root, asset) {
            println!(
                "  {:<13} present  ({})",
                asset.role,
                human(asset_installed_bytes(root, asset))
            );
            report.skipped += 1;
            continue;
        }
        if !asset.default && !opts.wants(asset) {
            continue;
        }
        let n = fetch_one(root, asset)?;
        report.downloaded += 1;
        report.bytes += n;
    }

    // The set is only useful if the daemon can resolve it through the same
    // config it will run with, so verify through `ModelSet` — including the
    // ASR selection the daemon itself will make — not through the catalogue we
    // just wrote.
    let mut set = ModelSet::resolve_at(root.to_path_buf(), cfg);
    set.select_asr();
    let mut missing = set.missing();
    // The semantic model is verified only when it was asked for: it is not part
    // of `ModelSet` precisely because its absence must never read as a broken
    // install (see `models::SemanticModel`).
    if opts.semantic {
        let sem = SemanticModel::resolve_at(root.to_path_buf(), cfg);
        missing.extend(sem.entries().into_iter().filter(|e| !e.ok()));
    }
    if !missing.is_empty() {
        eprintln!();
        for e in &missing {
            eprintln!("  ! {:<13} {}", e.role, describe(e.state(), &e.path));
        }
        bail!(
            "{} model file(s) are still not usable after the fetch — \
             `[models]` in config.toml may point at names this catalogue does not publish",
            missing.len()
        );
    }
    Ok(report)
}

/// Every file this asset installs is on disk at exactly the catalogued size.
fn asset_satisfied(root: &Path, asset: &RemoteAsset) -> bool {
    asset
        .files
        .iter()
        .all(|(rel, want)| fs::metadata(root.join(rel)).map(|m| m.len()).ok() == Some(*want))
}

fn asset_installed_bytes(root: &Path, asset: &RemoteAsset) -> u64 {
    asset
        .files
        .iter()
        .map(|(rel, _)| fs::metadata(root.join(rel)).map(|m| m.len()).unwrap_or(0))
        .sum()
}

fn fetch_one(root: &Path, asset: &RemoteAsset) -> Result<u64> {
    let part = root.join(format!("{}.part", asset.file_name()));
    let _ = fs::remove_file(&part);

    let got = download(asset, &part)?;
    if got != asset.download_bytes {
        let _ = fs::remove_file(&part);
        bail!(
            "{}: downloaded {} bytes, expected exactly {} — refusing to install it \
             (the upstream asset changed, or the transfer was truncated)",
            asset.file_name(),
            got,
            asset.download_bytes
        );
    }

    match asset.install {
        Install::File(dest_rel) => {
            let dest = root.join(dest_rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            // Rename, not copy: the file only ever appears at its final path
            // once it is whole and verified.
            fs::rename(&part, &dest).with_context(|| format!("installing {}", dest.display()))?;
        }
        Install::TarBz2 => {
            unpack_tar_bz2(&part, root)
                .with_context(|| format!("unpacking {}", asset.file_name()))?;
            fs::remove_file(&part).ok();
        }
    }

    for (rel, want) in asset.files {
        let path = root.join(rel);
        let found = fs::metadata(&path)
            .map(|m| m.len())
            .with_context(|| format!("{} did not appear", path.display()))?;
        if found != *want {
            bail!(
                "{}: {} bytes on disk, catalogue says {}",
                path.display(),
                found,
                want
            );
        }
    }
    println!("  {:<13} ok", asset.role);
    Ok(got)
}

/// GET the asset into `dest`, drawing a progress line on stderr.
///
/// stderr on purpose: stdout carries the machine-ish report of what was
/// installed, and a `\r`-redrawn bar has no business in it.
fn download(asset: &RemoteAsset, dest: &Path) -> Result<u64> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .user_agent(concat!("nx-recall/", env!("CARGO_PKG_VERSION")))
        .build();

    let resp = agent
        .get(asset.url)
        .call()
        .with_context(|| format!("GET {}", asset.url))?;

    let total = resp
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(asset.download_bytes);

    let mut reader = resp.into_reader();
    let mut file = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut bar = Progress::new(asset.role, total);
    let mut buf = vec![0u8; 256 * 1024];
    let mut done: u64 = 0;

    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("reading {}", asset.url))?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .with_context(|| format!("writing {}", dest.display()))?;
        done += n as u64;
        bar.update(done);
    }
    file.sync_all().ok();
    bar.finish(done);
    Ok(done)
}

fn unpack_tar_bz2(archive: &Path, root: &Path) -> Result<()> {
    let file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let dec = bzip2::read::BzDecoder::new(io::BufReader::new(file));
    let mut tar = tar::Archive::new(dec);
    tar.set_overwrite(true);
    tar.unpack(root)
        .with_context(|| format!("unpacking into {}", root.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// progress
// ---------------------------------------------------------------------------

struct Progress {
    role: &'static str,
    total: u64,
    started: Instant,
    last_draw: Instant,
    tty: bool,
}

impl Progress {
    fn new(role: &'static str, total: u64) -> Self {
        // SAFETY: isatty only inspects the descriptor.
        let tty = unsafe { libc::isatty(libc::STDERR_FILENO) } == 1;
        let now = Instant::now();
        let p = Self {
            role,
            total,
            started: now,
            // Force the first draw.
            last_draw: now - Duration::from_secs(60),
            tty,
        };
        eprintln!("  {:<13} {} to download", role, human(total));
        p
    }

    fn update(&mut self, done: u64) {
        // A redraw every 200 ms on a terminal; a line every 5 s in a log, so a
        // systemd journal gets progress without getting a thousand lines of it.
        let interval = if self.tty {
            Duration::from_millis(200)
        } else {
            Duration::from_secs(5)
        };
        if self.last_draw.elapsed() < interval {
            return;
        }
        self.last_draw = Instant::now();
        self.draw(done, false);
    }

    fn finish(&mut self, done: u64) {
        self.draw(done, true);
    }

    fn draw(&self, done: u64, final_: bool) {
        let secs = self.started.elapsed().as_secs_f64().max(0.001);
        let rate = done as f64 / secs;
        let pct = if self.total > 0 {
            (done as f64 / self.total as f64 * 100.0).min(100.0)
        } else {
            0.0
        };
        let line = format!(
            "  {:<13} {:>5.1}%  {} / {}  {}/s",
            self.role,
            pct,
            human(done),
            human(self.total),
            human(rate as u64)
        );
        let mut err = io::stderr();
        if self.tty {
            let _ = write!(err, "\r{line}\x1b[K");
            if final_ {
                let _ = writeln!(err);
            }
        } else {
            let _ = writeln!(err, "{line}");
        }
        let _ = err.flush();
    }
}

pub fn human(bytes: u64) -> String {
    const MB: f64 = 1_048_576.0;
    const GB: f64 = 1_073_741_824.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else {
        format!("{:.0} KB", b / 1024.0)
    }
}

pub fn describe(state: EntryState, path: &Path) -> String {
    match state {
        EntryState::Ok => format!("ok  {}", path.display()),
        EntryState::Missing => format!("missing  {}", path.display()),
        EntryState::WrongSize { found, expected } => format!(
            "wrong size: {found} bytes, expected {expected}  {}",
            path.display()
        ),
    }
}

/// The models directory a fetch or a status should act on, in precedence order:
/// an explicit `--dir`, then `[models].dir`, then `<data-dir>/models`.
pub fn target_dir(explicit: Option<&Path>, cfg: &ModelsConfig, data_dir: &Path) -> PathBuf {
    if let Some(p) = explicit {
        return p.to_path_buf();
    }
    if let Some(p) = &cfg.dir {
        return p.clone();
    }
    crate::models::default_dir(data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{expected_bytes, total_download_bytes};

    /// Two publishers, and only two. Everything the analysis leg needs comes
    /// from k2-fsa's release assets; the optional semantic model is the one
    /// exception, and it is pinned to a commit rather than to `main` so that
    /// "the catalogued size" cannot change under us.
    #[test]
    fn every_catalogued_url_comes_from_a_publisher_we_named() {
        for a in REMOTE_ASSETS {
            let sherpa = a
                .url
                .starts_with("https://github.com/k2-fsa/sherpa-onnx/releases/download/");
            let semantic = a.role == SEMANTIC_ROLE || a.role == SEMANTIC_TOKENIZER_ROLE;
            if semantic {
                assert!(
                    a.url.starts_with(
                        "https://huggingface.co/Xenova/multilingual-e5-small/resolve/"
                    ),
                    "{} points somewhere unexpected: {}",
                    a.role,
                    a.url
                );
                assert!(
                    !a.url.contains("/resolve/main/"),
                    "{} must be pinned to a commit, not to a branch: {}",
                    a.role,
                    a.url
                );
                assert!(!a.default, "semantic search is opt-in");
            } else {
                assert!(sherpa, "{} points somewhere unexpected: {}", a.role, a.url);
            }
            assert!(a.download_bytes > 0);
            assert!(!a.files.is_empty());
        }
    }

    /// The two optional legs are independent switches: a bare fetch installs
    /// neither, and asking for one must not drag in the other.
    #[test]
    fn the_optional_assets_are_each_behind_their_own_flag() {
        let bare = FetchOptions {
            force: false,
            fallback_asr: false,
            semantic: false,
        };
        let sem = FetchOptions {
            semantic: true,
            ..bare
        };
        let en = FetchOptions {
            fallback_asr: true,
            ..bare
        };
        for a in REMOTE_ASSETS.iter().filter(|a| !a.default) {
            assert!(!bare.wants(a), "{} is not part of a bare fetch", a.role);
        }
        let semantic_assets: Vec<&RemoteAsset> = REMOTE_ASSETS
            .iter()
            .filter(|a| a.role == SEMANTIC_ROLE || a.role == SEMANTIC_TOKENIZER_ROLE)
            .collect();
        assert_eq!(semantic_assets.len(), 2, "model and tokenizer, both needed");
        for a in &semantic_assets {
            assert!(sem.wants(a));
            assert!(!en.wants(a));
        }
        let fb = REMOTE_ASSETS
            .iter()
            .find(|a| a.role == "asr-fallback")
            .unwrap();
        assert!(en.wants(fb));
        assert!(!sem.wants(fb));
    }

    #[test]
    fn the_semantic_model_is_catalogued_at_its_exact_size() {
        assert_eq!(
            expected_bytes("multilingual-e5-small-int8/model.onnx"),
            Some(118_308_185)
        );
        assert_eq!(
            expected_bytes("multilingual-e5-small-int8/tokenizer.json"),
            Some(17_082_730)
        );
        assert_eq!(
            crate::models::semantic_download_bytes(),
            118_308_185 + 17_082_730
        );
        // ...and it is NOT in the default set's budget, either way round.
        assert!(total_download_bytes(true) < 700_000_000);
    }

    /// The v3 entry, transcribed from the release asset and the unpacked files.
    /// Every number here is checked by the fetch itself, so a re-upload upstream
    /// fails the download rather than installing a different model under the
    /// same name — and this test is what stops the numbers drifting silently.
    #[test]
    fn the_default_asr_is_the_multilingual_export() {
        let a = REMOTE_ASSETS.iter().find(|a| a.role == "asr").unwrap();
        assert!(a.default);
        assert_eq!(
            a.url,
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/\
             sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2"
        );
        assert_eq!(a.download_bytes, 487_170_055);
        assert_eq!(a.install, Install::TarBz2);
        assert_eq!(
            a.files,
            &[
                (
                    "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/encoder.int8.onnx",
                    652_184_281
                ),
                (
                    "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/decoder.int8.onnx",
                    11_845_275
                ),
                (
                    "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/joiner.int8.onnx",
                    6_355_277
                ),
                (
                    "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/tokens.txt",
                    93_939
                ),
            ]
        );
    }

    /// The English-only export is not fetched by default any more, but it is
    /// still catalogued: `models status` needs its sizes to report a fallback
    /// install, and `--fallback-asr` needs its URL.
    #[test]
    fn the_english_only_export_stays_catalogued_but_is_not_default() {
        let a = REMOTE_ASSETS
            .iter()
            .find(|a| a.role == "asr-fallback")
            .unwrap();
        assert!(!a.default);
        assert_eq!(a.download_bytes, 108_035_095);
        assert_eq!(
            expected_bytes(
                "sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8/encoder.int8.onnx"
            ),
            Some(131_113_202)
        );
        // Optional, but not the only optional thing any more: semantic search
        // adds two, and this assertion is about the ASR leg.
        assert_eq!(
            REMOTE_ASSETS
                .iter()
                .filter(|a| !a.default && a.role.starts_with("asr"))
                .count(),
            1
        );
    }

    #[test]
    fn the_catalogue_covers_every_file_the_daemon_resolves() {
        let cfg = ModelsConfig::default();
        let set = ModelSet::resolve_at(PathBuf::from("/models"), &cfg);
        for e in set.entries() {
            assert!(
                e.expected.is_some(),
                "{} ({}) has no catalogued size — status and fetch would disagree",
                e.role,
                e.path.display()
            );
        }
    }

    #[test]
    fn the_default_set_is_the_documented_size() {
        // DESIGN §4's "~700 MB default set" on disk, now literally true: the
        // multilingual encoder alone unpacks to 622 MB. ~496 MB compressed.
        assert_eq!(
            total_download_bytes(false),
            6_958_444 + 26_485_263 + 487_170_055
        );
        // The optional English-only export is only counted when it is asked for.
        assert_eq!(
            total_download_bytes(true),
            total_download_bytes(false) + 108_035_095
        );
        assert_eq!(expected_bytes("eres2net_en.onnx"), Some(26_485_263));
        assert_eq!(expected_bytes("no/such/model.onnx"), None);
    }

    #[test]
    fn the_embedding_is_installed_under_the_name_the_model_id_uses() {
        let a = REMOTE_ASSETS
            .iter()
            .find(|a| a.role == "embedding")
            .unwrap();
        assert_eq!(a.install, Install::File("eres2net_en.onnx"));
        // The stored embed_model_id is derived from the file stem, so renaming
        // the download is load-bearing, not cosmetic.
        let set = ModelSet::resolve_at(PathBuf::from("/models"), &ModelsConfig::default());
        assert_eq!(set.embed_model_id(), "eres2net_en@1");
    }

    #[test]
    fn file_names_come_off_the_url() {
        let a = REMOTE_ASSETS.iter().find(|a| a.role == "asr").unwrap();
        assert_eq!(
            a.file_name(),
            "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2"
        );
    }

    #[test]
    fn an_already_correct_directory_needs_no_download() {
        let dir = std::env::temp_dir().join(format!("nxr-fetch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let a = REMOTE_ASSETS
            .iter()
            .find(|a| a.role == "embedding")
            .unwrap();
        assert!(!asset_satisfied(&dir, a));

        // A file of the right name but the wrong length is not "present".
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("eres2net_en.onnx"), b"not a model").unwrap();
        assert!(!asset_satisfied(&dir, a));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_target_directory_falls_back_to_the_data_dir() {
        let cfg = ModelsConfig::default();
        assert_eq!(
            target_dir(None, &cfg, Path::new("/data")),
            PathBuf::from("/data/models")
        );
        let configured = ModelsConfig {
            dir: Some(PathBuf::from("/srv/models")),
            ..Default::default()
        };
        assert_eq!(
            target_dir(None, &configured, Path::new("/data")),
            PathBuf::from("/srv/models")
        );
        assert_eq!(
            target_dir(Some(Path::new("/tmp/m")), &configured, Path::new("/data")),
            PathBuf::from("/tmp/m")
        );
    }
}
