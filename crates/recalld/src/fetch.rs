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
//!
//! ## Why this downloads in twelve pieces
//!
//! Measured on the dev machine's own line, 2026-09-01: **50 kB/s on a single
//! connection against ~13 MB/s across twelve ranged ones.** A 620 MB model took
//! hours as one stream. The line is not slow — it is *shaped per connection*,
//! which is common on consumer uplinks and invisible to any speed test that
//! opens more than one socket.
//!
//! So a large asset is split into ranges, fetched concurrently, and written
//! into one preallocated file at the right offsets. Three properties are
//! load-bearing and each has a test:
//!
//! - **The verification does not change.** The completed file must still weigh
//!   exactly what the catalogue says, byte for byte, and every installed file
//!   is size-checked afterwards exactly as before.
//! - **It resumes.** Each range's progress is journalled next to the `.part`
//!   file, so a fetch interrupted at 90% of two gigabytes restarts at 90%.
//! - **It falls back.** A server that does not answer `Range` with a `206` gets
//!   the old single stream, unchanged. Parallelism is an optimisation, never a
//!   requirement, and a mirror that cannot do it must still work.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::ModelsConfig;
use crate::models::{
    EntryState, Group, Install, ModelEntry, ModelSet, REMOTE_ASSETS, RemoteAsset, SemanticModel,
};

/// Read timeout for a single chunk. The whole download has no deadline — a
/// 1.9 GB model on a slow line is not an error — but a stalled connection is.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Ranged connections a large asset is split across. Twelve is what the
/// measurement above used; past about a dozen the per-connection shaping stops
/// being the bottleneck and a host starts looking at you strangely.
pub const CONNECTIONS: usize = 12;

/// Below this, one stream wins: twelve TLS handshakes cost more than the file.
pub const MIN_PARALLEL_BYTES: u64 = 8 * 1024 * 1024;

/// Attempts per range before the whole download gives up. A dropped connection
/// mid-file is ordinary on the line this exists for, and each retry resumes
/// from what that range has already written rather than from its start.
const MAX_ATTEMPTS: usize = 5;

const BUF: usize = 256 * 1024;

#[derive(Debug, Default)]
pub struct FetchOptions {
    /// Re-download everything, even files that are already the right size.
    pub force: bool,
    /// Also install the optional English-only ASR export. Off by default: the
    /// multilingual set beats it at English too, and it only exists here so a
    /// machine that wants the small model can still get it.
    pub fallback_asr: bool,
    /// Also install the memory graph's Tier 3 assets (GRAPH.md): the 1.9 GB
    /// GGUF and the llama.cpp binaries. Off by default, like the feature.
    pub graph: bool,
    /// Also install the optional text-embedding model for semantic search
    /// (135 MB). Off by default: keyword search works without it, and this is
    /// the one asset that buys a *feature* rather than correctness.
    pub semantic: bool,
    /// Also install the German flip arbiter (~208 MB, 0.7.7). Off by default:
    /// without it a German-looking flip is *flagged* rather than re-read, which
    /// is exactly what 0.6.1 did and is a correct, quieter daemon.
    pub arbiter_de: bool,
    /// Also install the transcript cross-check decoder (~154 MB, 0.8.0). Off by
    /// default: without it `asr_confidence` is null, which says "nothing has
    /// checked these words" and is true.
    pub confidence: bool,
    /// Also install the night shift's GGML model (~1.03 GB, 0.9.0). Off by
    /// default, like the feature: the model is only half of what the night
    /// shift needs, and the other half — `whisper-cli` with a GPU backend — is
    /// compiled by `models build-night` rather than downloaded, because
    /// upstream publishes no such binary for this card.
    pub night: bool,
    /// Also install the Japanese decoder and the spoken-language identifier
    /// that routes to it (~605 MB, 0.11.0). Off by default: without them a
    /// Japanese turn is transliterated into Latin letters by the multilingual
    /// decoder, which is what every release up to 0.10.3 did.
    pub japanese: bool,
    /// Also install the Korean and Chinese decoder, on top of everything
    /// `japanese` installs (~1.6 GB together, 0.11.6). Off by default, for the
    /// same reason and with the same consequence.
    ///
    /// A superset rather than an alternative: Japanese is one of the three
    /// languages this flag promises, and its decoder is the Parakeet rather
    /// than SenseVoice (FINDINGS §27, rule (b)). So `--cjk` implies
    /// `--japanese`, and `--japanese` alone still installs exactly the 605 MB
    /// pair it always did.
    pub cjk: bool,
    /// Also install the dedicated translator (~911 MB, 0.11.0). Off by
    /// default, and not only because of the size: NLLB-200 is CC-BY-NC 4.0, so
    /// this is the one asset in the catalogue a person has to *choose* for a
    /// licence reason rather than a disk one. Without it translation runs on
    /// the graph model's prompt, which is what 0.9.0 shipped.
    pub translator: bool,
    /// Force the single-stream path. Only the test suite sets this; it is how
    /// the fallback is exercised without finding a server that lacks ranges.
    pub single_stream: bool,
}

impl FetchOptions {
    /// The optional groups this run was asked for.
    pub fn extra_groups(&self) -> Vec<Group> {
        let mut out = Vec::new();
        if self.fallback_asr {
            out.push(Group::FallbackAsr);
        }
        if self.graph {
            out.push(Group::Graph);
        }
        if self.semantic {
            out.push(Group::Semantic);
        }
        if self.arbiter_de {
            out.push(Group::ArbiterDe);
        }
        if self.confidence {
            out.push(Group::Confidence);
        }
        if self.night {
            out.push(Group::Night);
        }
        // `--cjk` implies `--japanese`: the Japanese arm of the route runs on
        // the Parakeet, not on SenseVoice, so a flag that promises three
        // languages has to install both decoders.
        if self.japanese || self.cjk {
            out.push(Group::Japanese);
        }
        if self.cjk {
            out.push(Group::Cjk);
        }
        if self.translator {
            out.push(Group::Translator);
        }
        out
    }

    fn wants(&self, asset: &RemoteAsset) -> bool {
        asset.default() || self.extra_groups().contains(&asset.group)
    }
}

#[derive(Debug, Default)]
pub struct FetchReport {
    pub downloaded: usize,
    pub skipped: usize,
    pub bytes: u64,
    /// Ranged connections the last large download actually used. Reported so
    /// the command can say whether the fast path was available at all.
    pub connections: usize,
}

/// Bring `root` up to the full default model set, plus whatever optional groups
/// were asked for.
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
                "  {:<14} present  ({})",
                asset.role,
                human(asset_installed_bytes(root, asset))
            );
            report.skipped += 1;
            continue;
        }
        if !opts.wants(asset) {
            continue;
        }
        let (n, connections) = fetch_one(root, asset, opts)?;
        report.downloaded += 1;
        report.bytes += n;
        report.connections = report.connections.max(connections);
    }

    // The set is only useful if the daemon can resolve it through the same
    // config it will run with, so verify through `ModelSet` — including the
    // ASR selection the daemon itself will make — not through the catalogue we
    // just wrote. The optional groups are deliberately not part of this: a
    // machine that never asked for the graph model is not incomplete.
    let mut set = ModelSet::resolve_at(root.to_path_buf(), cfg);
    set.select_asr();
    let mut missing = set.missing();
    // Each optional group is verified only when it was asked for: an absent
    // optional model must never read as a broken install (see
    // `models::SemanticModel`). A machine that never wanted a German arbiter
    // is not broken.
    for group in opts.extra_groups() {
        missing.extend(
            optional_group_entries(root, cfg, group)
                .into_iter()
                .filter(|e| !e.ok()),
        );
    }
    if !missing.is_empty() {
        eprintln!();
        for e in &missing {
            eprintln!("  ! {:<14} {}", e.role, describe(e.state(), &e.path));
        }
        bail!(
            "{} model file(s) are still not usable after the fetch — \
             `[models]` in config.toml may point at names this catalogue does not publish",
            missing.len()
        );
    }
    Ok(report)
}

/// What one optional group's files resolve to, through the same door the
/// daemon will open them by.
///
/// A `match` with **no catch-all arm**, and that is the whole point of the
/// function existing. The four groups that had a verify block before 0.11.x
/// each had it written out by hand as `if opts.semantic { … }`, and a fifth
/// and sixth group arrived without one: `--japanese` installed a 605 MB pair
/// and nothing ever asked `JapaneseModel::present()` or `LidModel::present()`
/// afterwards. `--night` never had one either. The gap is latent today —
/// every resolver derives the same paths from the same constants the
/// catalogue uses — but it is exactly the gap the surrounding code was
/// written to close, and it closes silently: a rename or a config-overridable
/// directory makes `models fetch --japanese` print its files and exit 0 while
/// `Japanese::ready()` is false and the feature never runs.
///
/// Written as a match so the compiler asks the question the next time a
/// `Group` is added, rather than the next time somebody reads this file.
fn optional_group_entries(root: &Path, cfg: &ModelsConfig, group: Group) -> Vec<ModelEntry> {
    use crate::models::{
        ARBITER_DE, ArbiterModel, CjkModel, ConfidenceModel, LidModel, TranslatorModel,
    };
    match group {
        Group::Semantic => SemanticModel::resolve_at(root.to_path_buf(), cfg).entries(),
        Group::ArbiterDe => ArbiterModel::resolve_at(root.to_path_buf(), ARBITER_DE).entries(),
        Group::Confidence => ConfidenceModel::resolve_at(root.to_path_buf(), 1).entries(),
        Group::Translator => TranslatorModel::resolve_at(root.to_path_buf()).entries(),
        Group::Japanese => {
            let ja = CjkModel::resolve_at(root.to_path_buf(), crate::models::JAPANESE_ASR);
            let lid = LidModel::resolve_at(root.to_path_buf(), crate::models::LID_WHISPER);
            ja.entries().into_iter().chain(lid.entries()).collect()
        }
        // The identifier is verified with `Group::Japanese`, which `--cjk`
        // always brings with it, so it is not checked twice here.
        Group::Cjk => {
            CjkModel::resolve_at(root.to_path_buf(), crate::models::SENSE_VOICE_ASR).entries()
        }
        // The model downloads and is byte-checked with everything else; the
        // *runtime* is compiled by `models build-night` and is deliberately
        // not a catalogue asset, so there is nothing here a resolver could
        // check that `fetch_one` has not already checked.
        Group::Night => Vec::new(),
        // Verified through `ModelSet` above, like the default set: the graph
        // model is resolved by `Llm::resolve` from `[graph]`, not from
        // `[models]`, and a half-installed one is reported by `models status`.
        Group::Graph => Vec::new(),
        // The old English-only export is part of the ASR selection `set`
        // already made above.
        Group::FallbackAsr => Vec::new(),
        // Not an optional group: `extra_groups` cannot yield it.
        Group::Speech => Vec::new(),
    }
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

fn fetch_one(root: &Path, asset: &RemoteAsset, opts: &FetchOptions) -> Result<(u64, usize)> {
    let part = root.join(format!("{}.part", asset.file_name()));

    let plan = Plan::for_asset(asset, opts, &part)?;
    let connections = plan.connections();
    let got = plan.run(&part)?;
    if got != asset.download_bytes {
        // The `.part` is removed here and only here: a file of the wrong LENGTH
        // is not a partial download, it is a different file, and resuming into
        // it would produce a plausible-looking corruption.
        let _ = fs::remove_file(&part);
        let _ = fs::remove_file(journal_path(&part));
        bail!(
            "{}: downloaded {} bytes, expected exactly {} — refusing to install it \
             (the upstream asset changed, or the transfer was truncated)",
            asset.file_name(),
            got,
            asset.download_bytes
        );
    }
    let _ = fs::remove_file(journal_path(&part));

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
        Install::TarGzInto { dir, keep } => {
            let into = root.join(dir);
            unpack_tar_gz_into(&part, &into, keep)
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
    println!("  {:<14} ok", asset.role);
    Ok((got, connections))
}

// ---------------------------------------------------------------------------
// the transfer
// ---------------------------------------------------------------------------

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout_read(READ_TIMEOUT)
        .user_agent(concat!("nx-recall/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// How one asset is going to be fetched, decided before a byte moves.
enum Plan<'a> {
    /// One connection, start to finish. What every server gets when it declines
    /// ranges, and what small assets get regardless.
    Single {
        role: &'a str,
        url: &'a str,
        total: u64,
    },
    /// `n` ranges in parallel over a file of known length.
    Ranged {
        role: &'a str,
        url: &'a str,
        total: u64,
        chunks: Vec<Chunk>,
    },
}

impl<'a> Plan<'a> {
    fn for_asset(asset: &'a RemoteAsset, opts: &FetchOptions, part: &Path) -> Result<Self> {
        let single = Plan::Single {
            role: asset.role,
            url: asset.url,
            total: asset.download_bytes,
        };
        if opts.single_stream || asset.download_bytes < MIN_PARALLEL_BYTES {
            let _ = fs::remove_file(part);
            let _ = fs::remove_file(journal_path(part));
            return Ok(single);
        }
        // One byte, to ask a question: does this host serve ranges, and does it
        // agree with the catalogue about how long the file is?
        let Some(total) = probe_ranges(asset.url)? else {
            eprintln!(
                "  {:<14} the server does not serve byte ranges — falling back to one stream",
                asset.role
            );
            let _ = fs::remove_file(part);
            let _ = fs::remove_file(journal_path(part));
            return Ok(single);
        };
        if total != asset.download_bytes {
            // Not a hard error yet: the size check after the transfer is the
            // authority, and saying so now is more useful than failing now.
            eprintln!(
                "  {:<14} the server reports {} bytes, the catalogue says {} — \
                 the transfer will be refused at the end",
                asset.role, total, asset.download_bytes
            );
        }
        Ok(Plan::Ranged {
            role: asset.role,
            url: asset.url,
            total,
            chunks: resume(part, total, CONNECTIONS)?,
        })
    }

    fn connections(&self) -> usize {
        match self {
            Plan::Single { .. } => 1,
            Plan::Ranged { chunks, .. } => chunks.len(),
        }
    }

    fn run(self, dest: &Path) -> Result<u64> {
        match self {
            Plan::Single { role, url, total } => single_stream(role, url, total, dest),
            Plan::Ranged {
                role,
                url,
                total,
                chunks,
            } => ranged(role, url, total, chunks, dest),
        }
    }
}

/// `Some(total)` when the host answered a one-byte range request with a `206`
/// and a `Content-Range` we can read a length out of. `None` means "one stream,
/// then" — for any reason at all, including a host that simply said 200.
fn probe_ranges(url: &str) -> Result<Option<u64>> {
    let resp = match agent().get(url).set("Range", "bytes=0-0").call() {
        Ok(r) => r,
        // A transport error here is a real problem and is worth reporting now
        // rather than after twelve threads have each hit it.
        Err(e) => return Err(anyhow::anyhow!(e)).with_context(|| format!("GET {url}")),
    };
    if resp.status() != 206 {
        return Ok(None);
    }
    // "bytes 0-0/16701436"
    Ok(resp
        .header("content-range")
        .and_then(|v| v.rsplit('/').next().map(str::trim).map(str::to_string))
        .and_then(|n| n.parse::<u64>().ok())
        .filter(|n| *n > 0))
}

/// One range of the file, and how much of it is already on disk.
struct Chunk {
    start: u64,
    /// Inclusive, as HTTP means it.
    end: u64,
    done: AtomicU64,
}

impl Chunk {
    fn len(&self) -> u64 {
        self.end - self.start + 1
    }
}

fn journal_path(part: &Path) -> PathBuf {
    let mut s = part.as_os_str().to_os_string();
    s.push(".progress");
    PathBuf::from(s)
}

/// Work out where to start, reading the journal next to a `.part` file that
/// survived an interrupted run.
///
/// Anything the least bit inconsistent — a `.part` of the wrong length, a
/// journal for a different split, a count that exceeds its range — starts the
/// whole download again. Resuming into a file we cannot fully account for is
/// how a download produces something that is the right size and the wrong
/// bytes, which is the one outcome the size check cannot catch.
fn resume(part: &Path, total: u64, n: usize) -> Result<Vec<Chunk>> {
    let per = total.div_ceil(n as u64);
    let mut chunks = Vec::with_capacity(n);
    for i in 0..n {
        let start = per * i as u64;
        if start >= total {
            break;
        }
        chunks.push(Chunk {
            start,
            end: (start + per - 1).min(total - 1),
            done: AtomicU64::new(0),
        });
    }

    let usable = fs::metadata(part).map(|m| m.len()).ok() == Some(total)
        && read_journal(&journal_path(part), total, chunks.len())
            .map(|done| {
                for (chunk, have) in chunks.iter().zip(done) {
                    chunk.done.store(have.min(chunk.len()), Ordering::SeqCst);
                }
            })
            .is_some();
    if !usable {
        let _ = fs::remove_file(part);
        let _ = fs::remove_file(journal_path(part));
        for chunk in &chunks {
            chunk.done.store(0, Ordering::SeqCst);
        }
    }
    Ok(chunks)
}

/// `total`, then one line per range. Plain text on purpose: it has to be
/// readable by a person wondering what a stalled download is doing.
fn read_journal(path: &Path, total: u64, n: usize) -> Option<Vec<u64>> {
    let text = fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if lines.next()?.trim().parse::<u64>().ok()? != total {
        return None;
    }
    let done: Vec<u64> = lines.filter_map(|l| l.trim().parse().ok()).collect();
    (done.len() == n).then_some(done)
}

fn write_journal(path: &Path, total: u64, chunks: &[Chunk]) {
    let mut text = format!("{total}\n");
    for c in chunks {
        text.push_str(&format!("{}\n", c.done.load(Ordering::SeqCst)));
    }
    let _ = fs::write(path, text);
}

/// The measured fast path: N ranges, N connections, one preallocated file.
fn ranged(role: &str, url: &str, total: u64, chunks: Vec<Chunk>, dest: &Path) -> Result<u64> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .read(true)
        .open(dest)
        .with_context(|| format!("creating {}", dest.display()))?;
    file.set_len(total)
        .with_context(|| format!("reserving {} for {}", human(total), dest.display()))?;

    let already: u64 = chunks.iter().map(|c| c.done.load(Ordering::SeqCst)).sum();
    let transferred = AtomicU64::new(0);
    let stop = AtomicBool::new(false);
    let mut bar = Progress::new(role, total, chunks.len());
    if already > 0 {
        eprintln!(
            "  {:<14} resuming at {} of {}",
            role,
            human(already),
            human(total)
        );
    }
    let journal = journal_path(dest);
    let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());

    std::thread::scope(|scope| {
        for chunk in &chunks {
            let file = &file;
            let transferred = &transferred;
            let stop = &stop;
            let failures = &failures;
            scope.spawn(move || {
                if let Err(e) = fetch_range(url, chunk, file, transferred, stop) {
                    // One range failing ends the download, but the others are
                    // asked to stop rather than killed: whatever they have
                    // already written stays in the journal and resumes.
                    stop.store(true, Ordering::SeqCst);
                    failures
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(format!("{e:#}"));
                }
            });
        }

        // The main thread does the two things that must not be done twelve
        // times over: draw one bar, and journal one consistent set of offsets.
        let mut last_journal = Instant::now();
        loop {
            let done: u64 = chunks.iter().map(|c| c.done.load(Ordering::SeqCst)).sum();
            bar.update(done);
            if last_journal.elapsed() > Duration::from_secs(2) {
                write_journal(&journal, total, &chunks);
                last_journal = Instant::now();
            }
            if done >= total || stop.load(Ordering::SeqCst) {
                break;
            }
            // Short, because this loop is also what notices the download has
            // FINISHED: a lazy poll here would put a floor under the wall time
            // of every small transfer.
            std::thread::sleep(Duration::from_millis(15));
        }
    });

    write_journal(&journal, total, &chunks);
    let failures = failures.into_inner().unwrap_or_else(|p| p.into_inner());
    if let Some(first) = failures.first() {
        bail!("{role}: {first} (the partial download was kept; run the fetch again to resume)");
    }
    let done: u64 = chunks.iter().map(|c| c.done.load(Ordering::SeqCst)).sum();
    file.sync_all().ok();
    bar.finish(done);
    let _ = transferred;
    Ok(done)
}

/// One range, written straight into the destination at its own offsets.
///
/// `write_at` rather than seek-then-write: twelve threads share one file, and
/// positioned writes are the only way that is not a race.
fn fetch_range(
    url: &str,
    chunk: &Chunk,
    file: &File,
    transferred: &AtomicU64,
    stop: &AtomicBool,
) -> Result<()> {
    // Its own agent, so this really is its own TCP connection rather than a
    // turn at a shared pooled one — which is the entire point of the exercise.
    let agent = agent();
    let mut buf = vec![0u8; BUF];

    for attempt in 1..=MAX_ATTEMPTS {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let done = chunk.done.load(Ordering::SeqCst);
        if done >= chunk.len() {
            return Ok(());
        }
        let from = chunk.start + done;
        let range = format!("bytes={from}-{}", chunk.end);
        let attempt_result = (|| -> Result<()> {
            let resp = agent
                .get(url)
                .set("Range", &range)
                .call()
                .map_err(|e| anyhow::anyhow!("{e}"))
                .with_context(|| format!("GET {url} [{range}]"))?;
            if resp.status() != 206 {
                bail!(
                    "the server stopped honouring byte ranges mid-download \
                     (status {} for {range})",
                    resp.status()
                );
            }
            let mut reader = resp.into_reader();
            let mut at = from;
            loop {
                if stop.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    return Ok(());
                }
                let n = n.min((chunk.end + 1 - at) as usize);
                if n == 0 {
                    return Ok(());
                }
                file.write_at(&buf[..n], at)?;
                at += n as u64;
                chunk.done.store(at - chunk.start, Ordering::SeqCst);
                transferred.fetch_add(n as u64, Ordering::Relaxed);
            }
        })();

        match attempt_result {
            Ok(()) if chunk.done.load(Ordering::SeqCst) >= chunk.len() => return Ok(()),
            Ok(()) if stop.load(Ordering::SeqCst) => return Ok(()),
            // A short body is a dropped connection, not an error the caller
            // needs to see — go round again from where this range got to.
            Ok(()) | Err(_) if attempt < MAX_ATTEMPTS => {
                std::thread::sleep(Duration::from_millis(250 * attempt as u64));
            }
            Ok(()) => bail!(
                "the connection kept dropping: {} of {} bytes after {MAX_ATTEMPTS} attempts",
                chunk.done.load(Ordering::SeqCst),
                chunk.len()
            ),
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// GET the asset into `dest` over one connection, drawing a progress line on
/// stderr. The path every small asset takes, and every server that declines
/// ranges.
///
/// stderr on purpose: stdout carries the machine-ish report of what was
/// installed, and a `\r`-redrawn bar has no business in it.
fn single_stream(role: &str, url: &str, hint: u64, dest: &Path) -> Result<u64> {
    let resp = agent()
        .get(url)
        .call()
        .map_err(|e| anyhow::anyhow!("{e}"))
        .with_context(|| format!("GET {url}"))?;

    let total = resp
        .header("content-length")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(hint);

    let mut reader = resp.into_reader();
    let mut file = File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut bar = Progress::new(role, total, 1);
    let mut buf = vec![0u8; BUF];
    let mut done: u64 = 0;

    loop {
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("reading {url}"))?;
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

// ---------------------------------------------------------------------------
// unpacking
// ---------------------------------------------------------------------------

fn unpack_tar_bz2(archive: &Path, root: &Path) -> Result<()> {
    let file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let dec = bzip2::read::BzDecoder::new(io::BufReader::new(file));
    let mut tar = tar::Archive::new(dec);
    tar.set_overwrite(true);
    tar.unpack(root)
        .with_context(|| format!("unpacking into {}", root.display()))?;
    Ok(())
}

/// Unpack the entries of a flat `.tar.gz` whose file name starts with one of
/// `keep` into `into`, stripping the archive's own top-level directory.
///
/// Entry by entry rather than `Archive::unpack`, because two things have to be
/// true that `unpack` will not give us: the build-numbered top directory has to
/// go (the catalogue should not have to spell a build number in every path),
/// and the thirty programs nobody shells out to have to stay out of the models
/// directory. Every path is also checked to be a plain file name — a tarball
/// entry called `../../.bashrc` is a well-known way to be interesting.
fn unpack_tar_gz_into(archive: &Path, into: &Path, keep: &[&str]) -> Result<usize> {
    use std::os::unix::fs::PermissionsExt;

    let file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let dec = flate2::read::GzDecoder::new(io::BufReader::new(file));
    let mut tar = tar::Archive::new(dec);
    fs::create_dir_all(into).with_context(|| format!("creating {}", into.display()))?;

    let mut written = 0usize;
    for entry in tar.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path()?.into_owned();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.contains('/') || name.contains("..") {
            bail!(
                "{} contains a suspicious entry: {}",
                archive.display(),
                path.display()
            );
        }
        if !keep.iter().any(|k| name.starts_with(k)) {
            continue;
        }
        let dest = into.join(name);
        let mut out = File::create(&dest).with_context(|| format!("writing {}", dest.display()))?;
        io::copy(&mut entry, &mut out)?;
        // The runner has to be runnable; the shared objects are marked the same
        // way upstream ships them, and both come off the archive's own mode.
        if let Ok(mode) = entry.header().mode() {
            let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(mode | 0o600));
        }
        written += 1;
    }
    Ok(written)
}

// ---------------------------------------------------------------------------
// progress
// ---------------------------------------------------------------------------

struct Progress {
    role: String,
    total: u64,
    connections: usize,
    started: Instant,
    last_draw: Instant,
    tty: bool,
}

impl Progress {
    fn new(role: &str, total: u64, connections: usize) -> Self {
        // SAFETY: isatty only inspects the descriptor.
        let tty = unsafe { libc::isatty(libc::STDERR_FILENO) } == 1;
        let now = Instant::now();
        let p = Self {
            role: role.to_string(),
            total,
            connections,
            started: now,
            // Force the first draw.
            last_draw: now - Duration::from_secs(60),
            tty,
        };
        eprintln!(
            "  {:<14} {} to download{}",
            role,
            human(total),
            if connections > 1 {
                format!(" over {connections} connections")
            } else {
                String::new()
            }
        );
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
            "  {:<14} {:>5.1}%  {} / {}  {}/s{}",
            self.role,
            pct,
            human(done),
            human(self.total),
            human(rate as u64),
            if self.connections > 1 {
                format!("  ×{}", self.connections)
            } else {
                String::new()
            }
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
    // The two semantic roles are a test-only concern here: the download paths
    // treat every asset alike, and only the catalogue assertions below care
    // which role an asset carries.
    use crate::models::{
        SEMANTIC_ROLE, SEMANTIC_TOKENIZER_ROLE, expected_bytes, total_download_bytes,
    };

    // ---- the catalogue -----------------------------------------------------

    #[test]
    fn every_catalogued_url_comes_from_a_publisher_we_named() {
        // Five publishers, and only five: sherpa-onnx releases for the speech
        // leg, llama.cpp releases + bartowski's quants for the graph's Tier 3,
        // the e5 mirror for semantic search — the latter pinned to a commit
        // rather than a branch so "the catalogued size" cannot change under us
        // — and whisper.cpp's own model repository for the night shift's GGML
        // file (0.9.0). A URL that drifts off this list is a supply-chain
        // change and has to be a visible diff.
        const HOSTS: [&str; 6] = [
            "https://github.com/k2-fsa/sherpa-onnx/releases/download/",
            "https://github.com/ggml-org/llama.cpp/releases/download/",
            "https://huggingface.co/bartowski/",
            "https://huggingface.co/Xenova/multilingual-e5-small/resolve/",
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/",
            // 0.11.0: the transformers.js mirror of NLLB-200-distilled-600M,
            // pinned to a commit for the same reason the e5 one is.
            "https://huggingface.co/Xenova/nllb-200-distilled-600M/resolve/",
        ];
        for a in REMOTE_ASSETS {
            assert!(
                HOSTS.iter().any(|h| a.url.starts_with(h)),
                "{} points somewhere unexpected: {}",
                a.role,
                a.url
            );
            let semantic = a.role == SEMANTIC_ROLE || a.role == SEMANTIC_TOKENIZER_ROLE;
            if a.group == Group::Translator {
                assert!(
                    !a.url.contains("/resolve/main/"),
                    "{} must be pinned to a commit, not to a branch: {}",
                    a.role,
                    a.url
                );
                assert!(!a.default(), "the translator is opt-in — and CC-BY-NC");
            }
            if semantic {
                assert!(
                    !a.url.contains("/resolve/main/"),
                    "{} must be pinned to a commit, not to a branch: {}",
                    a.role,
                    a.url
                );
                assert!(!a.default(), "semantic search is opt-in");
            }
            assert!(a.download_bytes > 0);
            assert!(!a.files.is_empty());
        }
    }

    /// The two optional legs are independent switches: a bare fetch installs
    /// neither, and asking for one must not drag in the other.
    #[test]
    fn the_optional_assets_are_each_behind_their_own_flag() {
        let bare = FetchOptions::default();
        let sem = FetchOptions {
            semantic: true,
            ..bare
        };
        let en = FetchOptions {
            fallback_asr: true,
            ..bare
        };
        for a in REMOTE_ASSETS.iter().filter(|a| !a.default()) {
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

    /// 0.11.0. Three files, one group, one flag — and the flag is not one of
    /// anybody else's, because this is the one asset in the catalogue whose
    /// LICENCE is the reason it is optional rather than its size.
    #[test]
    fn the_translator_is_catalogued_at_its_exact_size_behind_its_own_flag() {
        assert_eq!(
            expected_bytes("nllb-200-distilled-600m-int8/encoder.onnx"),
            Some(419_120_483)
        );
        assert_eq!(
            expected_bytes("nllb-200-distilled-600m-int8/decoder_merged.onnx"),
            Some(475_505_771)
        );
        assert_eq!(
            expected_bytes("nllb-200-distilled-600m-int8/tokenizer.json"),
            Some(17_331_224)
        );
        assert_eq!(
            crate::models::translator_download_bytes(),
            419_120_483 + 475_505_771 + 17_331_224
        );
        let assets: Vec<&RemoteAsset> = REMOTE_ASSETS
            .iter()
            .filter(|a| a.group == Group::Translator)
            .collect();
        assert_eq!(assets.len(), 3, "encoder, decoder and tokenizer");
        let bare = FetchOptions::default();
        let want = FetchOptions {
            translator: true,
            ..FetchOptions::default()
        };
        let sem = FetchOptions {
            semantic: true,
            ..FetchOptions::default()
        };
        for a in &assets {
            assert!(!bare.wants(a), "a bare fetch must not pull 911 MB");
            assert!(want.wants(a));
            assert!(!sem.wants(a), "--semantic must not drag the translator in");
        }
        // …and it is not in the default set's budget.
        assert!(total_download_bytes(&[]) < 700_000_000);
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
        assert!(total_download_bytes(&[]) < 700_000_000);
    }

    /// The v3 entry, transcribed from the release asset and the unpacked files.
    /// Every number here is checked by the fetch itself, so a re-upload upstream
    /// fails the download rather than installing a different model under the
    /// same name — and this test is what stops the numbers drifting silently.
    #[test]
    fn the_default_asr_is_the_multilingual_export() {
        let a = REMOTE_ASSETS.iter().find(|a| a.role == "asr").unwrap();
        assert!(a.default());
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
        assert!(!a.default());
        assert_eq!(a.group, Group::FallbackAsr);
        assert_eq!(a.download_bytes, 108_035_095);
        assert_eq!(
            expected_bytes(
                "sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8/encoder.int8.onnx"
            ),
            Some(131_113_202)
        );
    }

    /// GRAPH.md's Tier 3, in the catalogue: optional, byte-exact, and behind
    /// its own flag. Nothing about a default install pays for it.
    #[test]
    fn the_graph_model_is_catalogued_as_an_optional_group() {
        let llm = REMOTE_ASSETS
            .iter()
            .find(|a| a.role == "graph.llm")
            .expect("the bake-off's winner is catalogued");
        assert_eq!(llm.group, Group::Graph);
        assert!(!llm.default(), "a fresh install must not fetch 1.9 GB");
        // The exact figure the model weighs, checked byte for byte by the fetch.
        assert_eq!(llm.download_bytes, 1_929_903_264);
        assert_eq!(
            llm.install,
            Install::File("qwen2.5-3b-instruct-q4_k_m.gguf")
        );
        assert_eq!(
            expected_bytes("qwen2.5-3b-instruct-q4_k_m.gguf"),
            Some(1_929_903_264)
        );

        let rt = REMOTE_ASSETS
            .iter()
            .find(|a| a.role == "graph.runtime")
            .expect("and so is the runtime it needs");
        assert_eq!(rt.group, Group::Graph);
        assert_eq!(rt.download_bytes, 16_701_436);
        assert_eq!(
            rt.install,
            Install::TarGzInto {
                dir: "llama",
                keep: &["llama-cli", "lib"],
            }
        );
        assert_eq!(expected_bytes("llama/llama-cli"), Some(1_453_352));
        // The config's defaults have to name what the catalogue installs, or
        // `models status` and the daemon would look in different places.
        let graph = crate::config::GraphConfig::default();
        assert_eq!(graph.llm_model, "qwen2.5-3b-instruct-q4_k_m.gguf");
        assert_eq!(graph.llama_dir, "llama");
        // Optional, but not the only optional thing any more: semantic search
        // adds two, and this assertion is about the ASR leg.
        assert_eq!(
            REMOTE_ASSETS
                .iter()
                .filter(|a| !a.default() && a.role.starts_with("asr"))
                .count(),
            1
        );
    }

    /// A flag that installs files nothing checks afterwards is a fetch that
    /// can print its files, exit 0, and leave the feature off.
    ///
    /// Every optional group with files a *resolver* can find has to be
    /// verified through that resolver after the download, which is what the
    /// four hand-written `if opts.x { … }` blocks used to do — and what
    /// `--japanese` never got. `optional_group_entries` is a match with no
    /// catch-all so the compiler asks about the next group; this asks about
    /// the ones already here.
    #[test]
    fn every_optional_group_is_verified_through_the_door_the_daemon_opens() {
        let cfg = ModelsConfig::default();
        let root = std::path::Path::new("/nonexistent/nx-recall-audit");
        // Flags on, so `extra_groups` yields every optional group there is.
        let all = FetchOptions {
            fallback_asr: true,
            graph: true,
            semantic: true,
            arbiter_de: true,
            confidence: true,
            night: true,
            japanese: true,
            translator: true,
            ..Default::default()
        };
        let groups = all.extra_groups();
        assert_eq!(groups.len(), 8, "a flag was added without a group");

        for group in groups {
            let entries = optional_group_entries(root, &cfg, group);
            // The four groups whose files a resolver owns must produce them,
            // and every one of those paths must be a path the catalogue knows
            // a size for — otherwise `ModelEntry::state` degrades to "it
            // exists" and the size check silently stops happening.
            let checked = REMOTE_ASSETS.iter().any(|a| {
                a.group == group
                    && !matches!(
                        group,
                        // Night ships a model and a runtime it compiles, and
                        // the model is byte-checked by `fetch_one`; the graph
                        // model and the old English-only export are both
                        // resolved through `ModelSet` above.
                        Group::Night | Group::Graph | Group::Speech | Group::FallbackAsr
                    )
            });
            if !checked {
                continue;
            }
            assert!(
                !entries.is_empty(),
                "`models fetch` for {group:?} verifies nothing afterwards"
            );
            for e in &entries {
                assert!(
                    e.expected.is_some(),
                    "{:?} / {} resolves to {} — a path the catalogue has no size for, \
                     so its size is never checked",
                    group,
                    e.role,
                    e.path.display()
                );
                // …and it is a path under the root that was asked for.
                assert!(e.path.starts_with(root), "{}", e.path.display());
            }
        }

        // Japanese is the pair, both halves.
        let ja = optional_group_entries(root, &cfg, Group::Japanese);
        assert_eq!(ja.len(), 4, "the decoder's two files and the identifier's");
        assert!(ja.iter().any(|e| e.role.starts_with("japanese")));
        assert!(ja.iter().any(|e| e.role.starts_with("lid")));

        // Korean and Chinese are one decoder and no second identifier: they
        // ride on the same whisper-tiny, which `--cjk` always brings along
        // through `Group::Japanese` (0.11.6).
        let cjk = optional_group_entries(root, &cfg, Group::Cjk);
        assert_eq!(cjk.len(), 2, "SenseVoice's graph and its token table");
        assert!(cjk.iter().all(|e| e.role.starts_with("cjk")));
    }

    #[test]
    fn asking_for_cjk_asks_for_japanese_too() {
        // Japanese is one of the three languages `--cjk` promises and its
        // decoder is the Parakeet, not SenseVoice (FINDINGS §27, rule (b)), so
        // the flag is a superset rather than an alternative — and `--japanese`
        // alone must still install exactly the pair it always did.
        let cjk = FetchOptions {
            cjk: true,
            ..Default::default()
        };
        assert!(cjk.extra_groups().contains(&Group::Japanese));
        assert!(cjk.extra_groups().contains(&Group::Cjk));
        assert_eq!(
            total_download_bytes(&cjk.extra_groups()) - total_download_bytes(&[]),
            crate::models::cjk_download_bytes()
        );

        let ja = FetchOptions {
            japanese: true,
            ..Default::default()
        };
        assert_eq!(ja.extra_groups(), vec![Group::Japanese]);
        assert_eq!(
            total_download_bytes(&ja.extra_groups()) - total_download_bytes(&[]),
            crate::models::japanese_download_bytes()
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
    fn the_default_set_is_the_documented_size_and_the_options_add_to_it() {
        // DESIGN §4's "~700 MB default set" on disk: the multilingual encoder
        // alone unpacks to 622 MB. ~496 MB compressed.
        let base = 6_958_444 + 26_485_263 + 487_170_055;
        assert_eq!(total_download_bytes(&[]), base);
        assert_eq!(
            total_download_bytes(&[Group::FallbackAsr]),
            base + 108_035_095
        );
        // Tier 3 nearly quadruples a cold fetch, which is exactly why it is a
        // flag and not a default.
        assert_eq!(
            total_download_bytes(&[Group::Graph]),
            base + 1_929_903_264 + 16_701_436
        );
        assert_eq!(expected_bytes("eres2net_en.onnx"), Some(26_485_263));
        assert_eq!(expected_bytes("no/such/model.onnx"), None);
    }

    #[test]
    fn the_fetch_only_wants_the_groups_it_was_asked_for() {
        let bare = FetchOptions::default();
        assert!(bare.extra_groups().is_empty());
        for a in REMOTE_ASSETS {
            assert_eq!(bare.wants(a), a.default(), "{}", a.role);
        }
        let graph = FetchOptions {
            graph: true,
            ..Default::default()
        };
        assert!(
            graph.wants(
                REMOTE_ASSETS
                    .iter()
                    .find(|a| a.role == "graph.llm")
                    .unwrap()
            )
        );
        assert!(
            !graph.wants(
                REMOTE_ASSETS
                    .iter()
                    .find(|a| a.role == "asr-fallback")
                    .unwrap()
            )
        );
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

    // ---- the transfer, against a local server ------------------------------
    //
    // A fixture, never the network: these tests have to run on a machine with
    // no internet and must never depend on what GitHub feels like doing today.
    // The server can be told to shape each connection, which is the whole
    // reason the parallel path exists — and to refuse ranges, which is the
    // reason the fallback does.

    use super::testserver::{Serve, Shape};

    fn blob(n: usize) -> Vec<u8> {
        // Not zeroes: a positioned-write bug that lands a range at the wrong
        // offset is invisible in a file of identical bytes.
        (0..n).map(|i| (i.wrapping_mul(31) % 251) as u8).collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nxr-fetch-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_ranged_download_reassembles_the_file_byte_for_byte() {
        let body = blob(3 * 1024 * 1024 + 777);
        let server = Serve::start(body.clone(), Shape::Ranges);
        let dir = scratch("ranged");
        let dest = dir.join("asset.bin");

        let total = probe_ranges(&server.url())
            .unwrap()
            .expect("the fixture serves ranges");
        assert_eq!(total, body.len() as u64);

        let chunks = resume(&dest, total, CONNECTIONS).unwrap();
        assert_eq!(chunks.len(), CONNECTIONS);
        let got = ranged("test", &server.url(), total, chunks, &dest).unwrap();

        assert_eq!(got, body.len() as u64);
        assert_eq!(fs::read(&dest).unwrap(), body, "the bytes are not the file");
        assert!(
            server.connections() >= 2,
            "only {} connection(s) — this was not parallel at all",
            server.connections()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The fallback. A server that answers a ranged request with a plain 200
    /// gets exactly the old code path, and the file still arrives.
    #[test]
    fn a_server_without_ranges_falls_back_to_one_stream() {
        let body = blob(512 * 1024);
        let server = Serve::start(body.clone(), Shape::NoRanges);
        let dir = scratch("noranges");
        let dest = dir.join("asset.bin");

        assert_eq!(
            probe_ranges(&server.url()).unwrap(),
            None,
            "a 200 to a Range request must not be read as range support"
        );
        let got = single_stream("test", &server.url(), body.len() as u64, &dest).unwrap();
        assert_eq!(got, body.len() as u64);
        assert_eq!(fs::read(&dest).unwrap(), body);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Resume: half the ranges are already on disk and journalled, and the
    /// second run only fetches what is missing.
    #[test]
    fn an_interrupted_download_resumes_instead_of_starting_again() {
        let body = blob(2 * 1024 * 1024);
        let server = Serve::start(body.clone(), Shape::Ranges);
        let dir = scratch("resume");
        let dest = dir.join("asset.bin");
        let total = body.len() as u64;

        // Simulate an interrupted run: the file is preallocated, the first half
        // of the ranges are written, and the journal says so. The split is
        // computed the same way `resume` computes it, by hand, so the setup
        // does not depend on the function under test.
        let per = total.div_ceil(CONNECTIONS as u64);
        {
            let f = OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&dest)
                .unwrap();
            f.set_len(total).unwrap();
            let mut journal = format!("{total}\n");
            for i in 0..CONNECTIONS {
                let start = per * i as u64;
                let end = (start + per - 1).min(total - 1);
                if i < CONNECTIONS / 2 {
                    f.write_at(&body[start as usize..=end as usize], start)
                        .unwrap();
                    journal.push_str(&format!("{}\n", end - start + 1));
                } else {
                    journal.push_str("0\n");
                }
            }
            f.sync_all().unwrap();
            fs::write(journal_path(&dest), journal).unwrap();
        }

        let chunks = resume(&dest, total, CONNECTIONS).unwrap();
        let already: u64 = chunks.iter().map(|c| c.done.load(Ordering::SeqCst)).sum();
        assert!(already > 0, "the journal was not believed");
        assert!(already < total);

        let served_before = server.bytes_served();
        let got = ranged("test", &server.url(), total, chunks, &dest).unwrap();
        assert_eq!(got, total);
        assert_eq!(fs::read(&dest).unwrap(), body);
        let served = server.bytes_served() - served_before;
        assert!(
            served < total,
            "resuming re-fetched {served} of {total} bytes — that is not a resume"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A journal that does not describe the file next to it is not trusted:
    /// resuming into a file we cannot account for is how a download ends up the
    /// right size and the wrong bytes.
    #[test]
    fn an_inconsistent_journal_starts_the_download_again() {
        let dir = scratch("badjournal");
        let dest = dir.join("asset.bin");
        fs::write(&dest, vec![7u8; 1000]).unwrap();
        fs::write(journal_path(&dest), "999999\n1\n2\n").unwrap();

        let chunks = resume(&dest, 4096, 4).unwrap();
        assert!(chunks.iter().all(|c| c.done.load(Ordering::SeqCst) == 0));
        assert!(!dest.exists(), "the mismatched part file was kept");
        let _ = fs::remove_dir_all(&dir);
    }

    /// The measurement this whole path exists for, reproduced on the fixture:
    /// the server shapes every connection to a fixed rate, exactly as the
    /// user's line does, and twelve of them beat one by close to twelvefold.
    #[test]
    fn parallel_ranges_beat_one_stream_on_a_per_connection_shaped_line() {
        // 2 MB at 4 MB/s per connection: ~500 ms as one stream, and it should
        // land near 12x faster split twelve ways. Sized so the test costs well
        // under a second either way.
        let body = blob(2_000_000);
        let per_conn = 4_000_000;

        let one = Serve::start(body.clone(), Shape::Shaped(per_conn));
        let dir = scratch("speed");
        let a = dir.join("single.bin");
        let started = Instant::now();
        single_stream("single", &one.url(), body.len() as u64, &a).unwrap();
        let single = started.elapsed();

        let many = Serve::start(body.clone(), Shape::ShapedRanges(per_conn));
        let b = dir.join("parallel.bin");
        let total = probe_ranges(&many.url()).unwrap().unwrap();
        let chunks = resume(&b, total, CONNECTIONS).unwrap();
        let started = Instant::now();
        ranged("parallel", &many.url(), total, chunks, &b).unwrap();
        let parallel = started.elapsed();

        assert_eq!(fs::read(&a).unwrap(), body);
        assert_eq!(fs::read(&b).unwrap(), body);
        eprintln!(
            "shaped at {}/s per connection: single {:?}, {CONNECTIONS}-way {:?} ({:.1}x)",
            human(per_conn as u64),
            single,
            parallel,
            single.as_secs_f64() / parallel.as_secs_f64().max(0.001)
        );
        // Deliberately a loose bound: this is a wall-clock assertion on a
        // shared machine, and the claim being defended is "many connections
        // are much faster", not a specific multiple.
        assert!(
            parallel * 3 < single,
            "twelve connections were only {:?} against {:?} for one — \
             the ranges are not running concurrently",
            parallel,
            single
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The one guarantee that must not change: whatever the transport did, the
    /// file has to weigh exactly what the catalogue says or nothing is
    /// installed.
    #[test]
    fn a_wrong_length_asset_is_refused_and_leaves_nothing_installed() {
        let body = blob(9 * 1024 * 1024);
        let server = Serve::start(body.clone(), Shape::Ranges);
        let dir = scratch("wronglen");
        // A catalogue entry that disagrees with the server by one byte.
        let asset = RemoteAsset {
            role: "test",
            url: Box::leak(server.url().into_boxed_str()),
            download_bytes: body.len() as u64 + 1,
            install: Install::File("out.bin"),
            group: Group::Speech,
            files: &[("out.bin", 1)],
        };
        let err = fetch_one(&dir, &asset, &FetchOptions::default()).unwrap_err();
        assert!(
            format!("{err:#}").contains("refusing to install"),
            "{err:#}"
        );
        assert!(!dir.join("out.bin").exists());
        assert!(
            !dir.join("asset.bin.part").exists(),
            "a part file of the wrong length must not be left to resume into"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The install rule for the llama.cpp release: strip the build-numbered top
    /// directory, keep the runner and its shared objects, drop the rest.
    #[test]
    fn a_tar_gz_installs_only_the_entries_it_was_told_to_keep() {
        let dir = scratch("targz");
        let archive = dir.join("bundle.tar.gz");
        {
            let out = File::create(&archive).unwrap();
            let enc = flate2::write::GzEncoder::new(out, flate2::Compression::fast());
            let mut tar = tar::Builder::new(enc);
            for (name, body, mode) in [
                ("llama-b10736/llama-cli", &b"runner"[..], 0o755),
                ("llama-b10736/libllama.so", &b"shared"[..], 0o755),
                ("llama-b10736/llama-server", &b"not wanted"[..], 0o755),
                ("llama-b10736/LICENSE", &b"nor this"[..], 0o644),
            ] {
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_mode(mode);
                header.set_cksum();
                tar.append_data(&mut header, name, body).unwrap();
            }
            tar.into_inner().unwrap().finish().unwrap();
        }

        let into = dir.join("llama");
        let n = unpack_tar_gz_into(&archive, &into, &["llama-cli", "lib"]).unwrap();
        assert_eq!(n, 2);
        assert_eq!(fs::read(into.join("llama-cli")).unwrap(), b"runner");
        assert_eq!(fs::read(into.join("libllama.so")).unwrap(), b"shared");
        assert!(
            !into.join("llama-server").exists(),
            "the whole toolkit was unpacked, not the two files we shell out to"
        );
        assert!(!into.join("LICENSE").exists());
        // The runner has to be runnable.
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(into.join("llama-cli"))
            .unwrap()
            .permissions()
            .mode();
        assert!(
            mode & 0o100 != 0,
            "llama-cli came out non-executable: {mode:o}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

/// A throwaway HTTP server for the fetch tests. Never compiled into the daemon.
///
/// It exists because the three properties the parallel path claims — ranges,
/// resume, and a fallback — are all properties of a *conversation with a
/// server*, and testing them against the real one would make the suite depend
/// on the network the whole design is trying to stop depending on.
#[cfg(test)]
mod testserver {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    #[derive(Clone, Copy)]
    pub enum Shape {
        /// Honours `Range` with a 206.
        Ranges,
        /// Ignores `Range` and answers 200 with the whole body — the fallback
        /// case, and what a plain static file server often does.
        NoRanges,
        /// One stream, rate-limited to N bytes per second.
        Shaped(usize),
        /// Ranges, each connection rate-limited to N bytes per second. The
        /// user's own line, in miniature.
        ShapedRanges(usize),
    }

    pub struct Serve {
        port: u16,
        connections: Arc<AtomicU64>,
        bytes: Arc<AtomicU64>,
    }

    impl Serve {
        pub fn start(body: Vec<u8>, shape: Shape) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback port");
            let port = listener.local_addr().unwrap().port();
            let connections = Arc::new(AtomicU64::new(0));
            let bytes = Arc::new(AtomicU64::new(0));
            let body = Arc::new(body);
            {
                let connections = Arc::clone(&connections);
                let bytes = Arc::clone(&bytes);
                std::thread::spawn(move || {
                    for stream in listener.incoming().flatten() {
                        connections.fetch_add(1, Ordering::SeqCst);
                        let body = Arc::clone(&body);
                        let bytes = Arc::clone(&bytes);
                        std::thread::spawn(move || {
                            let _ = handle(stream, &body, shape, &bytes);
                        });
                    }
                });
            }
            Self {
                port,
                connections,
                bytes,
            }
        }

        pub fn url(&self) -> String {
            format!("http://127.0.0.1:{}/asset.bin", self.port)
        }

        pub fn connections(&self) -> u64 {
            self.connections.load(Ordering::SeqCst)
        }

        pub fn bytes_served(&self) -> u64 {
            self.bytes.load(Ordering::SeqCst)
        }
    }

    fn handle(
        mut stream: TcpStream,
        body: &[u8],
        shape: Shape,
        served: &AtomicU64,
    ) -> std::io::Result<()> {
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut range: Option<(usize, usize)> = None;
        let mut line = String::new();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                return Ok(());
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            if let Some(spec) = trimmed
                .strip_prefix("Range: bytes=")
                .or_else(|| trimmed.strip_prefix("range: bytes="))
            {
                let (a, b) = spec.split_once('-').unwrap_or((spec, ""));
                let start: usize = a.parse().unwrap_or(0);
                let end: usize = b.parse().unwrap_or(body.len() - 1);
                range = Some((start.min(body.len() - 1), end.min(body.len() - 1)));
            }
        }

        let honours = matches!(shape, Shape::Ranges | Shape::ShapedRanges(_));
        let rate = match shape {
            Shape::Shaped(n) | Shape::ShapedRanges(n) => Some(n),
            _ => None,
        };

        let (head, slice) = match (honours, range) {
            (true, Some((start, end))) => (
                format!(
                    "HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\n\
                     Content-Range: bytes {start}-{end}/{}\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    body.len(),
                    end - start + 1
                ),
                &body[start..=end],
            ),
            _ => (
                format!(
                    "HTTP/1.1 200 OK\r\nAccept-Ranges: none\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    body.len()
                ),
                body,
            ),
        };
        stream.write_all(head.as_bytes())?;

        match rate {
            None => {
                stream.write_all(slice)?;
                served.fetch_add(slice.len() as u64, Ordering::SeqCst);
            }
            Some(per_second) => {
                // A hundred slices a second: fine-grained enough that a short
                // transfer still sees the shaping rather than the sleep.
                let step = (per_second / 100).max(1);
                for piece in slice.chunks(step) {
                    stream.write_all(piece)?;
                    served.fetch_add(piece.len() as u64, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        stream.flush()
    }
}
