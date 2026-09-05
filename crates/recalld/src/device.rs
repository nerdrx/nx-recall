//! Which device each model on the live path runs on, and why.
//!
//! Every model this daemon loads runs on the CPU. That is not an oversight and
//! it is not a missing feature flag — it was measured, and the measurement
//! (FINDINGS §40) is that **there is no route to this machine's GPU for the
//! models that cost anything**. This module is that finding, written where a
//! client and a person reading `recalld status` can see it, so the next person
//! to ask "why isn't the 7900 XTX doing this?" gets the answer in one command
//! instead of re-running the round.
//!
//! The night shift is the exception and it is a real one: `crate::night` runs
//! whisper.cpp on Vulkan, through a binary `recalld models build-night`
//! compiles. That works because whisper.cpp has a Vulkan backend. Nothing on
//! the live path does.
//!
//! # The two reasons
//!
//! The live models split by which runtime they go through, and each runtime is
//! blocked for a different reason:
//!
//! * **sherpa-onnx** (Parakeet, ERes2Net, canary, the language identifier, the
//!   CJK decoders) accepts exactly seven provider strings — `cpu`, `cuda`,
//!   `coreml`, `xnnpack`, `nnapi`, `trt`, `directml` — and there is no AMD one
//!   to pass. Not "not built": the enum has no variant. Giving it one means
//!   patching upstream C++.
//! * **onnxruntime through `ort`** (Silero VAD, the overlap detector) *does*
//!   have AMD execution providers in the binding, and neither is reachable
//!   here. The ROCm one was removed from ONNX Runtime in 1.23 and the symbol is
//!   absent from every library AMD still publishes. The MIGraphX one exists,
//!   but the only fetchable library carrying it is ONNX Runtime 1.23.2 — which
//!   refuses the API version this crate is built against — and its provider
//!   needs `libmigraphx_c.so`, which pulls MIOpen, rocBLAS, hipBLASLt and
//!   Composable Kernel into `/opt/rocm`: about 19 GB, system-wide.
//!
//! # Why this is not a config switch
//!
//! Because it would be a switch with one position. FINDINGS §40 measured the
//! live path per model and **87% of its CPU is Parakeet**, which is behind the
//! first reason above and unreachable at any price short of patching
//! sherpa-onnx. The two models that *are* behind `ort` — the ones a MIGraphX
//! runtime could theoretically move — are **1.5% of the bill between them**. A
//! `live_gpu` setting could therefore, at its absolute best, and after a 19 GB
//! system-wide install, save one and a half percent of a cost that is already
//! a third of one core. There is no honest `on` to offer.

use serde_json::{Value, json};

/// One model on the live path.
pub struct Placement {
    /// What the model is called in the design docs.
    pub model: &'static str,
    /// The library that executes it.
    pub runtime: Runtime,
    /// Share of the live path's CPU, measured in FINDINGS §40. Reported so a
    /// client can order the list by what would actually be worth moving.
    pub cpu_share_pct: f32,
}

/// The two inference libraries the live path uses, and the reason each one is
/// stuck on the CPU on an AMD machine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Runtime {
    SherpaOnnx,
    Ort,
}

impl Runtime {
    pub fn as_str(self) -> &'static str {
        match self {
            Runtime::SherpaOnnx => "sherpa-onnx",
            Runtime::Ort => "onnxruntime",
        }
    }

    /// Why this runtime is on the CPU, in one sentence a person can act on.
    pub fn why(self) -> &'static str {
        match self {
            Runtime::SherpaOnnx => {
                "sherpa-onnx accepts no AMD execution provider — its provider enum has \
                 cpu, cuda, coreml, xnnpack, nnapi, trt and directml and nothing else, \
                 so this is upstream C++ and not a build flag"
            }
            Runtime::Ort => {
                "onnxruntime's ROCm provider was removed in 1.23, and its MIGraphX \
                 provider needs a runtime that is not fetchable at this crate's API \
                 version and would pull roughly 19 GB of ROCm math libraries into \
                 /opt/rocm system-wide"
            }
        }
    }
}

/// The live path, in the order a turn goes through it.
///
/// `cpu_share_pct` is from FINDINGS §40 — 22.8 minutes of the user's own
/// segments, the shipping configuration. It is a constant rather than a live
/// counter on purpose: it exists to say *what would be worth moving*, which is
/// a property of the models, and a per-turn timer on the live path to maintain
/// a number nobody reads is exactly the kind of cost this round is about.
pub const LIVE: &[Placement] = &[
    Placement {
        model: "silero-vad",
        runtime: Runtime::Ort,
        cpu_share_pct: 0.8,
    },
    Placement {
        model: "pyannote-segmentation-3.0",
        runtime: Runtime::Ort,
        cpu_share_pct: 0.7,
    },
    Placement {
        model: "parakeet-tdt-0.6b-v3",
        runtime: Runtime::SherpaOnnx,
        cpu_share_pct: 87.5,
    },
    Placement {
        model: "eres2net",
        runtime: Runtime::SherpaOnnx,
        cpu_share_pct: 11.0,
    },
];

/// The `devices` block of `status.asr`.
///
/// `night` is the one true "gpu" in the program and it is reported beside the
/// live models rather than only under `status.asr.night`, because the question
/// a person actually asks is "is my graphics card doing anything for this", and
/// an answer that omits the one thing that uses it is a misleading answer.
///
/// `light` is `None` on a daemon with no analysis models resolved — there is
/// no transcriber for the switch to apply to — and `Some((model_id, reason))`
/// otherwise, which is which decoder the transcriber line above actually is
/// right now and why (0.13.x, `crate::light`).
pub fn status_json(night_available: bool, light: Option<(&str, &str)>) -> Value {
    let models: Vec<Value> = LIVE
        .iter()
        .map(|p| {
            json!({
                "model": p.model,
                "runtime": p.runtime.as_str(),
                "device": "cpu",
                "cpu_share_pct": p.cpu_share_pct,
                "why": p.runtime.why(),
            })
        })
        .collect();
    json!({
        "live": "cpu",
        "live_models": models,
        "night": if night_available { "vulkan" } else { "unavailable" },
        "light": light.map(|(model, reason)| json!({ "model": model, "why": reason })),
        "summary": "every model on the live path runs on the CPU. The 87% of that \
                    cost which is the transcriber goes through sherpa-onnx, which has \
                    no AMD execution provider at all; the two models that could in \
                    principle move are 1.5% of it. Measured in FINDINGS §40. The GPU \
                    is used by the night shift, through whisper.cpp on Vulkan.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shares_are_a_whole_path() {
        let total: f32 = LIVE.iter().map(|p| p.cpu_share_pct).sum();
        assert!(
            (total - 100.0).abs() < 1.0,
            "the live path's shares must add up to the live path, got {total}"
        );
    }

    /// The argument this module exists to make: the models a GPU runtime could
    /// reach are not the models that cost anything. If a future round moves
    /// this line, the refusal in the docs has to be revisited with it.
    #[test]
    fn the_reachable_models_are_not_worth_reaching() {
        let reachable: f32 = LIVE
            .iter()
            .filter(|p| p.runtime == Runtime::Ort)
            .map(|p| p.cpu_share_pct)
            .sum();
        assert!(
            reachable < 5.0,
            "onnxruntime drives {reachable}% of the live path — over 5% and the \
             MIGraphX route is worth costing again"
        );
    }

    #[test]
    fn every_model_says_which_device_and_why() {
        let v = status_json(true, None);
        assert_eq!(v["live"], "cpu");
        assert_eq!(v["night"], "vulkan");
        let models = v["live_models"].as_array().expect("an array");
        assert_eq!(models.len(), LIVE.len());
        for m in models {
            assert_eq!(m["device"], "cpu");
            assert!(m["why"].as_str().is_some_and(|s| !s.is_empty()));
            assert!(m["runtime"].as_str().is_some_and(|s| !s.is_empty()));
        }
    }

    #[test]
    fn a_machine_without_the_vulkan_build_says_so() {
        assert_eq!(status_json(false, None)["night"], "unavailable");
    }

    #[test]
    fn no_analysis_models_means_no_light_block() {
        assert_eq!(status_json(true, None)["light"], Value::Null);
    }

    #[test]
    fn light_mode_says_which_model_is_live_and_why() {
        let v = status_json(
            true,
            Some(("parakeet-tdt-110m", "a captured source is a game")),
        );
        assert_eq!(v["light"]["model"], "parakeet-tdt-110m");
        assert_eq!(v["light"]["why"], "a captured source is a game");
    }
}
