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

pub mod allowlist;
pub mod analysis;
pub mod asr;
pub mod b64;
pub mod bus;
pub mod capture;
pub mod client;
pub mod clock;
pub mod config;
pub mod control;
pub mod embed;
pub mod fetch;
pub mod identity;
pub mod ingest;
pub mod lang;
pub mod models;
pub mod overlap;
pub mod pipeline;
pub mod proto;
pub mod proximity;
pub mod queue;
pub mod resample;
pub mod retention;
pub mod roster;
pub mod semantic;
pub mod server;
pub mod service;
pub mod split;
pub mod store;
pub mod threads;
pub mod turns;
pub mod vad;

/// ~640 KB, bundled so the daemon has no runtime asset lookup. The heavier
/// analysis models are *not* bundled — they live in `[models].dir`.
pub const VAD_MODEL: &[u8] = include_bytes!("../models/silero_vad.onnx");
