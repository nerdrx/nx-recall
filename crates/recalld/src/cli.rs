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

    /// Split a voice that turned out to be two people: re-cluster what the
    /// speaker's identity rests on, keep <ID> for the larger half and mint a
    /// new voice for the other. Refused if the two halves are one person.
    /// Needs the running daemon, because every client has to be told.
    Split {
        #[arg(value_name = "SPEAKER_ID")]
        speaker_id: i64,
    },

    /// Full-text search over transcripts.
    Search {
        #[arg(value_name = "QUERY")]
        query: Vec<String>,
        /// Maximum hits to print.
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },

    /// Stop writing anything, instantly. Capture keeps running; no segments,
    /// no audio files, no transcripts, no roster. The scripting half of the
    /// pause surfaces — the others are the tray dropdown and the GUI.
    Pause,

    /// Start writing again.
    Resume,

    /// Ask the running daemon how it is doing.
    Status,

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
    Status {
        /// Report on this directory instead of `[models].dir`.
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
    },

    /// Download the analysis models (~140 MB compressed, ~700 MB on disk) into
    /// the data directory. This is the only command in recalld that touches the
    /// network, and it is a setup step: capture and transcription never do.
    ///
    /// Safe to re-run — files already present at the expected size are skipped.
    Fetch {
        /// Install into this directory instead of `[models].dir` (default:
        /// `<data-dir>/models`).
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,

        /// Download everything again, even files that are already correct.
        #[arg(long)]
        force: bool,

        /// Do not write the resulting directory into config.toml. Without this
        /// the fetch points `[models].dir` at what it just installed, so
        /// `models status` and the daemon agree with it.
        #[arg(long)]
        no_config: bool,
    },
}
