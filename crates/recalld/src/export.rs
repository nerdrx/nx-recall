//! Local Markdown export (0.10.0).
//!
//! # What this is allowed to be
//!
//! DESIGN §12 is the constraint, not a caveat on it: *distribution remains the
//! axis that changes the calculus*. Recording people without their knowledge is
//! one legal question in Germany; handing those recordings to somebody else is
//! a different and much worse one. So this feature is defined by what it is
//! **not**:
//!
//! * It writes **files**, to **one directory on a local filesystem** that the
//!   user picked in a file dialog. That is the entire output surface.
//! * There is no share sheet, no upload, no clipboard, no "send to", no link,
//!   no cloud target, no e-mail, no HTTP anything. The daemon opens no socket
//!   for this and the GUI offers no destination other than a folder.
//! * The path is checked before a byte is written: it must be absolute, it must
//!   already exist, it must not be a volatile runtime directory, and it must not
//!   be a network mount that this code can recognise ([`check_dir`]). A network
//!   mount is a share, whatever the file manager calls it.
//!
//! The copy on the card and in the CLI says the same sentence: *this writes
//! files to your disk and nothing else*. That sentence is the feature.
//!
//! # What it writes
//!
//! One file per local calendar day that actually has turns in it —
//! `2026-09-01.md` — plus `people.md`. Each is stamped with [`MARKER`] on its
//! first line, and **that stamp is the only thing that makes a rewrite legal**:
//! an export into a directory that already holds a `2026-09-01.md` the user
//! wrote by hand refuses and names the file, rather than silently eating it
//! (see [`Plan::blocked`]).
//!
//! Days with no turns produce no file. An empty file in somebody's notes folder
//! is litter, and its absence is the honest statement that nothing was said.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::clock::civil_from_days;
use crate::store::{SegmentRow, Store};
use crate::timeref::{local_date, local_time};

/// The first line of every file this writes, and the permission slip for
/// overwriting one. A file without it belongs to somebody else.
pub const MARKER: &str = "<!-- nx-recall export -->";

/// The footnote a day file carries at most once, explaining the italics.
const SHAKY_REF: &str = "[^shaky]";
const SHAKY_NOTE: &str = "[^shaky]: A second decoder read this turn differently, so the words are uncertain. \
     The speaker is not in doubt; the transcript is.";

/// What `people.md` is called. Fixed, because a client has to be able to say
/// "and one index file" before the run.
pub const PEOPLE_FILE: &str = "people.md";

/// Why an export will not happen. Split from a plain error because the two
/// halves reach the client differently: a refusal is a decision with a reason a
/// person can act on (`err:refused`), a failure is a broken disk.
#[derive(Debug)]
pub enum ExportError {
    Refused(String),
    Failed(anyhow::Error),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Refused(m) => f.write_str(m),
            ExportError::Failed(e) => write!(f, "{e:#}"),
        }
    }
}

impl From<anyhow::Error> for ExportError {
    fn from(e: anyhow::Error) -> Self {
        ExportError::Failed(e)
    }
}

/// What to export. Every field except the directory narrows the range.
#[derive(Debug, Clone, Default)]
pub struct ExportRequest {
    pub dir: PathBuf,
    /// UTC nanoseconds, inclusive.
    pub from: Option<i64>,
    /// UTC nanoseconds, exclusive.
    pub to: Option<i64>,
    /// Canonical speaker id. Narrows to one voice's turns — which produces a
    /// transcript of half a conversation, and is exactly what somebody asking
    /// "what did I say" wants.
    pub speaker: Option<i64>,
    pub thread: Option<i64>,
    /// Write the assistant's translation under each turn that has one.
    pub include_translations: bool,
}

/// One file, rendered but not yet written.
#[derive(Debug, Clone)]
pub struct PlannedFile {
    /// The bare file name (`2026-09-01.md`), never a path: a plan is a list of
    /// names inside one chosen directory, and nothing here may escape it.
    pub name: String,
    pub body: String,
    pub turns: usize,
    pub conversations: usize,
    /// A file of this name already exists in the target directory.
    pub exists: bool,
    /// It exists and was **not** written by this feature, so writing it would
    /// destroy somebody's work.
    pub blocked: bool,
}

impl PlannedFile {
    pub fn bytes(&self) -> u64 {
        self.body.len() as u64
    }
}

/// The whole export, rendered in memory.
///
/// Rendering up front is what makes `export.preview` able to quote an exact
/// byte count and an exact file list rather than an estimate — and it is what
/// lets the overwrite guard run before the first `write()` rather than halfway
/// through the third file.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub files: Vec<PlannedFile>,
}

impl Plan {
    pub fn turns(&self) -> usize {
        self.files.iter().map(|f| f.turns).sum()
    }

    pub fn conversations(&self) -> usize {
        self.files.iter().map(|f| f.conversations).sum()
    }

    pub fn bytes(&self) -> u64 {
        self.files.iter().map(PlannedFile::bytes).sum()
    }

    /// Day files only — `people.md` is an index, not a day.
    pub fn days(&self) -> usize {
        self.files.iter().filter(|f| f.name != PEOPLE_FILE).count()
    }

    /// Existing files this export is not allowed to touch.
    pub fn blocked(&self) -> Vec<&str> {
        self.files
            .iter()
            .filter(|f| f.blocked)
            .map(|f| f.name.as_str())
            .collect()
    }
}

// ---------------------------------------------------------------------------
// the path guard
// ---------------------------------------------------------------------------

/// Filesystem magics that mean "this is somebody else's machine".
///
/// Writing a transcript onto a network share is publication with extra steps,
/// so it is refused here rather than argued about later. The list is what
/// `statfs(2)` can tell us cheaply and unambiguously; it is deliberately a
/// blocklist rather than an allowlist, because the honest failure mode is
/// letting an exotic *local* filesystem through, not blocking one.
///
/// FUSE (`0x65735546`) is **not** on it. Most FUSE mounts are local
/// (`ntfs-3g`, `mergerfs`, an encrypted home), a few are not (`sshfs`,
/// `rclone`), and the kernel cannot tell us which — refusing the class would
/// break more real setups than it protects.
const NETWORK_MAGICS: &[i64] = &[
    0x6969,      // NFS
    0xFF53_4D42, // CIFS / SMB1
    0xFE53_4D42, // SMB2
    0x517B,      // smbfs
    0x0102_1997, // 9P (v9fs)
    0x00C3_6400, // CephFS
    0x5346_414F, // AFS (OpenAFS)
    0x6B41_4653, // AFS (kAFS)
    0x7375_7245, // Coda
    0x7461_636F, // OCFS2
    0x0116_1970, // GFS2
];

/// Directories an export must never be written into, whatever the filesystem
/// says. `/run/user/<uid>` is the canonical one: it is a tmpfs that is wiped at
/// logout, so "exported" would mean "gone by tomorrow". The kernel's own
/// pseudo-filesystems are here for the same reason a `cd /proc` mistake should
/// not cost a transcript.
const REFUSED_PREFIXES: &[&str] = &["/run/user", "/proc", "/sys", "/dev"];

/// Is this a directory the export may write into?
///
/// Absolute, existing, a directory, not a volatile runtime path, not a
/// recognisable network mount. Everything else — permissions, a full disk — is
/// left to the write itself, which reports the real `errno` instead of a guess.
pub fn check_dir(dir: &Path) -> Result<(), ExportError> {
    let refuse = |m: String| Err(ExportError::Refused(m));

    if !dir.is_absolute() {
        return refuse(format!(
            "the export directory must be an absolute path; {} is relative, and \
             a relative path means a different folder depending on which process \
             expands it",
            dir.display()
        ));
    }
    // A `..` in the middle of an otherwise absolute path is not wrong, but it
    // is unreadable in a confirmation dialog, and the whole guard rests on the
    // user recognising the folder they picked.
    if dir
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return refuse(format!(
            "the export directory must be spelled out, not reached through \"..\": {}",
            dir.display()
        ));
    }
    let text = dir.to_string_lossy();
    for prefix in REFUSED_PREFIXES {
        if text == *prefix || text.starts_with(&format!("{prefix}/")) {
            return refuse(format!(
                "{} is under {prefix}, which is not a place files survive — \
                 pick a folder in your home directory",
                dir.display()
            ));
        }
    }
    if !dir.exists() {
        return refuse(format!(
            "{} does not exist. The export writes into a folder you already have, \
             so that a typo cannot scatter a transcript into a new one",
            dir.display()
        ));
    }
    if !dir.is_dir() {
        return refuse(format!("{} is not a directory", dir.display()));
    }
    if let Some(magic) = fs_magic(dir)
        && NETWORK_MAGICS.contains(&magic)
    {
        return refuse(format!(
            "{} is on a network filesystem (fs type {magic:#x}). This export \
             writes to local disks only: copying a transcript onto a share is \
             the one thing the design does not do",
            dir.display()
        ));
    }
    Ok(())
}

/// `statfs(2)`'s `f_type`, or `None` when it cannot be read — in which case the
/// path is allowed through. An unreadable mount is an unknown, and refusing
/// every unknown would refuse ordinary disks on any libc that surprises us.
fn fs_magic(dir: &Path) -> Option<i64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: `buf` is a zeroed statfs we own and `c` is a valid NUL-terminated
    // path that outlives the call.
    unsafe {
        let mut buf: libc::statfs = std::mem::zeroed();
        if libc::statfs(c.as_ptr(), &mut buf) != 0 {
            return None;
        }
        Some(buf.f_type as i64)
    }
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

/// `YYYY-MM-DD` for a local day number.
fn day_name(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The local day a turn belongs to, as days since the epoch.
fn day_of(t_ns: i64) -> i64 {
    let (y, m, d) = local_date(t_ns);
    crate::clock::days_from_civil(y, m, d)
}

fn hh_mm(t_ns: i64) -> String {
    let (h, m) = local_time(t_ns);
    format!("{h:02}:{m:02}")
}

fn speaker_of(row: &SegmentRow) -> &str {
    match row.speaker_name.as_deref() {
        Some(name) if !name.is_empty() => name,
        // An unlabelled turn is a real thing — overlapped speech, or a voice
        // the bank declined to guess at — and it is written as one rather than
        // dropped or attributed.
        _ => "Unknown voice",
    }
}

/// Is this turn one a second decoder disagreed with (v10 `asr_confidence`)?
fn is_shaky(row: &SegmentRow) -> bool {
    row.asr_confidence.as_deref() == Some("shaky")
}

/// The words as they stand: the night shift's reading if it replaced them is
/// already in `text`, so this is only ever `text`, trimmed and flattened.
///
/// Newlines inside a turn would break the `- ` list item into a fragment that
/// renders as body text, so they become spaces. ASR does not emit them today;
/// a corrected turn typed by a person can.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One day's Markdown.
///
/// `rows` must all fall on that local day and be in time order — [`plan`] is
/// what guarantees both, and this function is separately testable because of
/// it (the golden in the test module is the contract).
pub fn render_day(
    day: i64,
    rows: &[SegmentRow],
    worlds: &WorldIndex,
    opts: &ExportRequest,
) -> String {
    let mut out = String::new();
    out.push_str(MARKER);
    out.push('\n');
    out.push_str(&format!("# {}\n", day_name(day)));

    // One H2 per conversation, conversations in the order they started. Turns
    // with no thread (written before threading existed, or never placed) are
    // one final section rather than one section each.
    let mut order: Vec<Option<i64>> = Vec::new();
    let mut groups: BTreeMap<Option<i64>, Vec<&SegmentRow>> = BTreeMap::new();
    for row in rows {
        let key = row.thread_id;
        if !groups.contains_key(&key) {
            order.push(key);
        }
        groups.entry(key).or_default().push(row);
    }

    let mut any_shaky = false;
    for key in order {
        let turns = &groups[&key];
        let head = turns[0];
        let mut names: Vec<&str> = Vec::new();
        for t in turns {
            let n = speaker_of(t);
            if !names.contains(&n) {
                names.push(n);
            }
        }
        let world = worlds.at(head.t_start_ns);
        out.push('\n');
        match key {
            Some(_) => out.push_str(&format!(
                "## {} — {}",
                hh_mm(head.t_start_ns),
                names.join(", ")
            )),
            None => out.push_str(&format!(
                "## {} — {} (outside any conversation)",
                hh_mm(head.t_start_ns),
                names.join(", ")
            )),
        }
        if let Some(world) = world {
            out.push_str(&format!(" — in {world}"));
        }
        out.push('\n');

        for t in turns {
            let words = one_line(t.text.as_deref().unwrap_or(""));
            let shaky = is_shaky(t);
            any_shaky |= shaky;
            let body = if words.is_empty() {
                // A turn with audio and no words is not nothing: it is a turn
                // nobody has transcribed, and hiding it would make the export
                // claim a silence that did not happen.
                "_(not transcribed)_".to_string()
            } else if shaky {
                format!("_{words}_{SHAKY_REF}")
            } else {
                words
            };
            out.push_str(&format!(
                "- **{}** {}: {}\n",
                hh_mm(t.t_start_ns),
                speaker_of(t),
                body
            ));
            if opts.include_translations
                && let Some(tr) = t.translation.as_deref().map(one_line)
                && !tr.is_empty()
            {
                out.push_str(&format!("  > {tr}\n"));
            }
        }
    }

    if any_shaky {
        out.push('\n');
        out.push_str(SHAKY_NOTE);
        out.push('\n');
    }
    out
}

/// `people.md`: the named voices, what they speak, and when they were last
/// heard. Unnamed voices are left out — a list of forty `Speaker_12` rows is
/// not an index of people, it is noise.
pub fn render_people(store: &Store) -> anyhow::Result<String> {
    let speakers = store.list_speakers()?;
    let last = store.export_last_heard()?;
    let mut out = String::new();
    out.push_str(MARKER);
    out.push('\n');
    out.push_str("# People\n\nThe voices you have named. Anyone still unnamed is in the transcript but not here.\n");

    let mut named: Vec<_> = speakers.iter().filter(|s| s.name().is_some()).collect();
    named.sort_by(|a, b| a.display_name.cmp(&b.display_name));
    if named.is_empty() {
        out.push_str("\n_No voice has been named yet._\n");
        return Ok(out);
    }
    out.push('\n');
    for sp in named {
        let languages = match sp.languages.as_deref() {
            Some(l) if !l.is_empty() => l.join(", "),
            _ => "any language".to_string(),
        };
        let heard = match last.get(&sp.id) {
            Some(&ns) => format!("{} {}", day_name(day_of(ns)), hh_mm(ns)),
            None => "never (every turn deleted)".to_string(),
        };
        // The emoji half of a highlight, and only that half. Markdown has no
        // ground and no stylesheet, so a colour token would have to be printed
        // as the word "amber" — which is a note about this program's UI, not a
        // fact about the person, and `people.md` is a file about people. An
        // emoji is the same mark here that it is on every other surface.
        let icon = match sp.icon.as_deref().map(str::trim).filter(|i| !i.is_empty()) {
            Some(i) => format!("{i} "),
            None => String::new(),
        };
        out.push_str(&format!(
            "- **{icon}{}** — {languages} — last heard {heard}\n",
            sp.display_name
        ));
    }
    Ok(out)
}

/// Which world was being played at a given instant, from the roster.
///
/// The roster is the only place a world id is recorded, and it is recorded
/// against a presence interval rather than against a turn — so this is a
/// lookup, not a column, and "no world" is the ordinary answer for a machine
/// that has never run the roster watcher.
#[derive(Debug, Clone, Default)]
pub struct WorldIndex {
    /// `(from_ns, to_ns, world)`, in time order.
    spans: Vec<(i64, i64, String)>,
}

impl WorldIndex {
    pub fn build(store: &Store, from: i64, to: i64) -> anyhow::Result<Self> {
        let mut spans: Vec<(i64, i64, String)> = Vec::new();
        for row in store.roster_between(from, to)? {
            let Some(world) = row.world_id else { continue };
            let end = row.left_at_utc_ns.unwrap_or(i64::MAX);
            match spans.last_mut() {
                // Ten people in one world is ten rows saying the same thing.
                Some(last) if last.2 == world && row.joined_at_utc_ns <= last.1 => {
                    last.1 = last.1.max(end);
                }
                _ => spans.push((row.joined_at_utc_ns, end, world)),
            }
        }
        spans.sort_by_key(|s| s.0);
        Ok(Self { spans })
    }

    pub fn at(&self, t_ns: i64) -> Option<&str> {
        self.spans
            .iter()
            .find(|(from, to, _)| *from <= t_ns && t_ns < *to)
            .map(|(_, _, w)| w.as_str())
    }
}

// ---------------------------------------------------------------------------
// planning and writing
// ---------------------------------------------------------------------------

/// Render everything the request selects, and check every file it would touch.
///
/// This is `export.preview`, and it is also the first half of `export.run` —
/// one code path, so a preview can never describe an export that a run then
/// performs differently.
pub fn plan(store: &Store, req: &ExportRequest) -> Result<Plan, ExportError> {
    check_dir(&req.dir)?;

    let rows = store
        .export_segments(req.from, req.to, req.speaker, req.thread)
        .map_err(ExportError::Failed)?;

    let worlds = if rows.is_empty() {
        WorldIndex::default()
    } else {
        let lo = rows.first().map(|r| r.t_start_ns).unwrap_or(0);
        let hi = rows.last().map(|r| r.t_end_ns).unwrap_or(0);
        WorldIndex::build(store, lo, hi).map_err(ExportError::Failed)?
    };

    let mut by_day: BTreeMap<i64, Vec<SegmentRow>> = BTreeMap::new();
    for row in rows {
        by_day.entry(day_of(row.t_start_ns)).or_default().push(row);
    }

    let mut files = Vec::new();
    for (day, rows) in by_day {
        let body = render_day(day, &rows, &worlds, req);
        let conversations = rows
            .iter()
            .map(|r| r.thread_id)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        files.push(described(
            &req.dir,
            format!("{}.md", day_name(day)),
            body,
            rows.len(),
            conversations,
        ));
    }

    // `people.md` is written whenever anything is: an export of turns whose
    // speaker names are not explained anywhere is half an export.
    if !files.is_empty() {
        let body = render_people(store).map_err(ExportError::Failed)?;
        files.push(described(&req.dir, PEOPLE_FILE.to_string(), body, 0, 0));
    }

    Ok(Plan { files })
}

fn described(
    dir: &Path,
    name: String,
    body: String,
    turns: usize,
    conversations: usize,
) -> PlannedFile {
    let path = dir.join(&name);
    let exists = path.exists();
    let blocked = exists && !is_ours(&path);
    PlannedFile {
        name,
        body,
        turns,
        conversations,
        exists,
        blocked,
    }
}

/// How much of an existing file is read looking for [`MARKER`]. A stamp that is
/// not in the first kilobyte is not a header.
const MARKER_WINDOW: usize = 1024;

/// Was this file written by this feature?
///
/// The whole overwrite policy in one question. An unreadable file answers
/// "no" — if we cannot check, we do not overwrite.
pub fn is_ours(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut buf = vec![0u8; MARKER_WINDOW];
    let Ok(n) = f.read(&mut buf) else {
        return false;
    };
    String::from_utf8_lossy(&buf[..n]).contains(MARKER)
}

/// Write a plan, reporting progress as `(files done, files total)`.
///
/// Refuses before the first byte if any file in it is blocked: an export that
/// wrote three files and then stopped at somebody's hand-written notes would
/// have left the folder in a state nobody asked for.
pub fn write(
    plan: &Plan,
    dir: &Path,
    mut progress: impl FnMut(usize, usize),
) -> Result<u64, ExportError> {
    check_dir(dir)?;
    if let Some(name) = plan.blocked().first() {
        return Err(ExportError::Refused(format!(
            "{} already exists in {} and was not written by NX Recall — it has no \
             \"{MARKER}\" header, so overwriting it would destroy somebody's file. \
             Move it, or export into an empty folder",
            name,
            dir.display()
        )));
    }
    let total = plan.files.len();
    let mut bytes = 0u64;
    for (i, file) in plan.files.iter().enumerate() {
        let path = dir.join(&file.name);
        std::fs::write(&path, &file.body)
            .map_err(|e| ExportError::Failed(anyhow::anyhow!("writing {}: {e}", path.display())))?;
        bytes += file.bytes();
        progress(i + 1, total);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nxr-export-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A turn, at a wall-clock instant expressed in LOCAL time, so the golden
    /// below does not depend on the machine's timezone.
    fn at_local(y: i64, m: u32, d: u32, hh: i64, mm: i64) -> i64 {
        let days = crate::clock::days_from_civil(y, m, d);
        let naive = (days * 86_400 + hh * 3600 + mm * 60) * S;
        naive - crate::clock::local_offset_s(naive) * S
    }

    fn row(t: i64, speaker: &str, text: &str, thread: Option<i64>) -> SegmentRow {
        SegmentRow {
            id: t / S,
            session_id: 1,
            source: "VRChat.exe".into(),
            t_start_ns: t,
            t_end_ns: t + 2 * S,
            speaker_id: Some(1),
            speaker_name: Some(speaker.into()),
            speaker_colour: None,
            speaker_icon: None,
            text: Some(text.into()),
            overlap_frac: None,
            match_score: None,
            audio_path: "a.wav".into(),
            lang: None,
            lang_via: None,
            label_via: None,
            thread_id: thread,
            text_via: None,
            asr_confidence: None,
            night_text: None,
            translation: None,
            translation_via: None,
            mood: None,
            events: None,
        }
    }

    #[test]
    fn a_days_markdown_is_exactly_this() {
        // The golden. Everything the shape promises is in here: the marker, the
        // H1 day, one H2 per conversation with participants and the world, the
        // `- **HH:MM** Name: text` lines, italics plus ONE footnote for a shaky
        // turn, and a translation as an indented quote.
        let day = crate::clock::days_from_civil(2026, 9, 1);
        let base = at_local(2026, 9, 1, 19, 4);
        let mut rows = vec![
            row(base, "Kira", "hey, did you get the thing?", Some(7)),
            row(base + 60 * S, "You", "yeah,  it is on the desk", Some(7)),
            row(base + 120 * S, "Kira", "nice", Some(7)),
            row(base + 2000 * S, "Mara", "hallo zusammen", Some(9)),
        ];
        rows[2].asr_confidence = Some("shaky".into());
        rows[3].translation = Some("hello everyone".into());

        let mut worlds = WorldIndex::default();
        worlds
            .spans
            .push((base - 60 * S, base + 600 * S, "wrld_abc123".into()));

        let opts = ExportRequest {
            include_translations: true,
            ..Default::default()
        };
        let got = render_day(day, &rows, &worlds, &opts);
        let want = "\
<!-- nx-recall export -->
# 2026-09-01

## 19:04 — Kira, You — in wrld_abc123
- **19:04** Kira: hey, did you get the thing?
- **19:05** You: yeah, it is on the desk
- **19:06** Kira: _nice_[^shaky]

## 19:37 — Mara
- **19:37** Mara: hallo zusammen
  > hello everyone

[^shaky]: A second decoder read this turn differently, so the words are uncertain. \
The speaker is not in doubt; the transcript is.
";
        assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
    }

    #[test]
    fn translations_are_left_out_unless_they_are_asked_for() {
        let day = crate::clock::days_from_civil(2026, 9, 1);
        let mut r = row(at_local(2026, 9, 1, 10, 0), "Mara", "hallo", Some(1));
        r.translation = Some("hello".into());
        let body = render_day(day, &[r], &WorldIndex::default(), &ExportRequest::default());
        assert!(
            !body.contains("hello"),
            "an untranslated export leaked one:\n{body}"
        );
        assert!(body.contains("- **10:00** Mara: hallo"));
    }

    #[test]
    fn the_footnote_is_defined_once_however_many_shaky_turns_there_are() {
        let day = crate::clock::days_from_civil(2026, 9, 1);
        let t = at_local(2026, 9, 1, 10, 0);
        let mut a = row(t, "Kira", "one", Some(1));
        let mut b = row(t + 60 * S, "Kira", "two", Some(1));
        a.asr_confidence = Some("shaky".into());
        b.asr_confidence = Some("shaky".into());
        let body = render_day(
            day,
            &[a, b],
            &WorldIndex::default(),
            &ExportRequest::default(),
        );
        assert_eq!(body.matches("[^shaky]:").count(), 1, "{body}");
        assert_eq!(
            body.matches("[^shaky]").count(),
            3,
            "two refs and one definition:\n{body}"
        );
    }

    #[test]
    fn a_file_we_did_not_write_is_never_overwritten() {
        let dir = tmpdir("overwrite");
        let mine = dir.join("2026-09-01.md");
        let theirs = dir.join("2026-09-02.md");
        std::fs::write(&mine, format!("{MARKER}\n# 2026-09-01\n")).unwrap();
        std::fs::write(&theirs, "# my own notes about that evening\n").unwrap();

        assert!(is_ours(&mine));
        assert!(!is_ours(&theirs));

        let plan = Plan {
            files: vec![
                described(&dir, "2026-09-01.md".into(), "x".into(), 1, 1),
                described(&dir, "2026-09-02.md".into(), "y".into(), 1, 1),
            ],
        };
        assert_eq!(plan.blocked(), vec!["2026-09-02.md"]);

        let err = write(&plan, &dir, |_, _| {}).unwrap_err();
        match err {
            ExportError::Refused(m) => {
                assert!(
                    m.contains("2026-09-02.md"),
                    "the refusal must name the file: {m}"
                );
                assert!(m.contains(MARKER), "and say what the header is: {m}");
            }
            other => panic!("expected a refusal, got {other}"),
        }
        // Nothing was written — not even the file that WAS ours.
        assert_eq!(
            std::fs::read_to_string(&mine).unwrap(),
            format!("{MARKER}\n# 2026-09-01\n"),
            "a refused export must not have written the files before the blocked one"
        );
        assert_eq!(
            std::fs::read_to_string(&theirs).unwrap(),
            "# my own notes about that evening\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn our_own_files_are_rewritten_and_progress_is_reported() {
        let dir = tmpdir("rewrite");
        std::fs::write(dir.join("2026-09-01.md"), format!("{MARKER}\nold\n")).unwrap();
        let plan = Plan {
            files: vec![
                described(
                    &dir,
                    "2026-09-01.md".into(),
                    format!("{MARKER}\nnew\n"),
                    1,
                    1,
                ),
                described(
                    &dir,
                    PEOPLE_FILE.into(),
                    format!("{MARKER}\n# People\n"),
                    0,
                    0,
                ),
            ],
        };
        let mut seen = Vec::new();
        let bytes = write(&plan, &dir, |done, total| seen.push((done, total))).unwrap();
        assert_eq!(seen, vec![(1, 2), (2, 2)]);
        assert_eq!(bytes, plan.bytes());
        assert_eq!(
            std::fs::read_to_string(dir.join("2026-09-01.md")).unwrap(),
            format!("{MARKER}\nnew\n")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_path_guard_refuses_what_it_says_it_refuses() {
        // Relative.
        assert!(matches!(
            check_dir(Path::new("notes")),
            Err(ExportError::Refused(_))
        ));
        // Reached through "..".
        let dir = tmpdir("guard");
        let sneaky = dir.join("..").join(dir.file_name().unwrap());
        assert!(matches!(check_dir(&sneaky), Err(ExportError::Refused(_))));
        // Volatile runtime directories, whatever they contain.
        for bad in ["/run/user", "/run/user/1000/notes", "/proc", "/sys/kernel"] {
            match check_dir(Path::new(bad)) {
                Err(ExportError::Refused(m)) => assert!(m.contains(bad) || m.contains("under")),
                other => panic!("{bad} was not refused: {other:?}"),
            }
        }
        // Missing, and not-a-directory.
        assert!(matches!(
            check_dir(&dir.join("nope")),
            Err(ExportError::Refused(_))
        ));
        let file = dir.join("a-file");
        std::fs::write(&file, "x").unwrap();
        assert!(matches!(check_dir(&file), Err(ExportError::Refused(_))));
        // And the ordinary case: /tmp is a local disk and is allowed.
        check_dir(&dir).expect("a real local directory must pass");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_network_magic_would_be_refused() {
        // The live check needs a mount we cannot make in a test, so the table
        // itself is the thing worth pinning: NFS and SMB are on it, and the
        // filesystems people actually export onto are not.
        for net in [0x6969, 0xFF53_4D42u32 as i64, 0xFE53_4D42u32 as i64] {
            assert!(NETWORK_MAGICS.contains(&net), "{net:#x} fell off the list");
        }
        for local in [
            0xEF53,      // ext4
            0x9123_683E, // btrfs
            0x5846_5342, // xfs
            0x0102_1994, // tmpfs (/tmp — the tests write there)
            0x6573_5546, // fuse, deliberately allowed
        ] {
            assert!(
                !NETWORK_MAGICS.contains(&local),
                "{local:#x} would be refused"
            );
        }
    }
}
