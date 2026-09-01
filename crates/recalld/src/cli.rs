//! Command-line surface.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

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

    /// Turn the microphone on or off, or ask what it is doing.
    // clap reflows a doc comment into paragraphs, which turns the example block
    // below into one long line. `long_about` is taken verbatim.
    #[command(long_about = "\
Turn the microphone on or off, or ask what it is doing.

The microphone is the one source that is NOT an application rule: it hears the
ROOM, not a program, so it is off by default and has its own switch. In the
default `follow` mode it only records while an allowed application is itself
being captured.

  recalld mic          what it is doing right now
  recalld mic on       enable it, keeping the current mode
  recalld mic follow   enable it, only while an allowed app is captured
  recalld mic always   enable it, whenever the daemon is running
  recalld mic off      disable it

Needs the running daemon: the switch is live, and every connected client has to
be told.")]
    Mic {
        #[arg(value_enum, default_value_t = MicAction::Status)]
        action: MicAction,
    },

    /// Inspect the analysis models.
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },

    /// List known voices with how much they have said, or sweep the one-off
    /// ones out of the voicebank.
    Speakers {
        #[command(subcommand)]
        action: Option<SpeakersAction>,
    },

    /// Say which languages a voice speaks, so a wrong-language transcript can
    /// be corrected instead of merely noticed.
    // The doc comment is taken verbatim: clap would otherwise reflow the
    // example block into one paragraph.
    #[command(long_about = "\
Say which languages a voice speaks.

The multilingual ASR does not merely fail to identify a language on short
fragments — it picks the wrong one and commits. Measured on German read speech
cut to lobby-sized windows: 12% of 1 s fragments and 5% of 2 s ones come back
reading as English, against a median real turn of 2.4 s.

Knowing a voice speaks only English makes that fixable: a German-looking
transcript from that voice is re-decoded with the English-only model, which
cannot produce German at all. The other direction is only flagged — there is no
German-constrained decoder in the model catalogue yet.

  recalld languages 7 en        this voice speaks English only
  recalld languages 7 de,en     bilingual: nothing is ever corrected
  recalld languages 7 any       clear it (the default)

Needs the running daemon: every connected client has to be told.")]
    Languages {
        #[arg(value_name = "SPEAKER_ID")]
        speaker_id: i64,
        /// Comma-separated tags (`de`, `en`, `de,en`), or `any` to clear.
        #[arg(value_name = "CODES")]
        codes: String,
    },

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
pub enum SpeakersAction {
    /// Sweep voices that are almost certainly not people: at most one segment
    /// and under three seconds of speech in total. Lists them by default and
    /// changes nothing; `--apply` deletes them, cascading exactly as a
    /// delete-by-speaker does. Never touches a named voice or your own.
    Prune {
        /// Actually delete them. Without this the command only lists.
        #[arg(long)]
        apply: bool,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MicAction {
    /// Enable the microphone, keeping whichever mode is configured.
    On,
    /// Disable the microphone.
    Off,
    /// Enable it in follow mode: only while an allowed application is captured.
    Follow,
    /// Enable it in always mode: whenever the daemon is running.
    Always,
    /// Report the current state, changing nothing.
    Status,
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

    /// Download the analysis models (~500 MB compressed, ~700 MB on disk) into
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

        /// Also install the older English-only ASR export (~103 MB). Not part
        /// of the default set: the multilingual default transcribes English
        /// better as well. It exists as the fallback for machines that already
        /// have it.
        #[arg(long)]
        fallback_asr: bool,

        /// Do not write the resulting directory into config.toml. Without this
        /// the fetch points `[models].dir` at what it just installed, so
        /// `models status` and the daemon agree with it.
        #[arg(long)]
        no_config: bool,
    },
}
