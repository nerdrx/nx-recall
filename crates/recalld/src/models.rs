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
/// The same rule for the *text* embedding space (`crate::semantic`). Baked into
/// the `model_id` stored on every row of `segment_vectors`, so changing the
/// export, the pooling or the `query:`/`passage:` prefixes retires the old
/// vectors instead of silently comparing against them.
pub const SEMANTIC_CONTRACT_VERSION: u32 = 1;

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
    /// A `.tar.gz` whose entries all sit under one top-level directory named
    /// after the upstream build. That directory is **stripped** (its name
    /// carries a build number this catalogue should not have to spell twice)
    /// and the entries whose file name starts with one of `keep` are written
    /// flat into `<root>/<dir>`. Everything else in the archive is left on the
    /// floor: a release tarball of a toolkit carries a dozen programs and this
    /// daemon shells out to exactly one of them.
    TarGzInto {
        dir: &'static str,
        keep: &'static [&'static str],
    },
}

/// Which set an asset belongs to. Only [`Group::Speech`] is fetched by a bare
/// `models fetch`; the rest are opt-in flags, because each of them costs
/// hundreds of megabytes for something the daemon works perfectly well without.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// Capture's own models: ASR, embedding, segmentation. Without these the
    /// daemon degrades to VAD-only, so they are not optional in any real sense.
    Speech,
    /// The English-only ASR export that was the default up to 0.5.5.
    FallbackAsr,
    /// The memory graph's Tier 3 (GRAPH.md): a ≤3B Q4 GGUF and the llama.cpp
    /// binaries to run it. **Optional and off by default** — `[graph].enabled`
    /// ships false, and a machine that never turns it on never needs these.
    Graph,
    /// Semantic search's sentence-embedding model (DESIGN §6). Optional:
    /// keyword search works without it.
    Semantic,
    /// The German flip arbiter (0.7.7): Whisper base, forced to German. There
    /// is no German-only transducer to re-decode with, and Whisper's language
    /// token is the one honest way to force the constraint.
    ArbiterDe,
    /// The transcript cross-check (0.8.0): Canary 180m int8, a *second*
    /// decoder whose agreement with the first is the confidence flag. Optional,
    /// and without it `asr_confidence` is null rather than guessed.
    Confidence,
    /// The night shift's third decoder (0.9.0): whisper-large-v3 as a GGML
    /// file, run on the GPU overnight over the rows the cross-check called
    /// shaky. Optional, and the *runtime* for it is not in this catalogue at
    /// all — there is no ROCm/Vulkan `whisper-cli` release to download, so
    /// `recalld models build-night` compiles one. See [`Group::note`].
    Night,
    /// Japanese (0.11.0): a decoder that speaks it, and a spoken-language
    /// identifier to decide when to reach for one.
    ///
    /// **Two assets in one group on purpose.** Either alone is useless — a
    /// Japanese decoder nothing can route to never runs, and a language
    /// identifier with nothing to hand a Japanese turn to only produces a log
    /// line. `models fetch --japanese` installs the pair or neither.
    Japanese,
    /// Korean and Chinese (0.11.6): SenseVoice-Small, the one model in the zoo
    /// that speaks both — and the only thing that speaks Korean at all, since
    /// there is no Korean Parakeet (`…-0.6b-ko-3000-int8` is a 404).
    ///
    /// Its own group rather than a third asset in [`Group::Japanese`] because
    /// the bench kept two decoders rather than one (FINDINGS §27, rule (b)):
    /// SenseVoice is 4.0 CER points behind the Japanese Parakeet on Japanese at
    /// 3 s, so an install that only ever meets Japanese speakers should not
    /// download it. `models fetch --cjk` installs both groups, because Japanese
    /// is one of the three languages that flag promises.
    Cjk,
    /// The dedicated translator (0.11.0): NLLB-200-distilled-600M as two int8
    /// ONNX graphs plus its tokenizer. Optional, and **CC-BY-NC 4.0** — see
    /// the catalogue entry, which says what that means for anything commercial.
    Translator,
}

impl Group {
    pub fn is_default(self) -> bool {
        matches!(self, Group::Speech)
    }

    /// One line for `models status`, said from the user's point of view.
    pub fn note(self) -> &'static str {
        match self {
            Group::Speech => "required for transcription and speaker identity",
            Group::FallbackAsr => {
                "optional — the older English-only ASR export, also loaded by light mode \
                 (`[asr].light_mode`) while a game is running"
            }
            Group::Graph => {
                "optional — the memory graph's local model; nothing needs it unless \
                 [graph].enabled is on"
            }
            Group::Semantic => {
                "optional — the embedding model behind semantic search; keyword \
                 search works without it"
            }
            Group::ArbiterDe => {
                "optional — the German flip arbiter; without it a German-looking \
                 flip is flagged rather than re-read"
            }
            Group::Confidence => {
                "optional — the second decoder behind `asr_confidence`; without \
                 it turns are unflagged rather than wrongly flagged"
            }
            Group::Night => {
                "optional — the night shift's GPU decoder; the model downloads, \
                 the runtime is built by `recalld models build-night`"
            }
            Group::Japanese => {
                "optional — a Japanese decoder and the language identifier that \
                 routes to it; without them Japanese turns come back as Latin \
                 nonsense nothing can detect"
            }
            Group::Cjk => {
                "optional — a Korean and Chinese decoder; without it those turns \
                 come back as Latin nonsense nothing can detect"
            }
            Group::Translator => {
                "optional — the dedicated translator; without it translation \
                 runs on the graph model's prompt. CC-BY-NC 4.0: personal use only"
            }
        }
    }
}

/// One thing to download. `download_bytes` is the size of the asset itself (the
/// figure the host reports for it, and what a completed download must weigh);
/// `files` is what must exist under the models root afterwards.
#[derive(Debug, Clone)]
pub struct RemoteAsset {
    pub role: &'static str,
    pub url: &'static str,
    pub download_bytes: u64,
    pub install: Install,
    pub group: Group,
    /// `(path relative to the models root, exact byte size)`.
    ///
    /// Not necessarily every file the asset installs — the segmentation tarball
    /// carries a README nobody checks, and the llama.cpp one carries thirty-odd
    /// shared objects. These are the ones something actually opens, and the
    /// whole transfer is byte-verified against `download_bytes` regardless.
    pub files: &'static [(&'static str, u64)],
}

impl RemoteAsset {
    /// Last path component of the URL — also the name of the `.part` file.
    pub fn file_name(&self) -> &'static str {
        self.url.rsplit('/').next().unwrap_or(self.url)
    }

    pub fn default(&self) -> bool {
        self.group.is_default()
    }
}

/// `RemoteAsset::role` for the two optional semantic-search files. Named
/// constants because `models fetch --semantic` and `models status` both have to
/// pick them out of the catalogue and a typo in either would be silent.
pub const SEMANTIC_ROLE: &str = "semantic";
pub const SEMANTIC_TOKENIZER_ROLE: &str = "semantic.tokens";

/// The directory the semantic model installs into, under the models root.
pub const SEMANTIC_DIR: &str = "multilingual-e5-small-int8";

/// `RemoteAsset::role` for the German flip arbiter (0.7.7), for the same reason
/// the semantic roles are named: `models fetch --arbiter-de` and `models status`
/// both have to pick it out of the catalogue.
pub const ARBITER_DE_ROLE: &str = "arbiter.de";

/// `RemoteAsset::role` for the transcript cross-check decoder (0.8.0), for the
/// same reason: `models fetch --confidence` and `models status` both pick it
/// out of the catalogue by this string.
/// `RemoteAsset::role` for the night shift's GGML model (0.9.0).
pub const NIGHT_ROLE: &str = "night.model";
/// The file name the night model installs under, in the models root. Named
/// rather than derived from the URL for the reason every other model here is:
/// the config default spells this name, and it must not drift with whatever
/// upstream calls the file this year.
pub const NIGHT_MODEL_FILE: &str = "ggml-large-v3-q5_0.bin";
/// The whisper.cpp tag `models build-night` checks out. Pinned, like every
/// other asset's bytes: a build recipe that follows a moving branch is not a
/// pinned dependency, it is a hope.
pub const NIGHT_WHISPER_TAG: &str = "v1.9.3";
/// Header-only Khronos repos the Vulkan build needs when the distribution has
/// the driver but not the development headers. One SDK release, pinned.
pub const NIGHT_VULKAN_HEADERS_TAG: &str = "v1.4.313";
pub const NIGHT_SPIRV_HEADERS_TAG: &str = "vulkan-sdk-1.4.313.0";
/// Where the built runtime goes, under the models root.
pub const NIGHT_DIR: &str = "whisper";

// ---- the dedicated translator (0.11.0) ------------------------------------

/// `RemoteAsset::role`s for the three translator files, named for the same
/// reason every other group's are: `models fetch --translator` and
/// `models status` both pick them out of the catalogue by these strings.
pub const TRANSLATOR_ROLE: &str = "translator.encoder";
pub const TRANSLATOR_DECODER_ROLE: &str = "translator.decoder";
pub const TRANSLATOR_TOKENIZER_ROLE: &str = "translator.tokens";

/// The directory the translator installs into, under the models root. The
/// stored `translation.via` is derived from this name, exactly as the semantic
/// model's id is, so it must not drift with whatever upstream calls the export
/// this year.
pub const TRANSLATOR_DIR: &str = "nllb-200-distilled-600m-int8";
pub const TRANSLATOR_ENCODER_FILE: &str = "encoder.onnx";
pub const TRANSLATOR_DECODER_FILE: &str = "decoder_merged.onnx";
pub const TRANSLATOR_TOKENIZER_FILE: &str = "tokenizer.json";

/// Bumped when anything about how a translation is produced changes — the
/// export, the decoding, the language table. `translation.via` carries it, so a
/// row translated by one contract is never mistaken for a row translated by
/// another, and the whole lot can be found and re-run.
pub const TRANSLATOR_CONTRACT_VERSION: u32 = 1;

pub const CONFIDENCE_ROLE: &str = "confidence";

// ---- Japanese (0.11.0) ----------------------------------------------------

/// `RemoteAsset::role` for the Japanese decoder, for the same reason every
/// other optional role is named: `models fetch --japanese` and `models status`
/// both pick it out of the catalogue by this string.
pub const JAPANESE_ROLE: &str = "japanese.asr";
/// …and for the spoken-language identifier that routes turns to it.
pub const LID_ROLE: &str = "lid";

/// The directory the Japanese decoder installs into, under the models root.
pub const JAPANESE_DIR: &str = "sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8";

// ---- Korean and Chinese (0.11.6) ------------------------------------------

/// `RemoteAsset::role` for the Korean and Chinese decoder.
pub const SENSE_VOICE_ROLE: &str = "cjk.asr";

/// The directory SenseVoice installs into, under the models root.
pub const SENSE_VOICE_DIR: &str = "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17";

/// The directory the cross-check decoder installs into, under the models root.
pub const CONFIDENCE_DIR: &str = "sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8";

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
        group: Group::Speech,
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
        group: Group::Speech,
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
        group: Group::Speech,
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
    // Semantic search (crate::semantic). OPTIONAL, and the only asset in this
    // catalogue that is not published by k2-fsa: `intfloat/multilingual-e5-small`
    // as an int8 ONNX export, from the transformers.js mirror that publishes it,
    // pinned to a commit so "the catalogued size" cannot be changed under us by
    // a re-upload to `main`. The tokenizer is a second asset rather than part of
    // the first because upstream ships them as two files; the daemon refuses to
    // load the model without it, so half an install is not a usable one.
    //
    // Why this model and not the paraphrase MiniLM, and why int8 and not the
    // 470 MB fp32: see the module docs in `crate::semantic`, with numbers.
    RemoteAsset {
        role: SEMANTIC_ROLE,
        url: "https://huggingface.co/Xenova/multilingual-e5-small/resolve/761b726dd34fb83930e26aab4e9ac3899aa1fa78/onnx/model_quantized.onnx",
        download_bytes: 118_308_185,
        install: Install::File("multilingual-e5-small-int8/model.onnx"),
        group: Group::Semantic,
        files: &[("multilingual-e5-small-int8/model.onnx", 118_308_185)],
    },
    RemoteAsset {
        role: SEMANTIC_TOKENIZER_ROLE,
        url: "https://huggingface.co/Xenova/multilingual-e5-small/resolve/761b726dd34fb83930e26aab4e9ac3899aa1fa78/tokenizer.json",
        download_bytes: 17_082_730,
        install: Install::File("multilingual-e5-small-int8/tokenizer.json"),
        group: Group::Semantic,
        files: &[("multilingual-e5-small-int8/tokenizer.json", 17_082_730)],
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
        group: Group::FallbackAsr,
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
    // The German flip arbiter (0.7.7). OPTIONAL, and the flag is
    // `models fetch --arbiter-de`.
    //
    // Its job is not to beat the multilingual export at German — it is to beat
    // that export's *flips*, which are 103%-WER garbage. Measured
    // (spike/arbiter_de.py, the same fragment protocol that found the flips):
    // Whisper base forced to `language=de` flips German fragments to English
    // 2-3% of the time against v3's 12% at 1 s, comes back empty ~0% of the
    // time at 1.5 s and above, and its words are in the reference 54% of the
    // time at 1.5 s and 66% at 3 s. That is a poor transcript and a strict
    // improvement on the nonsense it replaces — which is why the replacement is
    // gated at 1.5 s (`crate::arbiter`) and flag-only below it, where precision
    // falls to 28%.
    //
    // int8, and only the two files the decoder opens plus the token table: the
    // tarball also carries fp32 exports and test WAVs this daemon never reads.
    RemoteAsset {
        role: ARBITER_DE_ROLE,
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-base.tar.bz2",
        download_bytes: 207_557_382,
        install: Install::TarBz2,
        group: Group::ArbiterDe,
        files: &[
            (
                "sherpa-onnx-whisper-base/base-encoder.int8.onnx",
                29_120_534,
            ),
            (
                "sherpa-onnx-whisper-base/base-decoder.int8.onnx",
                130_672_026,
            ),
            ("sherpa-onnx-whisper-base/base-tokens.txt", 816_730),
        ],
    },
    // The transcript cross-check (0.8.0). OPTIONAL, and the flag is
    // `models fetch --confidence`.
    //
    // Canary 180m flash int8: four languages, RTF 0.33 on one thread, and a
    // decoder built differently enough from the Parakeet transducer that its
    // agreement carries information. `spike/confidence_bench.py` measured what
    // that information is worth on FLEURS + LibriSpeech through Opus 24k: at
    // τ = 0.5 the turns where the two decoders disagree carry 76.4% word error
    // against 18.2% where they agree — a 4.2× split, which is the whole reason
    // the flag exists. It is never allowed to replace a word (§11: it drops
    // 11% of real turns outright, which is disqualifying for a transcriber and
    // irrelevant for a witness).
    //
    // int8, and only the two files the recognizer opens plus the token table;
    // the tarball also carries test WAVs this daemon never reads.
    RemoteAsset {
        role: CONFIDENCE_ROLE,
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8.tar.bz2",
        download_bytes: 153_692_328,
        install: Install::TarBz2,
        group: Group::Confidence,
        files: &[
            (
                "sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8/encoder.int8.onnx",
                132_678_643,
            ),
            (
                "sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8/decoder.int8.onnx",
                74_437_848,
            ),
            (
                "sherpa-onnx-nemo-canary-180m-flash-en-es-de-fr-int8/tokens.txt",
                53_555,
            ),
        ],
    },
    // ---- the night shift's third decoder (0.9.0) --------------------------
    //
    // Optional, and the flag is `models fetch --night`. One asset: the model.
    // There is deliberately no runtime asset next to it, which is the honest
    // difference between this group and `Group::Graph` — llama.cpp ships
    // prebuilt Linux binaries and whisper.cpp's releases carry no GPU backend
    // for an AMD card, so `models build-night` compiles `whisper-cli` from a
    // pinned tag on the machine that will run it.
    //
    // q5_0 rather than fp16: measured on 20 FLEURS utterances against the
    // sherpa int8 large-v3 that §11 and §12 used as their ceiling (see
    // `spike/night_vote_bench.py` and FINDINGS §13), the quantisation is a
    // wash on words and saves two thirds of the download and of the VRAM.
    RemoteAsset {
        role: NIGHT_ROLE,
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-q5_0.bin",
        download_bytes: 1_081_140_203,
        install: Install::File(NIGHT_MODEL_FILE),
        group: Group::Night,
        files: &[(NIGHT_MODEL_FILE, 1_081_140_203)],
    },
    // ---- Japanese (0.11.0) ------------------------------------------------
    //
    // Optional, and the flag is `models fetch --japanese`. Two assets, ~605 MB
    // together, and they are one group because either alone does nothing.
    //
    // The decoder is NVIDIA's Japanese Parakeet as published in the k2-fsa
    // zoo. Note the shape: `tdt_ctc`, and the export sherpa ships is the **CTC
    // head** — one `model.int8.onnx`, not the encoder/decoder/joiner triple
    // the multilingual v3 uses — which is why it is loaded by
    // `crate::asr_cjk::CjkAsr` through the offline `nemo_ctc` config rather than
    // through `Asr::load`.
    //
    // Measured (`spike/asr_ja.py` and `spike/asr_cjk.py`, FLEURS ja test, CER after NFKC
    // normalisation and punctuation stripping — WER is meaningless for a
    // language written without spaces): see FINDINGS §23 and the numbers on
    // `crate::asr_cjk`. It is preferred over routing Japanese turns to the
    // night shift's whisper-large-v3 because it runs on the CPU, in the live
    // path, in the same place the German arbiter already runs.
    //
    // sha256 of the tarball, recorded because a byte count is a weak hash and
    // this catalogue's contract is exactness:
    //   4b0a800ef29f4f4c8667339bf6f60d5bfdc2852ddc9dc5741aea65b6f8d1306b
    RemoteAsset {
        role: JAPANESE_ROLE,
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8.tar.bz2",
        download_bytes: 489_389_564,
        install: Install::TarBz2,
        group: Group::Japanese,
        files: &[
            (
                "sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8/model.int8.onnx",
                655_542_604,
            ),
            (
                "sherpa-onnx-nemo-parakeet-tdt_ctc-0.6b-ja-35000-int8/tokens.txt",
                28_557,
            ),
        ],
    },
    // ---- Korean and Chinese (0.11.6) --------------------------------------
    //
    // SenseVoice-Small, the FunASR model as published in the k2-fsa zoo:
    // zh + en + ja + ko + yue in one graph. The flag is `models fetch --cjk`,
    // which installs this on top of `--japanese`'s pair.
    //
    // It is here for Korean and Chinese and NOT for Japanese, and that split is
    // a measurement rather than caution (`spike/asr_cjk.py`, 200 FLEURS
    // utterances per language, CER after NFKC normalisation and punctuation
    // stripping — WER is meaningless for two languages written without spaces).
    // The rule going in was "one decoder for all three if SenseVoice is within
    // 2 CER points of the Japanese Parakeet on ja at 3 s":
    //
    //   ja  full 7.6% / 3 s 15.3%   against the Parakeet's 7.5% / 11.3%
    //   ko  full 9.2% / 3 s  9.6%
    //   zh  full 10.7% / 3 s 9.6%
    //
    // Level on whole utterances and 4.0 points behind on the 3 s fragment this
    // daemon lives in — twice the bar — so the Parakeet keeps Japanese. On
    // Korean and Chinese it has no competition: there is no Korean Parakeet
    // (`…-0.6b-ko-3000-int8` is a 404), and both languages sit at 9.6% CER at
    // 3 s, comfortably inside the 20% usability bar. RTF 0.012 on 4 cores.
    //
    // Only the int8 graph and the token table are checked; the tarball also
    // carries the 937 MB fp32 export and five test WAVs this daemon never
    // opens, which is why the download is four times what it installs.
    //
    // sha256 of the tarball:
    //   f6b2a72ebcb1ac7a764d4cfccd886e6bcb2a95c4657c2199d0ba95ed4b9ea71a
    RemoteAsset {
        role: SENSE_VOICE_ROLE,
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17.tar.bz2",
        download_bytes: 1_047_870_769,
        install: Install::TarBz2,
        group: Group::Cjk,
        files: &[
            (
                "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/model.int8.onnx",
                239_233_841,
            ),
            (
                "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17/tokens.txt",
                315_894,
            ),
        ],
    },
    // The spoken-language identifier (`crate::lid`): Whisper tiny int8, read
    // for its language token rather than for words.
    //
    // Tiny rather than base, and that is a measurement rather than a saving
    // (`spike/lid_bench.py`, 200 FLEURS utterances per language). Base is
    // BETTER at recall — 99.5% ja at 3 s against tiny's 96.5% — and it is the
    // wrong model anyway, because it fails the gate in the direction that
    // costs something: base hears 3% of English at 3 s and 5% at 1.5 s as
    // Japanese, while tiny heard **zero of 400** German and English
    // utterances as Japanese at any length. A missed Japanese turn stays
    // exactly as wrong as it is today; a German turn handed to a decoder that
    // speaks only Japanese is a new kind of wrong. Tiny is also a third of the
    // download and half base's RTF at 1.5 s.
    //
    // Re-measured on five languages for 0.11.6 (`spike/lid_cjk.py`, FINDINGS
    // §27) before Korean and Chinese were added as targets, because adding a
    // target adds a way for a German turn to be stolen. At 3 s: ja 96.5%,
    // ko 97.5%, zh 100.0%, and again **zero of 400** German and English
    // utterances heard as any of the three. The confusion that was expected —
    // ja↔zh, one script and much of a vocabulary — did not appear either
    // (0.5% one way, 0.0% the other); the cross-talk runs ja↔ko at 1.5–2.0%
    // and `asr_cjk::judge` reads the script rather than the reading, so it
    // costs nothing.
    //
    // Base is already on disk for anyone who fetched `--arbiter-de`, and is
    // still not reused — a feature whose accuracy depends on which *other*
    // optional group you happened to install is not one anybody can reason
    // about.
    //
    // Only the int8 encoder/decoder and the token table are checked; the
    // tarball also carries fp32 exports and test WAVs this daemon never reads.
    //
    // sha256 of the tarball:
    //   c46116994e539aa165266d96b325252728429c12535eb9d8b6a2b10f129e66b1
    RemoteAsset {
        role: LID_ROLE,
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-tiny.tar.bz2",
        download_bytes: 116_204_861,
        install: Install::TarBz2,
        group: Group::Japanese,
        files: &[
            (
                "sherpa-onnx-whisper-tiny/tiny-encoder.int8.onnx",
                12_937_772,
            ),
            (
                "sherpa-onnx-whisper-tiny/tiny-decoder.int8.onnx",
                89_855_401,
            ),
            ("sherpa-onnx-whisper-tiny/tiny-tokens.txt", 816_730),
        ],
    },
    // ---- the dedicated translator (0.11.0) --------------------------------
    //
    // Optional, and the flag is `models fetch --translator`. Three assets,
    // ~911 MB together, for a feature that already works without them: with the
    // translator absent, `[assist] translator` falls back to the graph model's
    // prompt, which is what 0.9.0 shipped.
    //
    // **Licence: CC-BY-NC 4.0.** NLLB-200 is published non-commercially. That
    // is fine for this daemon, which is one person's private install, and it is
    // a hard blocker for selling anything that bundles it — which is why this
    // is a separate opt-in group with the licence written next to the bytes
    // rather than a file quietly added to the default set.
    //
    // Measured (spike/nllb_bench.py, FINDINGS §23): 13 FLEURS directions, 100
    // parallel sentences each, scored by chrF against the human reference and
    // by the e5 cosine §16 used, against the shipped qwen2.5-3b prompt on the
    // same sentences and the same four pinned cores.
    //
    // The transformers.js mirror's int8 export, pinned to a commit so a
    // re-upload to `main` cannot change the catalogued size under us. The
    // decoder is the **merged** export — one graph carrying both the cache and
    // no-cache paths — because the alternative is two graphs and 915 MB for the
    // same thing; `crate::nllb` documents the one non-obvious thing about
    // driving it.
    RemoteAsset {
        role: TRANSLATOR_ROLE,
        url: "https://huggingface.co/Xenova/nllb-200-distilled-600M/resolve/261c31d1a5732c67cdd16d80e8d6088507c7ccea/onnx/encoder_model_quantized.onnx",
        download_bytes: 419_120_483,
        install: Install::File("nllb-200-distilled-600m-int8/encoder.onnx"),
        group: Group::Translator,
        files: &[("nllb-200-distilled-600m-int8/encoder.onnx", 419_120_483)],
    },
    RemoteAsset {
        role: TRANSLATOR_DECODER_ROLE,
        url: "https://huggingface.co/Xenova/nllb-200-distilled-600M/resolve/261c31d1a5732c67cdd16d80e8d6088507c7ccea/onnx/decoder_model_merged_quantized.onnx",
        download_bytes: 475_505_771,
        install: Install::File("nllb-200-distilled-600m-int8/decoder_merged.onnx"),
        group: Group::Translator,
        files: &[(
            "nllb-200-distilled-600m-int8/decoder_merged.onnx",
            475_505_771,
        )],
    },
    // The tokenizer is a third asset rather than part of the first because
    // upstream ships it separately, and the daemon refuses to load the model
    // without it: half an install is not a usable one.
    RemoteAsset {
        role: TRANSLATOR_TOKENIZER_ROLE,
        url: "https://huggingface.co/Xenova/nllb-200-distilled-600M/resolve/261c31d1a5732c67cdd16d80e8d6088507c7ccea/tokenizer.json",
        download_bytes: 17_331_224,
        install: Install::File("nllb-200-distilled-600m-int8/tokenizer.json"),
        group: Group::Translator,
        files: &[("nllb-200-distilled-600m-int8/tokenizer.json", 17_331_224)],
    },
    // ---- the memory graph's Tier 3 (GRAPH.md) -----------------------------
    //
    // Optional, and the flag is `models fetch --graph`. Two assets, ~1.95 GB
    // together, for a feature that ships switched off — so a fresh install
    // never pays for them and `models status` lists them as not required.
    //
    // The model is the bake-off's winner (spike/graph_bench, 20 gold cases
    // including 9 traps, four pinned cores at nice 19): Qwen2.5-3B-Instruct Q4
    // took 9/9 trap rejections and 9/9 on who-and-what, at 3.3 s/case. The 1.5B
    // was faster and gullible (5/9 traps); a missed promise costs a shrug and
    // an invented one poisons the feature.
    RemoteAsset {
        role: "graph.llm",
        url: "https://huggingface.co/bartowski/Qwen2.5-3B-Instruct-GGUF/resolve/main/Qwen2.5-3B-Instruct-Q4_K_M.gguf",
        download_bytes: 1_929_903_264,
        // Renamed on install for the same reason the embedding is: the config
        // default names this file, and that name must not drift with whatever
        // the upstream repository calls it this year.
        install: Install::File("qwen2.5-3b-instruct-q4_k_m.gguf"),
        group: Group::Graph,
        files: &[("qwen2.5-3b-instruct-q4_k_m.gguf", 1_929_903_264)],
    },
    // The runtime, as an upstream release binary. Deliberately not a build
    // dependency: `crate::llm` shells out to `llama-cli` exactly as the bake-off
    // harness did, so Tier 3 costs this crate no cmake, no C++ toolchain and no
    // new link-time anything. `llama-cli` and the shared objects it loads are
    // the only entries kept out of the archive's sixty-odd.
    RemoteAsset {
        role: "graph.runtime",
        url: "https://github.com/ggml-org/llama.cpp/releases/download/b10736/llama-b10736-bin-ubuntu-x64.tar.gz",
        download_bytes: 16_701_436,
        install: Install::TarGzInto {
            dir: "llama",
            keep: &["llama-cli", "lib"],
        },
        group: Group::Graph,
        files: &[
            ("llama/llama-cli", 1_453_352),
            ("llama/libllama.so", 4_529_552),
            ("llama/libggml-base.so", 920_216),
            ("llama/libllama-common.so", 6_089_880),
        ],
    },
];

/// The memory graph's Tier 3 assets, resolved on disk (GRAPH.md).
///
/// Separate from [`ModelSet`] on purpose: these are optional, and folding them
/// into the set the daemon checks at start-up would turn "I have not fetched a
/// 1.9 GB model I never asked for" into "analysis is incomplete".
#[derive(Debug, Clone)]
pub struct GraphModels {
    pub root: PathBuf,
    /// The GGUF.
    pub model: PathBuf,
    /// Where `llama-cli` and its shared objects live. Also what goes into
    /// `LD_LIBRARY_PATH` for the child, because the release binaries find each
    /// other by rpath-less name.
    pub lib_dir: PathBuf,
    pub cli: PathBuf,
}

impl GraphModels {
    pub fn resolve(root: &Path, cfg: &crate::config::GraphConfig) -> Self {
        let lib_dir = root.join(&cfg.llama_dir);
        Self {
            model: root.join(&cfg.llm_model),
            cli: lib_dir.join("llama-cli"),
            lib_dir,
            root: root.to_path_buf(),
        }
    }

    /// Both halves are on disk and the runner is executable. Anything less is
    /// "Tier 3 is not installed", which is a normal state and not an error.
    pub fn present(&self) -> bool {
        self.model.is_file() && self.cli.is_file()
    }

    /// What the graph's rows record as having produced them. The GGUF's own
    /// file stem, so a row written by one model is never confused with a row
    /// written by another — the same rule as `embed_model_id`.
    pub fn model_id(&self) -> String {
        stem(&self.model)
    }

    /// The optional entries `models status` lists under their own heading.
    pub fn entries(&self) -> Vec<ModelEntry> {
        [("graph.llm", &self.model), ("graph.runtime", &self.cli)]
            .into_iter()
            .map(|(role, path)| ModelEntry {
                role,
                path: path.clone(),
                expected: path
                    .strip_prefix(&self.root)
                    .ok()
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .as_deref()
                    .and_then(expected_bytes),
            })
            .collect()
    }
}

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

/// A Whisper export used as a **flip arbiter** (0.7.7): three files, and a
/// language that is forced on the decoder rather than read off the audio.
///
/// Deliberately not an [`AsrExport`]: Whisper has no joiner, its language is a
/// decoding *parameter* rather than a property of the weights, and it is never
/// the primary transcriber. Step 0 measured it at 4x the WER of Parakeet and
/// with a caption-style hallucination habit Parakeet does not have — which is
/// exactly why it is only ever asked a yes/no question about a fragment
/// somebody else already got wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WhisperExport {
    pub dir: &'static str,
    pub encoder: &'static str,
    pub decoder: &'static str,
    pub tokens: &'static str,
    /// The language token forced on every decode. Part of the model id, so a
    /// row re-read in German never claims to have been written by the same
    /// decoder as one re-read in English.
    pub lang: &'static str,
    pub note: &'static str,
}

impl WhisperExport {
    /// Stable identity of the words this arbiter produces, stored on the rows
    /// it rewrites. The forced language is in it because it is part of the
    /// decoding contract, not an observation.
    pub fn model_id(&self) -> String {
        format!("{}-{}@{ASR_CONTRACT_VERSION}", self.dir, self.lang)
    }
}

/// The German arbiter: Whisper base int8, forced to German. Optional, and the
/// only decoder in the catalogue that can produce German on demand.
pub const ARBITER_DE: WhisperExport = WhisperExport {
    dir: "sherpa-onnx-whisper-base",
    encoder: "base-encoder.int8.onnx",
    decoder: "base-decoder.int8.onnx",
    tokens: "base-tokens.txt",
    lang: "de",
    note: "Whisper base forced to German — 2-3% flips, 54-66% word precision at 1.5-3 s",
};

/// Where the German arbiter lives under a models root, and whether it is there.
///
/// Kept apart from [`ModelSet`] for the same reason [`SemanticModel`] is: its
/// absence is a normal state. Without it a suspected German flip is *flagged*
/// rather than re-read, which is exactly what 0.6.1 already did.
#[derive(Debug, Clone)]
pub struct ArbiterModel {
    pub root: PathBuf,
    pub export: WhisperExport,
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub tokens: PathBuf,
}

impl ArbiterModel {
    pub fn resolve_at(root: PathBuf, export: WhisperExport) -> Self {
        let dir = root.join(export.dir);
        Self {
            encoder: dir.join(export.encoder),
            decoder: dir.join(export.decoder),
            tokens: dir.join(export.tokens),
            export,
            root,
        }
    }

    pub fn entries(&self) -> Vec<ModelEntry> {
        [
            ("arbiter.encoder", &self.encoder),
            ("arbiter.decoder", &self.decoder),
            ("arbiter.tokens", &self.tokens),
        ]
        .into_iter()
        .map(|(role, path)| ModelEntry {
            role,
            path: path.clone(),
            expected: path
                .strip_prefix(&self.root)
                .ok()
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .as_deref()
                .and_then(expected_bytes),
        })
        .collect()
    }

    /// All three files at exactly the catalogued size. A truncated download is
    /// *absent*, not broken — the same rule every other model here follows.
    pub fn present(&self) -> bool {
        self.entries().iter().all(|e| e.ok())
    }

    pub fn model_id(&self) -> String {
        self.export.model_id()
    }

    /// What every "it is not installed" line says, so the CLI, the daemon's
    /// warning and `models status` all name the same command.
    pub fn how_to_get_it() -> String {
        format!(
            "the German arbiter is not installed. `recalld models fetch --arbiter-de` \
             installs {} ({}), and until then a German-looking flip is flagged rather \
             than re-read.",
            ARBITER_DE.dir,
            crate::fetch::human(arbiter_download_bytes()),
        )
    }
}

/// Where the cross-check decoder lives under a models root, and whether it is
/// there.
///
/// Kept apart from [`ModelSet`] like every other optional model: its absence is
/// a normal state, and what happens without it is that `asr_confidence` stays
/// null. A null there means "nothing checked these words", which is true, and
/// is a different thing from "checked and fine".
#[derive(Debug, Clone)]
pub struct ConfidenceModel {
    pub root: PathBuf,
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub tokens: PathBuf,
    /// Threads the recognizer is built with, from `[runtime].asr_threads`.
    pub threads: i32,
}

impl ConfidenceModel {
    pub fn resolve_at(root: PathBuf, threads: i32) -> Self {
        let dir = root.join(CONFIDENCE_DIR);
        Self {
            encoder: dir.join("encoder.int8.onnx"),
            decoder: dir.join("decoder.int8.onnx"),
            tokens: dir.join("tokens.txt"),
            threads,
            root,
        }
    }

    pub fn entries(&self) -> Vec<ModelEntry> {
        [
            ("confidence.encoder", &self.encoder),
            ("confidence.decoder", &self.decoder),
            ("confidence.tokens", &self.tokens),
        ]
        .into_iter()
        .map(|(role, path)| ModelEntry {
            role,
            path: path.clone(),
            expected: path
                .strip_prefix(&self.root)
                .ok()
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .as_deref()
                .and_then(expected_bytes),
        })
        .collect()
    }

    /// All three files at exactly the catalogued size, the same rule every
    /// other model here follows: a truncated download is *absent*, not broken.
    pub fn present(&self) -> bool {
        self.entries().iter().all(|e| e.ok())
    }

    /// What produced a confidence flag, with the source language in it: the
    /// same audio checked in German and in English is two different opinions,
    /// and a stored provenance that hid the difference would be a lie.
    pub fn model_id(&self, lang: &str) -> String {
        format!("{CONFIDENCE_DIR}-{lang}@{ASR_CONTRACT_VERSION}")
    }

    pub fn how_to_get_it() -> String {
        format!(
            "the cross-check decoder is not installed. `recalld models fetch --confidence` \
             installs {CONFIDENCE_DIR} ({}), and until then transcripts carry no \
             `asr_confidence`.",
            crate::fetch::human(confidence_download_bytes()),
        )
    }
}

/// The night shift's model and runtime on disk (0.9.0).
///
/// Two halves with different failure modes, which is why they are reported
/// apart: the model is an ordinary catalogued download and is either there at
/// the right size or absent, while `whisper-cli` is **built on this machine**
/// and its absence is the normal state of a fresh install.
#[derive(Debug, Clone)]
pub struct NightModels {
    pub root: PathBuf,
    /// The GGML file.
    pub model: PathBuf,
    /// The binary `crate::night` shells out to.
    pub cli: PathBuf,
    /// Where its shared objects live, for `LD_LIBRARY_PATH`.
    pub lib_dir: PathBuf,
}

impl NightModels {
    pub fn resolve_at(root: PathBuf, cfg: &crate::config::NightConfig) -> Self {
        let dir = root.join(&cfg.whisper_dir);
        Self {
            model: root.join(&cfg.model),
            cli: dir.join("whisper-cli"),
            lib_dir: dir,
            root,
        }
    }

    /// The model file at exactly the catalogued size, the rule every other
    /// download here follows.
    pub fn model_present(&self) -> bool {
        self.entries().iter().all(|e| e.ok())
    }

    /// The built runtime. No size check: this file came out of a compiler on
    /// this machine, not off a release page, so there is no byte count to
    /// compare it against and the honest test is "is it there and executable".
    pub fn runtime_present(&self) -> bool {
        self.cli.is_file()
    }

    pub fn present(&self) -> bool {
        self.model_present() && self.runtime_present()
    }

    pub fn entries(&self) -> Vec<ModelEntry> {
        vec![ModelEntry {
            role: NIGHT_ROLE,
            path: self.model.clone(),
            expected: expected_bytes(NIGHT_MODEL_FILE),
        }]
    }

    /// What produced a night transcript, with the model file in it, so a row
    /// rewritten by a different quantisation is distinguishable after the fact.
    pub fn model_id(&self) -> String {
        format!(
            "{}@{ASR_CONTRACT_VERSION}",
            self.model
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| NIGHT_MODEL_FILE.to_string())
        )
    }

    /// What every "the night shift cannot run" line says, so the CLI, the
    /// daemon's warning and `models status` all name the same two commands.
    pub fn how_to_get_it() -> String {
        format!(
            "the night shift is not installed. `recalld models fetch --night` downloads \
             {NIGHT_MODEL_FILE} ({}) and `recalld models build-night` compiles whisper.cpp \
             {NIGHT_WHISPER_TAG} into <models>/{NIGHT_DIR} — that one COMPILES, because no \
             GPU-capable whisper-cli is published for this card. Until both are there, \
             the night shift stays off.",
            crate::fetch::human(night_download_bytes()),
        )
    }
}

/// Bytes `models fetch --night` has to pull down.
pub fn night_download_bytes() -> u64 {
    REMOTE_ASSETS
        .iter()
        .filter(|a| a.group == Group::Night)
        .map(|a| a.download_bytes)
        .sum()
}

/// Bytes `models fetch --confidence` has to pull down.
pub fn confidence_download_bytes() -> u64 {
    REMOTE_ASSETS
        .iter()
        .filter(|a| a.group == Group::Confidence)
        .map(|a| a.download_bytes)
        .sum()
}

// ---- the CJK decoders, on disk (0.11.0, 0.11.6) ----------------------------

/// A single-graph offline export: one model file and a token table, and none of
/// [`AsrExport`]'s four.
///
/// Deliberately its own type rather than a fourth `AsrExport` for the same
/// reason [`WhisperExport`] is its own: the file layout is different, the
/// sherpa model config it fills in is different (`nemo_ctc` or `sense_voice`,
/// not `transducer`), and the languages it can write are a property of the
/// weights rather than a decoding parameter or a guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CtcExport {
    pub dir: &'static str,
    pub model: &'static str,
    pub tokens: &'static str,
    /// Which sherpa model config loads it, and therefore which languages
    /// [`crate::asr_cjk::judge`] will accept from it.
    ///
    /// Carried on the export rather than inferred from the directory name
    /// because getting it wrong is the one mistake in this area that neither
    /// fails to compile nor fails to load — it comes back as empty strings.
    pub decoder: crate::asr_cjk::Decoder,
    /// The languages a transcript from this export may be stamped with, in the
    /// catalogue's words rather than the router's. A fact about the weights.
    pub langs: &'static [&'static str],
    pub note: &'static str,
}

impl CtcExport {
    pub fn model_id(&self) -> String {
        format!("{}@{ASR_CONTRACT_VERSION}", self.dir)
    }
}

/// NVIDIA's Japanese Parakeet, CTC head, int8.
pub const JAPANESE_ASR: CtcExport = CtcExport {
    dir: JAPANESE_DIR,
    model: "model.int8.onnx",
    tokens: "tokens.txt",
    decoder: crate::asr_cjk::Decoder::Japanese,
    langs: &["ja"],
    note: "Japanese only — 11.3% CER at 3 s, CPU, live-path speed",
};

/// SenseVoice-Small int8: Korean and Chinese, and it writes Japanese too even
/// though nothing routes Japanese to it (FINDINGS §27).
pub const SENSE_VOICE_ASR: CtcExport = CtcExport {
    dir: SENSE_VOICE_DIR,
    model: "model.int8.onnx",
    tokens: "tokens.txt",
    decoder: crate::asr_cjk::Decoder::SenseVoice,
    langs: &["ko", "zh", "ja"],
    note: "Korean and Chinese — 9.6% CER each at 3 s, CPU",
};

/// The Whisper export the spoken-language identifier runs on. Encoder and
/// decoder only: LID reads the language token, so nothing ever asks it for
/// words and the token table it would need for them is not opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LidExport {
    pub dir: &'static str,
    pub encoder: &'static str,
    pub decoder: &'static str,
    pub note: &'static str,
}

pub const LID_WHISPER: LidExport = LidExport {
    dir: "sherpa-onnx-whisper-tiny",
    encoder: "tiny-encoder.int8.onnx",
    decoder: "tiny-decoder.int8.onnx",
    note: "Whisper tiny int8 — 96.5/97.5/100% ja/ko/zh recall at 3 s, 0/400 de+en false positives",
};

/// Where one CJK decoder lives under a models root, and whether it is there.
/// Kept apart from [`ModelSet`] like every other optional model: its absence is
/// a normal state, and what happens without it is that a Japanese, Korean or
/// Chinese turn stays transliterated — which is exactly what 0.10.3 did.
#[derive(Debug, Clone)]
pub struct CjkModel {
    pub root: PathBuf,
    pub export: CtcExport,
    pub model: PathBuf,
    pub tokens: PathBuf,
}

impl CjkModel {
    pub fn resolve_at(root: PathBuf, export: CtcExport) -> Self {
        let dir = root.join(export.dir);
        Self {
            model: dir.join(export.model),
            tokens: dir.join(export.tokens),
            export,
            root,
        }
    }

    pub fn entries(&self) -> Vec<ModelEntry> {
        let (model, tokens) = match self.export.decoder {
            crate::asr_cjk::Decoder::Japanese => ("japanese.model", "japanese.tokens"),
            crate::asr_cjk::Decoder::SenseVoice => ("cjk.model", "cjk.tokens"),
        };
        entries_under(&self.root, [(model, &self.model), (tokens, &self.tokens)])
    }

    /// Both files at exactly the catalogued size. A truncated download is
    /// *absent*, not broken — the rule every other model here follows.
    pub fn present(&self) -> bool {
        self.entries().iter().all(|e| e.ok())
    }

    pub fn model_id(&self) -> String {
        self.export.model_id()
    }

    /// The one line every "it is not installed" message says, for whichever of
    /// ja/ko/zh has no decoder on disk.
    ///
    /// Two flags because there are two downloads: `--japanese` is the 605 MB
    /// pair it has always been, and `--cjk` is that plus SenseVoice. Naming the
    /// smaller one when only Japanese is missing matters — telling somebody who
    /// wants Japanese to fetch 1.6 GB is how a correct message becomes an
    /// ignored one.
    pub fn how_to_get_it(missing: &[&str]) -> String {
        let only_ja = missing == ["ja"];
        let (flag, bytes) = if only_ja {
            ("--japanese", japanese_download_bytes())
        } else {
            ("--cjk", cjk_download_bytes())
        };
        format!(
            "no decoder is installed for {}. `recalld models fetch {flag}` installs it \
             and the language identifier beside it ({}), and until then such a turn comes \
             back transliterated into Latin letters that nothing downstream can detect.",
            missing.join("/"),
            crate::fetch::human(bytes),
        )
    }
}

/// Where the spoken-language identifier lives, and whether it is there.
#[derive(Debug, Clone)]
pub struct LidModel {
    pub root: PathBuf,
    pub export: LidExport,
    pub encoder: PathBuf,
    pub decoder: PathBuf,
}

impl LidModel {
    pub fn resolve_at(root: PathBuf, export: LidExport) -> Self {
        let dir = root.join(export.dir);
        Self {
            encoder: dir.join(export.encoder),
            decoder: dir.join(export.decoder),
            export,
            root,
        }
    }

    pub fn entries(&self) -> Vec<ModelEntry> {
        entries_under(
            &self.root,
            [
                ("lid.encoder", &self.encoder),
                ("lid.decoder", &self.decoder),
            ],
        )
    }

    pub fn present(&self) -> bool {
        self.entries().iter().all(|e| e.ok())
    }

    /// What decided a row's language, stored as `lang_via` evidence in the log
    /// and in `operations`. The export's own directory, so a reading taken by
    /// tiny is never confused with one taken by base.
    pub fn model_id(&self) -> String {
        format!("{}@{ASR_CONTRACT_VERSION}", self.export.dir)
    }
}

/// `ModelEntry` rows for a fixed list of `(role, path)`, each carrying whatever
/// size the catalogue has for it.
///
/// The four optional models before these two each spell this out inline —
/// `strip_prefix`, lossy-to-string, backslash fix, `expected_bytes` — and a
/// fifth and sixth copy is where one of them quietly stops checking sizes.
/// The existing four are deliberately left alone rather than swept into this
/// (a size check is not the place for a drive-by refactor); the two new ones
/// share it, and it is where the others should land next time one of them is
/// touched for its own reasons.
fn entries_under<'a>(
    root: &Path,
    items: impl IntoIterator<Item = (&'static str, &'a PathBuf)>,
) -> Vec<ModelEntry> {
    items
        .into_iter()
        .map(|(role, path)| ModelEntry {
            role,
            path: path.clone(),
            expected: path
                .strip_prefix(root)
                .ok()
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .as_deref()
                .and_then(expected_bytes),
        })
        .collect()
}

/// Bytes `models fetch --japanese` has to pull down: the decoder and the
/// identifier together, because the flag installs the pair or neither.
pub fn japanese_download_bytes() -> u64 {
    REMOTE_ASSETS
        .iter()
        .filter(|a| a.group == Group::Japanese)
        .map(|a| a.download_bytes)
        .sum()
}

/// Bytes `models fetch --cjk` has to pull down: both groups, because Japanese
/// is one of the three languages that flag promises and its decoder is the
/// Parakeet rather than SenseVoice (FINDINGS §27, rule (b)).
pub fn cjk_download_bytes() -> u64 {
    REMOTE_ASSETS
        .iter()
        .filter(|a| matches!(a.group, Group::Japanese | Group::Cjk))
        .map(|a| a.download_bytes)
        .sum()
}

/// Bytes `models fetch --arbiter-de` has to pull down.
pub fn arbiter_download_bytes() -> u64 {
    REMOTE_ASSETS
        .iter()
        .filter(|a| a.group == Group::ArbiterDe)
        .map(|a| a.download_bytes)
        .sum()
}

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

/// Total bytes the fetch has to pull down for a cold start: the default set,
/// plus whichever optional groups were actually asked for.
pub fn total_download_bytes(extra: &[Group]) -> u64 {
    REMOTE_ASSETS
        .iter()
        .filter(|a| a.default() || extra.contains(&a.group))
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

/// Where the optional text-embedding model lives, and whether it is there.
///
/// Kept apart from [`ModelSet`] on purpose. `ModelSet::complete()` is the gate
/// on the whole analysis leg — a missing file there costs the user their
/// transcripts — and semantic search is a *feature*: absent, everything else
/// works and keyword search is exactly what it was. Folding these two files
/// into `missing()` would turn "you have not opted into semantic search" into
/// "analysis is off", which is the wrong answer to the wrong question.
#[derive(Debug, Clone)]
pub struct SemanticModel {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub model: PathBuf,
    pub tokenizer: PathBuf,
}

impl SemanticModel {
    pub fn resolve(cfg: &ModelsConfig) -> Option<Self> {
        Some(Self::resolve_at(cfg.dir.clone()?, cfg))
    }

    pub fn resolve_at(root: PathBuf, cfg: &ModelsConfig) -> Self {
        let dir = root.join(&cfg.semantic);
        Self {
            model: dir.join(&cfg.semantic_model),
            tokenizer: dir.join(&cfg.semantic_tokenizer),
            dir,
            root,
        }
    }

    pub fn entries(&self) -> Vec<ModelEntry> {
        [
            (SEMANTIC_ROLE, &self.model),
            (SEMANTIC_TOKENIZER_ROLE, &self.tokenizer),
        ]
        .into_iter()
        .map(|(role, path)| ModelEntry {
            role,
            path: path.clone(),
            expected: path
                .strip_prefix(&self.root)
                .ok()
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .as_deref()
                .and_then(expected_bytes),
        })
        .collect()
    }

    /// Both files present at exactly the catalogued size. A half-installed or
    /// truncated model is *absent*, not broken: the daemon says "semantic
    /// search is not installed" and keyword search carries on.
    pub fn present(&self) -> bool {
        self.entries().iter().all(|e| e.ok())
    }

    /// Stable identity of the text embedding space, stored on every vector.
    pub fn model_id(&self) -> String {
        format!("{}@{SEMANTIC_CONTRACT_VERSION}", dir_name(&self.dir))
    }

    /// What `models status` prints when the files are not there — the whole
    /// point being that the toggle in the GUI, the status table and this line
    /// all name the same command.
    pub fn how_to_get_it() -> String {
        format!(
            "semantic search is not installed. `recalld models fetch --semantic` \
             installs {SEMANTIC_DIR} ({}), then `recalld semantic backfill` indexes \
             what has already been said.",
            crate::fetch::human(semantic_download_bytes())
        )
    }
}

/// Where the optional translator lives, and whether it is there (0.11.0).
///
/// Kept apart from [`ModelSet`] for exactly the reason [`SemanticModel`] is: a
/// machine that never opted into a 911 MB non-commercially-licensed translator
/// is not a broken install. Absent, `[assist] translator` falls back to the
/// graph model's prompt, which is what 0.9.0 shipped and still works.
#[derive(Debug, Clone)]
pub struct TranslatorModel {
    pub root: PathBuf,
    pub dir: PathBuf,
    pub encoder: PathBuf,
    pub decoder: PathBuf,
    pub tokenizer: PathBuf,
}

impl TranslatorModel {
    pub fn resolve(cfg: &ModelsConfig) -> Option<Self> {
        Some(Self::resolve_at(cfg.dir.clone()?))
    }

    pub fn resolve_at(root: PathBuf) -> Self {
        let dir = root.join(TRANSLATOR_DIR);
        Self {
            encoder: dir.join(TRANSLATOR_ENCODER_FILE),
            decoder: dir.join(TRANSLATOR_DECODER_FILE),
            tokenizer: dir.join(TRANSLATOR_TOKENIZER_FILE),
            dir,
            root,
        }
    }

    pub fn entries(&self) -> Vec<ModelEntry> {
        [
            (TRANSLATOR_ROLE, &self.encoder),
            (TRANSLATOR_DECODER_ROLE, &self.decoder),
            (TRANSLATOR_TOKENIZER_ROLE, &self.tokenizer),
        ]
        .into_iter()
        .map(|(role, path)| ModelEntry {
            role,
            path: path.clone(),
            expected: path
                .strip_prefix(&self.root)
                .ok()
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .as_deref()
                .and_then(expected_bytes),
        })
        .collect()
    }

    /// All three files present at exactly the catalogued size. A truncated or
    /// half-installed translator is *absent*, not broken: translation carries
    /// on through the other backend.
    pub fn present(&self) -> bool {
        self.entries().iter().all(|e| e.ok())
    }

    /// Stable identity of what produced a translation, stored in
    /// `translation.via` on every row this backend writes.
    pub fn model_id(&self) -> String {
        format!("{}@{TRANSLATOR_CONTRACT_VERSION}", dir_name(&self.dir))
    }

    pub fn how_to_get_it() -> String {
        format!(
            "the dedicated translator is not installed. `recalld models fetch --translator` \
             installs {TRANSLATOR_DIR} ({}), which is NLLB-200-distilled-600M under \
             CC-BY-NC 4.0 — free for a personal install, not for anything sold.",
            crate::fetch::human(translator_download_bytes())
        )
    }
}

/// Bytes `models fetch --translator` has to pull down.
pub fn translator_download_bytes() -> u64 {
    REMOTE_ASSETS
        .iter()
        .filter(|a| a.group == Group::Translator)
        .map(|a| a.download_bytes)
        .sum()
}

/// Bytes `models fetch --semantic` has to pull down.
pub fn semantic_download_bytes() -> u64 {
    REMOTE_ASSETS
        .iter()
        .filter(|a| a.role == SEMANTIC_ROLE || a.role == SEMANTIC_TOKENIZER_ROLE)
        .map(|a| a.download_bytes)
        .sum()
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

    /// The flip arbiter under this set's root (0.7.7). Resolving it here rather
    /// than from the config is what keeps the arbiter, the primary ASR and the
    /// English fallback all coming out of one directory.
    pub fn arbiter(&self, export: WhisperExport) -> ArbiterModel {
        ArbiterModel::resolve_at(self.root.clone(), export)
    }

    /// The Japanese decoder under this set's root (0.11.0), resolved the same
    /// way and for the same reason the arbiter is: one directory holds every
    /// decoder this daemon can reach for.
    pub fn japanese(&self) -> CjkModel {
        CjkModel::resolve_at(self.root.clone(), JAPANESE_ASR)
    }

    /// The Korean and Chinese decoder under this set's root (0.11.6).
    pub fn sense_voice(&self) -> CjkModel {
        CjkModel::resolve_at(self.root.clone(), SENSE_VOICE_ASR)
    }

    /// The spoken-language identifier under this set's root (0.11.0).
    pub fn lid(&self) -> LidModel {
        LidModel::resolve_at(self.root.clone(), LID_WHISPER)
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
