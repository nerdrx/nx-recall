//! Where the analysis models live on disk, and whether they are actually there.
//!
//! Nothing here downloads anything: `[models].dir` points at a directory the
//! user populated, and `recalld models status` says what is present. A missing
//! or incomplete directory degrades the daemon to Step 1 behaviour (capture and
//! VAD only) rather than failing to start.
//!
//! This module also holds the *catalogue*: the upstream URL and the exact byte
//! size of every file in the default model set. `recalld models fetch`
//! (`crate::fetch`) downloads against it and `recalld models status` reports
//! against it, so the two commands can never disagree about what "present"
//! means — a truncated file is a size mismatch to both of them.

use std::path::{Path, PathBuf};

use crate::config::ModelsConfig;

/// Bumped whenever the *contract* around a model changes in a way that makes
/// old vectors or transcripts incomparable — a different model file, different
/// feature extraction, different normalisation. It is baked into the stored
/// `embed_model_id` / `asr_model_id`, which is what the "never compare across
/// model ids" rule keys on.
pub const EMBED_CONTRACT_VERSION: u32 = 1;
pub const ASR_CONTRACT_VERSION: u32 = 1;

/// The Silero VAD graph is compiled into the binary (`crate::VAD_MODEL`), so it
/// is never fetched and never missing. Recorded here only so `models status`
/// can say so out loud instead of leaving a hole in the table.
pub const VAD_MODEL_BYTES: u64 = 643_854;

/// How a downloaded asset becomes files under the models root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Install {
    /// The download *is* the model: write it to this path under the root.
    File(&'static str),
    /// A `.tar.bz2` whose entries already carry the directory name we want,
    /// unpacked into the root as-is.
    TarBz2,
}

/// One thing to download. `download_bytes` is the size of the asset itself (the
/// figure GitHub reports for the release asset, and what a completed download
/// must weigh); `files` is what must exist under the models root afterwards.
#[derive(Debug, Clone)]
pub struct RemoteAsset {
    pub role: &'static str,
    pub url: &'static str,
    pub download_bytes: u64,
    pub install: Install,
    /// Part of the default set a bare `models fetch` installs. The one asset
    /// that is not (the English-only ASR export) is still catalogued, so
    /// `models status` can size it and `models fetch --fallback-asr` can pull
    /// it, but a fresh install does not spend 103 MB on a model the default
    /// already beats at English.
    pub default: bool,
    /// `(path relative to the models root, exact byte size)`.
    pub files: &'static [(&'static str, u64)],
}

impl RemoteAsset {
    /// Last path component of the URL — also the name of the `.part` file.
    pub fn file_name(&self) -> &'static str {
        self.url.rsplit('/').next().unwrap_or(self.url)
    }
}

/// The default model set of DESIGN §4, as published by the sherpa-onnx project.
///
/// Sizes are transcribed from the actual files, not from the documentation, and
/// they are exact: an off-by-anything means a corrupted or re-uploaded asset and
/// the fetch refuses it rather than installing a model the daemon would then
/// half-load. (`speaker-recongition-models` is upstream's spelling of the tag.)
pub const REMOTE_ASSETS: &[RemoteAsset] = &[
    RemoteAsset {
        role: "segmentation",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-segmentation-models/sherpa-onnx-pyannote-segmentation-3-0.tar.bz2",
        download_bytes: 6_958_444,
        install: Install::TarBz2,
        default: true,
        files: &[
            (
                "sherpa-onnx-pyannote-segmentation-3-0/model.onnx",
                5_992_913,
            ),
            (
                "sherpa-onnx-pyannote-segmentation-3-0/model.int8.onnx",
                1_540_506,
            ),
        ],
    },
    RemoteAsset {
        role: "embedding",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_eres2net_sv_en_voxceleb_16k.onnx",
        download_bytes: 26_485_263,
        // Renamed on install: the config default (and every stored
        // `embed_model_id`) says `eres2net_en`, and that name must not drift
        // with whatever upstream calls the file this year.
        install: Install::File("eres2net_en.onnx"),
        default: true,
        files: &[("eres2net_en.onnx", 26_485_263)],
    },
    // The default since 0.5.6. Measured (spike/asr_multilang.py, 60 utterances
    // each, clean and through the Opus-24k voice path): 8.4% WER on German
    // FLEURS and 1.4% on LibriSpeech dev-clean — better than the English-only
    // export is at English, against its 103% on German. It costs ~3x the
    // compute of the 110m and is still RTF 0.08 on one thread.
    RemoteAsset {
        role: "asr",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8.tar.bz2",
        download_bytes: 487_170_055,
        install: Install::TarBz2,
        default: true,
        files: &[
            (
                "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/encoder.int8.onnx",
                652_184_281,
            ),
            (
                "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/decoder.int8.onnx",
                11_845_275,
            ),
            (
                "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/joiner.int8.onnx",
                6_355_277,
            ),
            (
                "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8/tokens.txt",
                93_939,
            ),
        ],
    },
    // The English-only export that was the default up to 0.5.5. Not fetched by
    // default any more, but kept in the catalogue with its exact sizes: it is
    // what an existing install already has on disk, and the daemon falls back
    // to it rather than going silent when the multilingual set is missing.
    RemoteAsset {
        role: "asr-fallback",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8.tar.bz2",
        download_bytes: 108_035_095,
        install: Install::TarBz2,
        default: false,
        files: &[
            (
                "sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8/encoder.int8.onnx",
                131_113_202,
            ),
            (
                "sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8/decoder.int8.onnx",
                3_955_863,
            ),
            (
                "sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8/joiner.int8.onnx",
                1_411_403,
            ),
            (
                "sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8/tokens.txt",
                9_953,
            ),
        ],
    },
];

/// One published transducer export: the directory it unpacks to, the files
/// inside it, and what it can actually transcribe.
///
/// Both exports happen to use the same four file names, so switching between
/// them is a change of directory — which is exactly what makes the fallback in
/// [`ModelSet::select_asr`] a two-line operation rather than a second config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsrExport {
    pub dir: &'static str,
    pub encoder: &'static str,
    pub decoder: &'static str,
    pub joiner: &'static str,
    pub tokens: &'static str,
    /// The BCP-47 tag to store on the segments it produces. `None` for a
    /// multilingual export: the transducer returns text, not a language, and
    /// stamping every German turn `en` would be a lie the database keeps.
    pub lang: Option<&'static str>,
    /// One line for `models status`.
    pub note: &'static str,
}

/// Multilingual, and the default since 0.5.6.
pub const DEFAULT_ASR: AsrExport = AsrExport {
    dir: "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8",
    encoder: "encoder.int8.onnx",
    decoder: "decoder.int8.onnx",
    joiner: "joiner.int8.onnx",
    tokens: "tokens.txt",
    lang: None,
    note: "multilingual, 25 languages — 8.4% WER German, 1.4% English",
};

/// English only, and the default up to 0.5.5.
pub const FALLBACK_ASR: AsrExport = AsrExport {
    dir: "sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8",
    encoder: "encoder.int8.onnx",
    decoder: "decoder.int8.onnx",
    joiner: "joiner.int8.onnx",
    tokens: "tokens.txt",
    lang: Some("en"),
    note: "English only — 2.0% WER English, 103% German",
};

/// In preference order: the first export whose files are all on disk wins.
pub const ASR_EXPORTS: &[AsrExport] = &[DEFAULT_ASR, FALLBACK_ASR];

/// Which ASR export the daemon will actually run, once the disk has been
/// consulted. Returned by [`ModelSet::select_asr`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrSelection {
    /// The multilingual default is installed.
    Default,
    /// It is not, but the English-only export is. Transcription runs on that
    /// rather than switching itself off — an update must never cost the user
    /// their transcripts, only some of their accuracy.
    Fallback,
    /// `[models].asr` names an export this catalogue does not publish, so it is
    /// a deliberate choice and is used exactly as written, present or not.
    Pinned,
    /// Neither catalogued export is installed. Analysis stays off.
    Missing,
}

impl AsrSelection {
    /// The line the daemon logs at start-up, `None` when there is nothing worth
    /// saying. Every non-`None` case names the command that fixes it.
    pub fn warning(&self, root: &Path) -> Option<String> {
        match self {
            AsrSelection::Fallback => Some(format!(
                "the multilingual ASR model is not installed under {} — transcribing with the \
                 English-only {}, which scores ~103% WER on German. Run `recalld models fetch` \
                 to install {} ({}).",
                root.display(),
                FALLBACK_ASR.dir,
                DEFAULT_ASR.dir,
                DEFAULT_ASR.note,
            )),
            _ => None,
        }
    }
}

/// Total bytes the fetch has to pull down for a cold start. The optional
/// English-only export is only counted when it is actually being asked for.
pub fn total_download_bytes(include_optional: bool) -> u64 {
    REMOTE_ASSETS
        .iter()
        .filter(|a| a.default || include_optional)
        .map(|a| a.download_bytes)
        .sum()
}

/// Expected size of a file the catalogue knows about, by its path relative to
/// the models root. `None` for anything we did not publish (a hand-placed
/// model, or a variant the user dropped in).
pub fn expected_bytes(relative: &str) -> Option<u64> {
    REMOTE_ASSETS
        .iter()
        .flat_map(|a| a.files)
        .find(|(p, _)| *p == relative)
        .map(|(_, n)| *n)
}

/// The default ASR export's files under `root`, as entries that can report
/// their own state. `models status` uses this to show *why* it fell back,
/// which is otherwise invisible: the table shows the export in use, not the
/// one that is missing.
pub fn default_asr_entries(root: &Path) -> Vec<ModelEntry> {
    [
        ("asr.encoder", DEFAULT_ASR.encoder),
        ("asr.decoder", DEFAULT_ASR.decoder),
        ("asr.joiner", DEFAULT_ASR.joiner),
        ("asr.tokens", DEFAULT_ASR.tokens),
    ]
    .into_iter()
    .map(|(role, name)| {
        let rel = format!("{}/{name}", DEFAULT_ASR.dir);
        ModelEntry {
            role,
            path: root.join(&rel),
            expected: expected_bytes(&rel),
        }
    })
    .collect()
}

#[derive(Debug, Clone)]
pub struct ModelEntry {
    pub role: &'static str,
    pub path: PathBuf,
    /// Size the catalogue says this file has, when it is a catalogue file.
    pub expected: Option<u64>,
}

/// What `models status` says about one file, and what `models fetch` uses to
/// decide whether it still has work to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryState {
    Ok,
    Missing,
    /// On disk, but not the size the catalogue expects: a truncated download,
    /// or a different model wearing the same name.
    WrongSize {
        found: u64,
        expected: u64,
    },
}

impl ModelEntry {
    pub fn present(&self) -> bool {
        self.path.exists()
    }

    pub fn bytes(&self) -> Option<u64> {
        std::fs::metadata(&self.path).ok().map(|m| m.len())
    }

    pub fn state(&self) -> EntryState {
        match (self.bytes(), self.expected) {
            (None, _) => EntryState::Missing,
            (Some(_), None) => EntryState::Ok,
            (Some(found), Some(expected)) if found == expected => EntryState::Ok,
            (Some(found), Some(expected)) => EntryState::WrongSize { found, expected },
        }
    }

    pub fn ok(&self) -> bool {
        self.state() == EntryState::Ok
    }
}

/// Resolved absolute paths for every model the analysis leg needs.
#[derive(Debug, Clone)]
pub struct ModelSet {
    pub root: PathBuf,
    /// Directory of the transducer export, used for the ASR model id.
    pub asr_dir: PathBuf,
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub joiner: PathBuf,
    pub tokens: PathBuf,
    pub embedding: PathBuf,
    pub segmentation: PathBuf,
    pub asr_threads: i32,
}

impl ModelSet {
    /// `None` when no `[models].dir` is configured — analysis is simply off.
    pub fn resolve(cfg: &ModelsConfig) -> Option<Self> {
        Some(Self::resolve_at(cfg.dir.clone()?, cfg))
    }

    /// Resolve against an explicit root, whatever `[models].dir` says. This is
    /// what `models fetch --dir` installs into, and what `models status` then
    /// reports on, so a fetch and a status of the same directory describe the
    /// same six files.
    pub fn resolve_at(root: PathBuf, cfg: &ModelsConfig) -> Self {
        let asr_dir = root.join(&cfg.asr);
        Self {
            encoder: asr_dir.join(&cfg.asr_encoder),
            decoder: asr_dir.join(&cfg.asr_decoder),
            joiner: asr_dir.join(&cfg.asr_joiner),
            tokens: asr_dir.join(&cfg.asr_tokens),
            embedding: root.join(&cfg.embedding),
            segmentation: root.join(&cfg.segmentation),
            asr_dir,
            root,
            asr_threads: cfg.asr_threads,
        }
    }

    /// Point the ASR leg at `export` under this root.
    fn point_asr_at(&mut self, export: &AsrExport) {
        let dir = self.root.join(export.dir);
        self.encoder = dir.join(export.encoder);
        self.decoder = dir.join(export.decoder);
        self.joiner = dir.join(export.joiner);
        self.tokens = dir.join(export.tokens);
        self.asr_dir = dir;
    }

    /// The same set with its ASR leg pointed at another catalogued export.
    ///
    /// This is what makes the 0.6.1 wrong-language re-decode a small thing: the
    /// English-only export is loaded from the same root, through the same
    /// `Asr::load`, and carries its own `asr_model_id` and `lang` — so a
    /// re-decoded row records the model that actually produced its words.
    pub fn with_asr(&self, export: &AsrExport) -> Self {
        let mut me = self.clone();
        me.point_asr_at(export);
        me
    }

    /// Is every file of `export` on disk at exactly its catalogued size?
    pub fn has_asr_export(&self, export: &AsrExport) -> bool {
        self.asr_export_ok(export)
    }

    /// Is every file of `export` on disk at exactly its catalogued size?
    fn asr_export_ok(&self, export: &AsrExport) -> bool {
        [export.encoder, export.decoder, export.joiner, export.tokens]
            .iter()
            .all(|name| {
                let rel = format!("{}/{name}", export.dir);
                let found = std::fs::metadata(self.root.join(&rel))
                    .map(|m| m.len())
                    .ok();
                match (found, expected_bytes(&rel)) {
                    (Some(found), Some(want)) => found == want,
                    (Some(_), None) => true,
                    (None, _) => false,
                }
            })
    }

    /// Resolve the ASR leg against what is actually installed, and say what was
    /// chosen. This is the step that keeps an update from turning transcription
    /// off: a machine that only has the old English-only export keeps
    /// transcribing on it (loudly) instead of losing the analysis leg.
    ///
    /// A configured directory the catalogue does not publish is left alone —
    /// that is somebody's own model and none of our business. A configured
    /// directory that *is* one of ours is treated as a preference, not a pin,
    /// so an install carrying the 0.5.5 default in its `config.toml` picks up
    /// the multilingual set the moment it is fetched.
    pub fn select_asr(&mut self) -> AsrSelection {
        let configured = self.asr_dir.file_name().map(|s| s.to_string_lossy());
        if !ASR_EXPORTS
            .iter()
            .any(|e| configured.as_deref() == Some(e.dir))
        {
            return AsrSelection::Pinned;
        }
        for (rank, export) in ASR_EXPORTS.iter().enumerate() {
            if self.asr_export_ok(export) {
                self.point_asr_at(export);
                return if rank == 0 {
                    AsrSelection::Default
                } else {
                    AsrSelection::Fallback
                };
            }
        }
        // Nothing usable: report against the default, because that is what
        // `models fetch` is about to install.
        self.point_asr_at(&DEFAULT_ASR);
        AsrSelection::Missing
    }

    /// Just the export directory's name, for reporting.
    pub fn asr_dir_name(&self) -> String {
        dir_name(&self.asr_dir)
    }

    /// The catalogued export this set's ASR paths point at, if any.
    pub fn asr_export(&self) -> Option<&'static AsrExport> {
        let dir = self.asr_dir.file_name()?.to_string_lossy().to_string();
        ASR_EXPORTS.iter().find(|e| e.dir == dir)
    }

    /// The language tag to store on this model's transcripts. `None` means "the
    /// model did not say", which is the truth for a multilingual export.
    pub fn asr_lang(&self) -> Option<&'static str> {
        self.asr_export().and_then(|e| e.lang)
    }

    pub fn entries(&self) -> Vec<ModelEntry> {
        [
            ("asr.encoder", &self.encoder),
            ("asr.decoder", &self.decoder),
            ("asr.joiner", &self.joiner),
            ("asr.tokens", &self.tokens),
            ("embedding", &self.embedding),
            ("segmentation", &self.segmentation),
        ]
        .into_iter()
        .map(|(role, path)| ModelEntry {
            role,
            path: path.clone(),
            expected: self.relative(path).as_deref().and_then(expected_bytes),
        })
        .collect()
    }

    /// The path of `p` relative to the models root, in the same spelling the
    /// catalogue uses. `None` when the config points outside the root.
    fn relative(&self, p: &Path) -> Option<String> {
        p.strip_prefix(&self.root)
            .ok()
            .map(|r| r.to_string_lossy().replace('\\', "/"))
    }

    /// Files that are absent *or* the wrong size — both mean the analysis leg
    /// cannot start, and both are fixed by `recalld models fetch`.
    pub fn missing(&self) -> Vec<ModelEntry> {
        self.entries().into_iter().filter(|e| !e.ok()).collect()
    }

    pub fn complete(&self) -> bool {
        self.missing().is_empty()
    }

    /// Stable identity of the transcript producer, stored on every segment.
    ///
    /// The whole directory name, not its stem: `…-tdt-0.6b-v3-int8` has a dot
    /// in it, and a stem would file every v3 transcript under
    /// `sherpa-onnx-nemo-parakeet-tdt-0`.
    pub fn asr_model_id(&self) -> String {
        format!("{}@{ASR_CONTRACT_VERSION}", dir_name(&self.asr_dir))
    }

    /// Stable identity of the embedding space. Vectors carrying different ids
    /// are never comparable; see `embed::Embedding::cosine`.
    pub fn embed_model_id(&self) -> String {
        format!("{}@{EMBED_CONTRACT_VERSION}", stem(&self.embedding))
    }
}

/// Where the model set goes when nothing else says otherwise: `<data-dir>/models`
/// (DESIGN §4 — "fetched on first run into the data dir"). The data dir survives
/// uninstall, which is exactly right for 700 MB the user should not re-download
/// because the hub replaced a binary.
pub fn default_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("models")
}

fn stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".into())
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(dir: &str) -> ModelsConfig {
        ModelsConfig {
            dir: Some(PathBuf::from(dir)),
            ..Default::default()
        }
    }

    #[test]
    fn no_dir_means_analysis_is_off() {
        assert!(ModelSet::resolve(&ModelsConfig::default()).is_none());
    }

    #[test]
    fn paths_hang_off_the_configured_root() {
        let m = ModelSet::resolve(&cfg("/models")).unwrap();
        assert_eq!(m.embedding, PathBuf::from("/models/eres2net_en.onnx"));
        assert_eq!(
            m.segmentation,
            PathBuf::from("/models/sherpa-onnx-pyannote-segmentation-3-0/model.onnx")
        );
        assert!(m.encoder.starts_with("/models"));
        assert!(m.encoder.ends_with("encoder.int8.onnx"));
    }

    /// A models root populated with sparse files at exactly the catalogued
    /// sizes: "present" means the right size everywhere in this module, so a
    /// fallback test has to satisfy the same rule the daemon does.
    struct Root(PathBuf);

    impl Root {
        fn new(name: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("nxr-models-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let me = Self(dir);
            // Every install has these; only the ASR leg varies below.
            me.place("eres2net_en.onnx");
            me.place("sherpa-onnx-pyannote-segmentation-3-0/model.onnx");
            me
        }

        /// Create `rel` at its catalogued size, without writing 600 MB.
        fn place(&self, rel: &str) {
            let path = self.0.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let f = std::fs::File::create(&path).unwrap();
            f.set_len(expected_bytes(rel).unwrap_or_else(|| panic!("{rel} is not catalogued")))
                .unwrap();
        }

        fn place_export(&self, export: &AsrExport) {
            for name in [export.encoder, export.decoder, export.joiner, export.tokens] {
                self.place(&format!("{}/{name}", export.dir));
            }
        }

        fn set(&self) -> ModelSet {
            ModelSet::resolve(&cfg(self.0.to_str().unwrap())).unwrap()
        }
    }

    impl Drop for Root {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_multilingual_export_is_chosen_when_it_is_installed() {
        let root = Root::new("default");
        root.place_export(&DEFAULT_ASR);
        let mut m = root.set();
        assert_eq!(m.select_asr(), AsrSelection::Default);
        assert!(m.complete());
        assert_eq!(m.asr_dir_name(), DEFAULT_ASR.dir);
        // Multilingual: the model does not say which language it heard, so
        // nothing is stamped on the transcript.
        assert_eq!(m.asr_lang(), None);
        assert!(AsrSelection::Default.warning(&m.root).is_none());
    }

    /// An update must never turn transcription off. With only the old
    /// English-only export on disk the daemon runs on it and says so.
    #[test]
    fn a_missing_default_falls_back_to_the_english_export_with_a_warning() {
        let root = Root::new("fallback");
        root.place_export(&FALLBACK_ASR);
        let mut m = root.set();
        let selection = m.select_asr();

        assert_eq!(selection, AsrSelection::Fallback);
        assert!(
            m.complete(),
            "the fallback set must be usable, not merely named"
        );
        assert_eq!(m.asr_dir_name(), FALLBACK_ASR.dir);
        assert_eq!(
            m.encoder,
            m.root.join(FALLBACK_ASR.dir).join("encoder.int8.onnx")
        );
        assert_eq!(m.asr_model_id(), format!("{}@1", FALLBACK_ASR.dir));
        assert_eq!(m.asr_lang(), Some("en"));

        let warning = selection
            .warning(&m.root)
            .expect("a silent downgrade is the failure mode this exists to prevent");
        assert!(warning.contains("recalld models fetch"), "{warning}");
        assert!(warning.contains(DEFAULT_ASR.dir), "{warning}");

        // ...and the fallback is a stopgap, not a new default: `models status`
        // can still show what is missing.
        let default_missing = default_asr_entries(&m.root);
        assert_eq!(default_missing.len(), 4);
        assert!(
            default_missing
                .iter()
                .all(|e| e.state() == EntryState::Missing)
        );
    }

    /// A config carrying the 0.5.5 default is a preference, not a pin: the
    /// multilingual set wins the moment it is on disk, without anyone having to
    /// hand-edit config.toml after an update.
    #[test]
    fn an_old_configured_default_upgrades_itself_once_the_new_set_is_there() {
        let root = Root::new("upgrade");
        root.place_export(&DEFAULT_ASR);
        root.place_export(&FALLBACK_ASR);
        let mut m = ModelSet::resolve(&ModelsConfig {
            dir: Some(root.0.clone()),
            asr: FALLBACK_ASR.dir.into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(m.select_asr(), AsrSelection::Default);
        assert_eq!(m.asr_dir_name(), DEFAULT_ASR.dir);
    }

    #[test]
    fn neither_export_present_means_analysis_stays_off() {
        let root = Root::new("none");
        let mut m = root.set();
        assert_eq!(m.select_asr(), AsrSelection::Missing);
        assert!(!m.complete());
        // It reports against the default, because that is what a fetch installs.
        assert_eq!(m.asr_dir_name(), DEFAULT_ASR.dir);
        assert_eq!(m.missing().len(), 4);
        assert!(AsrSelection::Missing.warning(&m.root).is_none());
    }

    /// A half-unpacked or truncated export is not a usable one: the fallback
    /// has to be as strict about size as everything else, or the daemon would
    /// pick a set it then fails to load.
    #[test]
    fn a_truncated_export_is_not_selected() {
        let root = Root::new("truncated");
        root.place_export(&FALLBACK_ASR);
        std::fs::write(
            root.0.join(FALLBACK_ASR.dir).join("joiner.int8.onnx"),
            b"not a model",
        )
        .unwrap();
        let mut m = root.set();
        assert_eq!(m.select_asr(), AsrSelection::Missing);
    }

    #[test]
    fn a_model_outside_the_catalogue_is_used_exactly_as_configured() {
        let root = Root::new("pinned");
        root.place_export(&DEFAULT_ASR);
        let mut m = ModelSet::resolve(&ModelsConfig {
            dir: Some(root.0.clone()),
            asr: "my-own-export".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(m.select_asr(), AsrSelection::Pinned);
        assert_eq!(m.asr_dir_name(), "my-own-export");
        assert_eq!(m.asr_lang(), None);
    }

    #[test]
    fn a_missing_directory_is_reported_not_panicked_on() {
        let m = ModelSet::resolve(&cfg("/definitely/not/here")).unwrap();
        assert!(!m.complete());
        assert_eq!(m.missing().len(), m.entries().len());
    }

    #[test]
    fn model_ids_carry_the_contract_version() {
        let m = ModelSet::resolve(&cfg("/models")).unwrap();
        assert_eq!(m.embed_model_id(), "eres2net_en@1");
        // The whole directory name: `0.6b` has a dot in it, and a file_stem
        // would file every v3 transcript under `…-parakeet-tdt-0`.
        assert_eq!(
            m.asr_model_id(),
            "sherpa-onnx-nemo-parakeet-tdt-0.6b-v3-int8@1"
        );
    }
}
