//! Command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "recalld",
    version,
    about = "NX Recall capture daemon: per-application audio capture, VAD segmentation, \
             transcription, speaker identity"
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

    /// Inspect the analysis models.
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },

    /// List known voices with how much they have said.
    Speakers,

    /// Give a voice a name. Retroactive by nature: the numeric id is the
    /// identity, so every past and future segment follows.
    Name {
        #[arg(value_name = "SPEAKER_ID")]
        speaker_id: i64,
        #[arg(value_name = "DISPLAY_NAME")]
        display_name: String,
    },

    /// Merge two voices that turned out to be the same person: <A> is
    /// tombstoned and everything it owns moves to <B>.
    Merge {
        #[arg(value_name = "FROM")]
        from: i64,
        #[arg(value_name = "INTO")]
        into: i64,
    },

    /// Full-text search over transcripts.
    Search {
        #[arg(value_name = "QUERY")]
        query: Vec<String>,
        /// Maximum hits to print.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },

    /// Chronological transcript dump.
    Transcript {
        /// Restrict to one capture session.
        #[arg(long, value_name = "N")]
        session: Option<i64>,
        /// Restrict to one speaker, by id or display name.
        #[arg(long, value_name = "SPEAKER")]
        speaker: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ModelsAction {
    /// Report which analysis models are present and which are missing.
    /// Downloads nothing.
    Status,
}
