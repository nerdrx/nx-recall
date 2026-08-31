//! Command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "recalld",
    version,
    about = "NX Recall capture daemon: per-application audio capture, VAD segmentation, storage"
)]
pub struct Cli {
    /// Config file (default: $XDG_CONFIG_HOME/nx-recall/config.toml).
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Data directory (default: $XDG_DATA_HOME/nx-recall).
    #[arg(long, global = true, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the capture daemon in the foreground.
    Run,

    /// List the application playback streams currently on the PipeWire graph,
    /// with the identity we would tag them by and whether they are allowlisted.
    /// Captures nothing.
    Probe,

    /// Show every source seen so far and its allow flag.
    Sources,

    /// Allow capture of a source, by match key (usually the process binary,
    /// or the PE name for Wine programs).
    Allow {
        #[arg(value_name = "MATCH_KEY")]
        match_key: String,
    },

    /// Stop capturing a source.
    Deny {
        #[arg(value_name = "MATCH_KEY")]
        match_key: String,
    },
}
