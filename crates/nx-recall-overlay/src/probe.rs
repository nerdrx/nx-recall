//! What the runtime on this machine will actually let us do.
//!
//! This is the whole of the honest part of the headset story. An overlay that
//! draws captions into somebody's OpenXR session needs `XR_EXTX_overlay`, and
//! whether that exists is a fact about the runtime rather than about our code —
//! so it is measured, printed, and allowed to say no.
//!
//! The probe deliberately stops at `xrCreateInstance`. Enumerating extensions
//! does not start a session, does not open a device, and does not touch a
//! headset: it is safe to run while somebody is in VR, which matters, because
//! the person most likely to run it is somebody wondering why their captions
//! are not there.

use anyhow::{Context, Result};

/// Everything the probe found, in the order a person wants to read it.
pub struct Probe {
    pub runtime: String,
    pub runtime_version: String,
    pub api_version: String,
    /// Every extension the runtime advertises, sorted.
    pub extensions: Vec<String>,
    /// The one that decides whether route 1 exists at all.
    pub overlay: bool,
}

impl Probe {
    pub fn run() -> Result<Self> {
        // `linked()` uses the loader that was linked at build time; `load()`
        // dlopens the system one. The system loader is the right answer here —
        // it is the one that reads the active_runtime.json a person's headset
        // software actually wrote, which is precisely what we are measuring.
        let entry = unsafe { openxr::Entry::load() }
            .context("no OpenXR loader on this machine (install `openxr` / libopenxr_loader)")?;

        let available = entry
            .enumerate_extensions()
            .context("the loader is here but no runtime answered — is one installed and active?")?;

        // The crate models the extension list as a struct of bools, which is
        // convenient and lossy: a runtime can advertise something this build of
        // the crate has never heard of. The raw list is the honest report, so
        // it is read back out of the same struct's Debug shape rather than
        // invented — every field that is true, by its published name.
        let extensions = enabled_names(&available);
        let overlay = available.extx_overlay;

        // An instance with NO extensions: enough to ask the runtime its NAME,
        // and the cheapest thing the loader will do.
        //
        // It is deliberately allowed to fail. Under WiVRn (and Monado
        // generally) the extension list comes out of the runtime library
        // itself, but creating an instance needs the compositor SERVICE to be
        // up — so on a machine where nobody is in VR right now this call is
        // refused while the measurement above has already succeeded. Treating
        // that as fatal would mean the probe could only ever be run at the one
        // moment it is least welcome to run: mid-session.
        let app = openxr::ApplicationInfo {
            application_name: "nx-recall-overlay",
            application_version: 0,
            engine_name: "nx-recall",
            engine_version: 0,
            api_version: openxr::Version::new(1, 0, 0),
        };
        let (runtime, runtime_version) =
            match entry.create_instance(&app, &openxr::ExtensionSet::default(), &[]) {
                Ok(instance) => match instance.properties() {
                    Ok(p) => (p.runtime_name.clone(), p.runtime_version.to_string()),
                    Err(e) => (format!("<xrGetInstanceProperties: {e}>"), String::new()),
                },
                Err(e) => (
                    format!("<not running: {e}>"),
                    "the extension list above is still the runtime's own".to_owned(),
                ),
            };

        Ok(Self {
            runtime,
            runtime_version,
            api_version: format!("{}", openxr::Version::new(1, 0, 0)),
            extensions,
            overlay,
        })
    }

    /// The report, as a person reads it — and as a bug report quotes it.
    pub fn print(&self) {
        println!("runtime      {} {}", self.runtime, self.runtime_version);
        println!("api          {}", self.api_version);
        println!("extensions   {}", self.extensions.len());
        for name in &self.extensions {
            println!("  {name}");
        }
        println!();
        if self.overlay {
            println!("XR_EXTX_overlay: PRESENT — an overlay session is possible here.");
            println!("Run `nx-recall-overlay --overlay` with an OpenXR application already running.");
        } else {
            println!("XR_EXTX_overlay: ABSENT.");
            println!();
            println!("This runtime will not host an overlay session, so there is no way for this");
            println!("binary to draw into somebody else's OpenXR frame. The supported route on");
            println!("this machine is to mirror the desktop captions window into the headset with");
            println!("wlx-overlay-s — see docs/OVERLAY.md. Nothing about that needs this binary.");
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "runtime": self.runtime,
            "runtime_version": self.runtime_version,
            "api_version": self.api_version,
            "extx_overlay": self.overlay,
            "extensions": self.extensions,
        })
    }
}

/// Every extension the runtime advertises, by published name.
///
/// The `openxr` crate parses the runtime's list into a struct of booleans whose
/// field names are the extension names lowercased with `XR_` stripped, so the
/// names can be recovered exactly — and, more importantly, the ones this build
/// of the crate does NOT know about are recovered too, as `other`. A probe that
/// silently dropped those would under-report the runtime, which is the one
/// thing a probe must not do.
fn enabled_names(set: &openxr::ExtensionSet) -> Vec<String> {
    // Debug for ExtensionSet prints `field: true/false` for every known
    // extension plus `other: [b"…"]` for the rest. Parsing it is unlovely and
    // it is still the only lossless read the crate offers: the struct is
    // `#[non_exhaustive]`, has no iterator and no name table.
    //
    // The field names are the published names mechanically transformed —
    // `XR_EXTX_overlay` becomes `extx_overlay` — so the inverse is exact:
    // uppercase the vendor tag before the first underscore, leave the rest.
    let dump = format!("{set:#?}");
    let mut out: Vec<String> = Vec::new();
    for line in dump.lines() {
        let line = line.trim().trim_end_matches(',');
        let Some((field, "true")) = line.split_once(": ") else {
            continue;
        };
        let Some((tag, rest)) = field.split_once('_') else {
            continue;
        };
        out.push(format!("XR_{}_{rest}", tag.to_uppercase()));
    }
    // Extensions this build of the crate has never heard of. A probe that
    // dropped these would under-report the runtime, which is the one thing a
    // probe must not do.
    for name in &set.other {
        let bytes: Vec<u8> = name.iter().copied().take_while(|b| *b != 0).collect();
        out.push(String::from_utf8_lossy(&bytes).into_owned());
    }
    out.sort();
    out.dedup();
    out
}
