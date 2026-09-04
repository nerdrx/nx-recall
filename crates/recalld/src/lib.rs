//! NX Recall capture daemon.
//!
//! Step 1 is capture: PipeWire per-application audio, an allowlist, Silero VAD,
//! SQLite. Steps 2 and 3 add the analysis leg on top of the same inference
//! thread: overlap segmentation, ASR, and speaker identity. Step 4 adds the
//! control socket (`server` / `service` / `proto` / `bus`), global pause
//! (`control`), the VRChat roster (`roster`) and retention (`retention`).
//!
//! The crate is a library so the acceptance suite in `tests/` can drive the
//! real pipeline against the golden fixtures; `src/main.rs` is a thin bin.

// `status` is one `json!` literal and it grows by a few keys every release;
// 0.8.0's counters pushed it past the default 128 levels of macro recursion.
// Raised rather than split: the whole point of that literal is that a person
// can read the daemon's whole answer in one place.
#![recursion_limit = "256"]

pub mod accuracy;
pub mod allowlist;
pub mod analysis;
// ---- 0.11.0, grounded answers ----
pub mod answer;
// ---- end 0.11.0 ----
pub mod arbiter;
pub mod ask;
// ---- 0.9.0, the assistant ------------------------------------------------
pub mod asr;
pub mod asr_cjk;
pub mod assist;
pub mod b64;
pub mod bridge;
pub mod brief;
pub mod bus;
// ---- 0.11.0: learned identity ---------------------------------------------
pub mod calib;
// ---- end 0.11.0 -----------------------------------------------------------
pub mod canary;
pub mod capture;
pub mod client;
pub mod clock;
pub mod commitment;
pub mod config;
pub mod control;
pub mod digest;
pub mod embed;
pub mod enrich;
/// Local Markdown export (0.10.0). DESIGN §12: files on this disk, nothing else.
pub mod export;
pub mod fetch;
pub mod identity;
/// Where the audio came from, as evidence about who is on it (0.11.0).
pub mod identity_prior;
// ---- 0.11.0: learned identity ---------------------------------------------
pub mod identity_learn;
// ---- end 0.11.0 -----------------------------------------------------------
pub mod ingest;
pub mod lang;
/// Generated character-trigram tables for [`lang::guess_by_trigram`] (0.11.0).
/// Written by `spike/short_lang_bench.py --emit`; never edited by hand.
#[rustfmt::skip]
pub mod lang_ngrams;
pub mod langctx;
pub mod lid;
pub mod llm;
pub mod models;
/// 0.12.4: how a turn sounded — SenseVoice's own emotion and event tags,
/// read off the stored clips by a background pass. See `docs/PROTOCOL.md`
/// and `spike/FINDINGS.md` §42 for what of it is measured and what is only
/// stored.
pub mod mood;
pub mod night;
pub mod nllb;
pub mod notes;
pub mod overlap;
pub mod palette;
pub mod partial;
/// 0.12.1: one Discord user's own audio, as its own source.
pub mod peruser;
pub mod pipeline;
pub mod polyglot;
pub mod proto;
pub mod proximity;
pub mod quality;
pub mod queue;
pub mod reminders;
pub mod replay;
pub mod resample;
pub mod retention;
/// The room microphone (0.10.0): a second, physical input.
pub mod room;
pub mod roster;
pub mod semantic;
pub mod server;
pub mod service;
pub mod split;
pub mod store;
// ---- 0.12.0 --------------------------------------------------------------
/// The archive language sweep: the identifier, over the rows captured before
/// there was one.
pub mod sweep;
// ---- end 0.12.0 ----------------------------------------------------------
pub mod threads;
pub mod timeref;
pub mod translate;
/// Ground truth from Discord (0.9.0): the labelling pass and the measurement.
pub mod truth;
/// The truth ingest's loopback HTTP listener (0.9.0).
pub mod truthnet;
// ---- end 0.9.0 -----------------------------------------------------------
/// Taking back the rows the audio-language route should not have rewritten
/// (0.12.0).
pub mod unroute;
// ---- end 0.12.0 ---------------------------------------------------------
pub mod turns;
// ---- 0.10.0, worlds and turn-taking --------------------------------------
/// Turn-taking statistics: how a person talks (0.10.0).
pub mod turntaking;
pub mod vad;
pub mod vocab;
/// World memory: where a conversation happened (0.10.0).
pub mod worlds;
// ---- end 0.10.0 ----------------------------------------------------------

/// ~640 KB, bundled so the daemon has no runtime asset lookup. The heavier
/// analysis models are *not* bundled — they live in `[models].dir`.
pub const VAD_MODEL: &[u8] = include_bytes!("../models/silero_vad.onnx");
