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
        files: &[("eres2net_en.onnx", 26_485_263)],
    },
    RemoteAsset {
        role: "asr",
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8.tar.bz2",
        download_bytes: 108_035_095,
        install: Install::TarBz2,
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

/// Total bytes the fetch has to pull down for a cold start.
pub fn total_download_bytes() -> u64 {
    REMOTE_ASSETS.iter().map(|a| a.download_bytes).sum()
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
    pub fn asr_model_id(&self) -> String {
        format!("{}@{ASR_CONTRACT_VERSION}", stem(&self.asr_dir))
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
        assert_eq!(
            m.asr_model_id(),
            "sherpa-onnx-nemo-parakeet_tdt_transducer_110m-en-36000-int8@1"
        );
    }
}
