//! NX Recall capture daemon.
//!
//! Step 1 is capture: PipeWire per-application audio, an allowlist, Silero VAD,
//! SQLite. Steps 2 and 3 add the analysis leg on top of the same inference
//! thread: overlap segmentation, ASR, and speaker identity.
//!
//! The crate is a library so the acceptance suite in `tests/` can drive the
//! real pipeline against the golden fixtures; `src/main.rs` is a thin bin.

pub mod allowlist;
pub mod analysis;
pub mod asr;
pub mod capture;
pub mod clock;
pub mod config;
pub mod embed;
pub mod identity;
pub mod ingest;
pub mod models;
pub mod overlap;
pub mod pipeline;
pub mod queue;
pub mod resample;
pub mod store;
pub mod turns;
pub mod vad;

/// ~640 KB, bundled so the daemon has no runtime asset lookup. The heavier
/// analysis models are *not* bundled — they live in `[models].dir`.
pub const VAD_MODEL: &[u8] = include_bytes!("../models/silero_vad.onnx");
