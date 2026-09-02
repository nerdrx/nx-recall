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

    // ---- 0.10.0, the room microphone -------------------------------------
    /// Turn the room microphone on or off, or point it at a device.
    #[command(long_about = "\
Turn the room microphone on or off, or point it at a device.

This is a SECOND, physical microphone: the desk mic that hears the people
sitting in the room with you, who never joined the instance. Unlike the headset
microphone, its voices are matched against the voicebank and enrolled like
anybody else's — nothing on this device is labelled as you.

It is off by default and it needs a device: there is no sensible default for a
second input, and following the system default would open the headset the
`recalld mic` switch is already on.

  recalld devices                     the capture devices you can pick from
  recalld room --device alsa_input.x  pin the device, changing nothing else
  recalld room on                     enable it (needs a device first)
  recalld room follow                 enable it, only while an allowed app runs
  recalld room always                 enable it, whenever the daemon runs
  recalld room off                    disable it

Needs the running daemon: the switch is live, and every connected client has to
be told.")]
    Room {
        #[arg(value_enum, default_value_t = MicAction::Status)]
        action: MicAction,
        /// The PipeWire `node.name` to capture, from `recalld devices`.
        #[arg(long, value_name = "NODE_NAME")]
        device: Option<String>,
    },

    /// List the capture devices on the PipeWire graph, with the `node.name`
    /// the room microphone is pinned by. Captures nothing.
    Devices,

    /// Write the transcript to Markdown files in a folder on this disk.
    #[command(long_about = "\
Write the transcript to Markdown files in a folder on this disk.

One file per day (2026-09-01.md) plus people.md, in the directory you name. That
directory is the whole output: this writes files to your disk and nothing else —
there is no upload, no share, no link, and the path is refused if it is not an
absolute local one (a network mount is a share, whatever the file manager calls
it).

Files NX Recall wrote are rewritten; a file it did not write is never touched —
it refuses and names the file, because the folder is yours.

  recalld export ~/notes/recall
  recalld export ~/notes/recall --from 2026-08-01 --to 2026-09-01
  recalld export ~/notes/recall --speaker 7

Runs in this process against the database directly, so it works whether or not
the daemon is running.")]
    Export {
        /// An absolute path to an existing directory on a local filesystem.
        #[arg(value_name = "DIR")]
        dir: PathBuf,
        /// ISO-8601 instant or date, inclusive.
        #[arg(long, value_name = "WHEN")]
        from: Option<String>,
        /// ISO-8601 instant or date, exclusive.
        #[arg(long, value_name = "WHEN")]
        to: Option<String>,
        /// Only this voice's turns.
        #[arg(long, value_name = "SPEAKER_ID")]
        speaker: Option<i64>,
        /// Only this conversation.
        #[arg(long, value_name = "THREAD_ID")]
        thread: Option<i64>,
        /// Write the assistant's translations under the turns that have one.
        #[arg(long)]
        translations: bool,
        /// Say what would be written, and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    // ---- end 0.10.0 -------------------------------------------------------
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

    /// The conversational language prior: what it has flagged, and re-reading
    /// the backlog now that there is something to re-read it with.
    // Verbatim: clap would reflow the example block into one paragraph.
    #[command(long_about = "\
The conversational language prior (0.7.7).

A conversation has a language. When ten turns running read as German and the
eleventh comes back as English, the eleventh is far more likely to be the
decoder flipping than the room switching — measured at 12% of 1 s fragments and
5% of 2 s ones, against a median real turn of 2.4 s.

A turn the classifier could not read at all takes the conversation's language.
A turn that reads as the OPPOSITE is handed to an arbiter: a decoder told which
language to hear. When there is no arbiter installed, or the turn is under
1.5 s (below which re-decoding is measurably not an improvement), the words are
kept and the row is flagged instead.

  recalld lang              how many turns are flagged, and what can settle them
  recalld lang repair       re-read the flagged ones from their audio

Repair is bounded, resumable and runs at idle priority: it is safe to run while
the daemon is capturing, and a run that is interrupted loses nothing.

`recalld models fetch --arbiter-de` installs the German arbiter (~208 MB);
`recalld models fetch --confidence` installs the cross-check decoder (~154 MB);
`--fallback-asr` installs the English one (~108 MB).")]
    Lang {
        #[command(subcommand)]
        action: Option<LangAction>,
    },

    // ---- 0.11.0: source-aware identity ------------------------------------
    /// Where each voice has been heard, and which labels the source-aware
    /// prior would have questioned.
    // Verbatim: clap would reflow the two halves into one paragraph.
    #[command(long_about = "\
Where each voice has been heard, and which labels that history argues against.

Some voices are only ever present on Discord, some on Discord and in VRChat,
some only through your own microphone. A voice heard eight hundred times on
Discord and never once in VRChat should not win a VRChat turn at 0.36 — and
until 0.11.0 it could, because the voicebank was asked one question about the
whole world at once.

`audit` prints three things and changes nothing:

  the matrix     every voice against every source it has been heard on
  the count      labels that pointed at a voice with no PRIOR history on that
                 source, judged by replaying the labels in the order they were
                 made — the only reading of the question that is not circular
  the tail       the twenty most recent of those, with their scores

`repair --foreign` takes those labels back to unassigned — never to another
voice, because the finding is an argument against the label a row has and not
for any other one. It previews by default; `--apply` writes.

The prior itself is off until `[identity].source_prior = true`.")]
    Identity {
        #[command(subcommand)]
        action: Option<IdentityAction>,
    },
    // ---- end 0.11.0 -------------------------------------------------------
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
        /// Search by meaning as well as by word, and fuse the two. Needs the
        /// semantic model — `recalld models fetch --semantic`.
        #[arg(long)]
        smart: bool,
    },

    /// Semantic search: the index behind "what did she say about that world"
    /// when you cannot remember the words.
    Semantic {
        #[command(subcommand)]
        action: SemanticAction,
    },

    /// Stop writing anything, instantly. Capture keeps running; no segments,
    /// no audio files, no transcripts, no roster. The scripting half of the
    /// pause surfaces — the others are the tray dropdown and the GUI.
    Pause,

    /// Start writing again.
    Resume,

    /// Ask the running daemon how it is doing.
    Status,

    /// The memory graph: what has been derived, and whether the local model is
    /// allowed to run.
    // Verbatim: clap would otherwise reflow the example block into a paragraph.
    #[command(long_about = "\
The memory graph (docs/GRAPH.md): who owes what to whom, and what conversations
were about.

Tiers 1 and 2 are deterministic and always on — conversation threads, time
references, and rule-based promise candidates. They cost nothing and they are
marked as guesses wherever they appear.

Tier 3 is a 3B local model, and it is OFF by default. Turning it on spends four
CPU cores at nice 19 on conversations nobody has looked at yet, and only while
no captured application is running and capture is not paused. Nothing leaves the
machine, at any tier.

  recalld graph            what has been derived, and what the worker is doing
  recalld graph on         allow the local model to run when the machine is idle
  recalld graph off        stop it
  recalld graph commitments   open promises, soonest first
  recalld graph topics        the conversation labels in use

`recalld models fetch --graph` installs the model (~1.9 GB); until then
`recalld graph on` reports it as not installed rather than pretending to work.

Needs the running daemon: the switch is live and every client has to be told.")]
    Graph {
        #[arg(value_enum, default_value_t = GraphAction::Status)]
        action: GraphAction,
    },

    // ---- 0.8.0, the product round -------------------------------------
    /// Ask the transcript a question in your own words.
    // Verbatim: clap would reflow the examples into one paragraph.
    #[command(long_about = "\
Ask the transcript a question in your own words.

One box. The daemon reads the question apart — a named voice, a time reference
in German or English, and whatever is left as the search — and tells you what it
understood before it shows you the answers, so a facet it got wrong is visible
rather than silent.

  recalld ask \"was hat Aspen gestern über den Shader gesagt\"
  recalld ask \"what did Kira mention about the fountain last week\"
  recalld ask \"Aspens Meinung zum Portal am Montag\"

Time words are read BACKWARDS here: a question is about what has already been
said, so \"am Montag\" is the Monday that happened, not the one coming. With the
semantic model installed the search is hybrid; without it, keyword only. Either
way the reply says which ran.")]
    Ask {
        #[arg(value_name = "QUESTION")]
        question: Vec<String>,
        /// Maximum hits to print.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },

    /// Notes to self: what you said into the microphone with a wake phrase in
    /// front of it.
    // Verbatim: the wake phrases are a list and clap would fuse them.
    #[command(long_about = "\
Notes to self.

A MICROPHONE turn that opens with a wake phrase is filed as a note. The turn
stays in the transcript; the note is an annotation pointing at it.

  \"Recall, merk dir …\"      \"Recall, notiz …\"
  \"Recall, remember …\"      \"Recall, note …\"

  recalld notes             what is still open
  recalld notes all         everything, including what you have finished with
  recalld notes done 3      tick one off
  recalld notes dismiss 3   it was never a note

Nothing but one of these commands ever moves a note off `open`.")]
    Notes {
        #[command(subcommand)]
        action: Option<NotesAction>,
    },

    /// What is still outstanding with one person: the thirty seconds before
    /// you say hello.
    Brief {
        #[arg(value_name = "SPEAKER_ID")]
        speaker_id: i64,
    },

    /// How wrong the transcripts were, measured from the corrections you made
    /// to them. A biased sample by construction — you correct what matters —
    /// and the only real measurement this machine has.
    Accuracy,

    /// Ground truth from Discord: the token the Vencord plugin needs, who has
    /// been linked to which voice, and how right the voicebank actually is.
    ///
    /// Discord's own client knows who is talking. Recall records the mixed
    /// call and has to guess. This is the two being compared.
    Truth {
        #[command(subcommand)]
        action: Option<TruthAction>,
    },

    // ---- 0.9.0, the assistant ------------------------------------------
    /// One paragraph per conversation, for a day.
    ///
    /// The local model writes these when it has read everything else it owes
    /// you; `recalld graph on` is what turns it on, and a machine that has
    /// never fetched it prints nothing at all.
    #[command(long_about = "\
One paragraph per conversation that has settled, for a day.\n\n\
DAY is a local calendar day (2026-09-02), or one of `today` and `yesterday`.\n\
With no DAY at all, the most recent conversations come back whichever day they\n\
happened on.\n\n\
Conversations the model read and declined to summarise are not listed. That is\n\
not a failure: eight turns of \"ja / ne / lol\" is a conversation by the\n\
threading rule and nothing worth a paragraph, and refusing it is what the\n\
verdict-first grammar is for.")]
    Digest {
        /// A local calendar day, `today`, or `yesterday`.
        #[arg(value_name = "DAY")]
        day: Option<String>,
    },
    // ---- end 0.9.0 -------------------------------------------------------

    // ---- 0.10.0, worlds and turn-taking ----------------------------------
    /// How one person talks: their share, their turns, who interrupts whom.
    ///
    /// Every number is a query over turns that already exist — nothing is
    /// stored and nothing is guessed by a model. Two of them are
    /// approximations with named failure modes, and the command prints the
    /// definition under the number rather than making you look it up.
    #[command(
        long_about = "How one person talks, over every conversation they have taken part in.

`--days N` restricts it to the last N days.

INTERRUPTIONS and LATENCY are approximations and the output says how. An
interruption is a turn that starts while somebody else is still talking AND
whose own audio holds overlapped speech; the overlap proves two people were
audible, not which two, and it cannot tell a genuine interruption from a
back-channel \"mhm\". Latency is the median gap from the previous speaker's
turn ending to theirs starting, over gaps of at most five seconds — longer
ones are dropped rather than clamped, because past that it is a lull and not
a reply."
    )]
    Stats {
        #[arg(value_name = "SPEAKER_ID")]
        speaker_id: i64,
        /// Only the last N days.
        #[arg(long, value_name = "N")]
        days: Option<i64>,
    },

    /// Every world a conversation has happened in.
    Worlds {
        /// How many to list.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    // ---- end 0.10.0 ------------------------------------------------------
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
pub enum TruthAction {
    /// Print the bearer token the plugin needs, generating one if there is
    /// none yet. Paste it into Vencord → Plugins → RecallBridge.
    Token,
    /// Turn the loopback ingest on. Edits the config; restart `recalld run`.
    On,
    /// Turn the loopback ingest off.
    Off,
    /// Every Discord account heard so far, and the voice it is linked to.
    Users,
    /// Say that a Discord account is a particular voice.
    Link {
        #[arg(value_name = "USER_ID")]
        user_id: String,
        #[arg(value_name = "SPEAKER_ID")]
        speaker_id: i64,
    },
    /// Take a link back.
    Unlink {
        #[arg(value_name = "USER_ID")]
        user_id: String,
    },
    /// How right the voicebank was, marked by Discord.
    Report,
}

#[derive(Subcommand, Debug)]
pub enum NotesAction {
    /// Every note, whatever state it is in. The default lists only open ones.
    All,
    /// Tick one off.
    Done {
        #[arg(value_name = "NOTE_ID")]
        id: i64,
    },
    /// It was never a note.
    Dismiss {
        #[arg(value_name = "NOTE_ID")]
        id: i64,
    },
    /// Put one back on the list.
    Reopen {
        #[arg(value_name = "NOTE_ID")]
        id: i64,
    },
}

#[derive(Subcommand, Debug)]
pub enum IdentityAction {
    /// The report: the voice × source matrix, the count of labels the rule
    /// questions, and the most recent of them. Changes nothing. The default.
    Audit,
    /// Take questioned labels back to unassigned. Previews unless `--apply`.
    Repair {
        /// Required, and the only selector there is. Naming it is the point:
        /// this command must never grow a mode that rewrites anything else.
        #[arg(long)]
        foreign: bool,
        /// Actually write. Without it the command only lists.
        #[arg(long)]
        apply: bool,
        /// Stop after this many rows.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
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

    /// Delete one voice: its conversations always, and — unless you keep the
    /// voiceprint — the identity behind them too.
    // Verbatim: clap would reflow the two halves into one paragraph, and the
    // difference between them is the whole point of the command.
    #[command(long_about = "\
Delete one voice.

Both halves soft-delete every conversation the voice still has, so the undo
window applies exactly as it does to any other delete. What differs is whether
the voiceprint survives:

  recalld speakers delete 7                    the voice goes too: prototypes,
                                               embeddings and golden samples are
                                               removed and it must enrol again
  recalld speakers delete 7 --keep-voiceprint  the words go, the identity stays
                                               and keeps being labelled

A voice with no conversations left is still deletable — that is the case this
exists for: once the segments are gone there is nothing left to delete BY, and
the voiceprint would otherwise go on matching new audio for ever.

Refused for your own pinned voice (turn the microphone off instead) and, on the
nuke path, for a voice other voices were merged into.

Needs the running daemon: every connected client has to be told.")]
    Delete {
        #[arg(value_name = "SPEAKER_ID")]
        speaker_id: i64,
        /// Keep the voice in the bank: delete the conversations only, and go on
        /// labelling this voice in future.
        #[arg(long)]
        keep_voiceprint: bool,
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

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphAction {
    /// What has been derived and what the worker is doing. Changes nothing.
    Status,
    /// Allow the local model to run while the machine is idle.
    On,
    /// Stop it. Everything already derived stays; nothing new is written.
    Off,
    /// Open promises, soonest due first.
    Commitments,
    /// The conversation labels in use.
    Topics,
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

        /// Also install the memory graph's local model (~1.9 GB) and the
        /// llama.cpp binaries that run it. Optional, and the feature it serves
        /// ships switched off — nothing needs these until `recalld graph on`.
        #[arg(long)]
        graph: bool,

        /// Also install the German flip arbiter (~208 MB): Whisper base, run
        /// with its language token forced to German. Optional — without it a
        /// German turn decoded as English is flagged rather than re-read,
        /// which is what the daemon did before 0.7.7.
        #[arg(long)]
        arbiter_de: bool,

        /// Also install the transcript cross-check decoder (~154 MB): Canary
        /// 180m, run as a second opinion whose agreement with the primary
        /// decoder becomes `asr_confidence`. Optional — without it transcripts
        /// carry no confidence flag at all, which is honest and quiet.
        #[arg(long)]
        confidence: bool,

        /// Also install the night shift's GGML model (~1.03 GB, 0.9.0):
        /// whisper-large-v3, read over the day's shaky rows on the GPU while
        /// the machine is nobody's. Optional, and only half of what the night
        /// shift needs — `recalld models build-night` compiles the runtime,
        /// because upstream publishes no GPU-capable whisper-cli for this card.
        #[arg(long)]
        night: bool,

        /// Also install the text-embedding model that semantic search needs
        /// (~135 MB). Not part of the default set: keyword search works
        /// without it, and it is a feature you opt into rather than something
        /// the daemon needs to be correct.
        #[arg(long)]
        semantic: bool,

        /// Do not write the resulting directory into config.toml. Without this
        /// the fetch points `[models].dir` at what it just installed, so
        /// `models status` and the daemon agree with it.
        #[arg(long)]
        no_config: bool,
    },

    /// Build the night shift's GPU decoder from source (0.9.0).
    ///
    /// The one asset in this program that COMPILES rather than downloads, and
    /// it is honest about why: whisper.cpp publishes no release binary with a
    /// GPU backend for an AMD card, so a `whisper-cli` that can use one has to
    /// be built on the machine that will run it. Needs git, cmake, a C++
    /// compiler and either the Vulkan headers and glslc, or hipBLAS.
    ///
    /// Clones whisper.cpp at a pinned tag into the models directory, builds it,
    /// and installs `whisper-cli` and its shared objects into
    /// `<models>/whisper`. Safe to re-run: an existing binary is left alone
    /// unless `--force` is given.
    BuildNight {
        /// Build into this directory instead of `[models].dir`.
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,

        /// The GPU backend to build. `vulkan` needs the Vulkan headers and
        /// `glslc`; `hip` needs hipBLAS and rocBLAS, which are a much larger
        /// install and are not present on every ROCm machine. `cpu` builds a
        /// working binary at roughly 17x real time, which is why the night
        /// shift was parked as a CPU feature in the first place — it exists
        /// here for a machine with no usable GPU backend, and the daemon warns
        /// when it is what got built.
        #[arg(long, value_name = "BACKEND", default_value = "vulkan")]
        backend: NightBackend,

        /// Build even if `whisper-cli` is already installed.
        #[arg(long)]
        force: bool,

        /// Parallel compile jobs. Defaults to half the machine's cores, at nice
        /// 19: a build that takes the whole box is a build nobody starts twice.
        #[arg(long, value_name = "N")]
        jobs: Option<usize>,
    },
}

/// Which whisper.cpp backend `models build-night` compiles.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum NightBackend {
    Vulkan,
    Hip,
    Cpu,
}

impl NightBackend {
    pub fn as_str(self) -> &'static str {
        match self {
            NightBackend::Vulkan => "vulkan",
            NightBackend::Hip => "hip",
            NightBackend::Cpu => "cpu",
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum LangAction {
    /// How many transcripts are flagged as a language the daemon could not
    /// settle, and which arbiters are installed to settle them. Changes
    /// nothing.
    Status,

    /// Re-read every flagged transcript from its audio, applying exactly the
    /// guards the live pipeline applies.
    // Verbatim: the operating model is two paragraphs and clap would fuse them.
    #[command(long_about = "\
Re-read every flagged transcript from its audio.

A flagged row is one whose words disagreed with the language expected of them —
from the speaker's declaration, or from the conversation around it — at a moment
when nothing could settle the disagreement: no arbiter installed, or an arbiter
whose own answer failed a guard. The words were kept and the row was marked.

This walks those marks. The target language is re-derived NOW, not read off the
old mark, because a declaration may have been added since and a conversation may
have grown a context it did not have. Rows under 1.5 s are left flagged: below
that the arbiter's word precision is 28% and replacing one wrong transcript with
another is not a correction. Rows whose audio the retention window has taken are
skipped — there is nothing left to re-read.

Bounded, resumable and idle-priority: the work list is a query, not a cursor.")]
    Repair {
        /// Rows per batch. Progress is reported once per batch.
        #[arg(long, value_name = "N", default_value_t = 32)]
        batch: usize,

        /// Stop after this many rows. Without it the walk runs to completion.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,

        /// Use this models directory instead of `[models].dir`.
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum SemanticAction {
    /// Report what semantic search has indexed, and what it still owes.
    Status,

    /// Embed every transcript that has no current vector.
    // Verbatim: the two paragraphs below are the whole operating model and
    // clap would reflow them into one.
    #[command(long_about = "\
Embed every transcript that has no current vector.

Segments captured from now on are embedded as they are transcribed. This is for
everything said BEFORE the model was installed — and for anything whose words
have changed since, because a corrected transcript and a re-decoded one both
make the old vector a wrong answer waiting to be given.

Resumable by construction: the work list is a query, not a cursor, so a run that
is interrupted loses at most one batch and the next run picks up the rest. It
runs at idle priority and holds the database only one batch at a time, so it is
safe to run while the daemon is capturing.")]
    Backfill {
        /// Segments per transaction.
        #[arg(long, value_name = "N", default_value_t = 128)]
        batch: usize,

        /// Stop after this many segments. Without it the backfill runs to
        /// completion.
        #[arg(long, value_name = "N")]
        limit: Option<usize>,

        /// Use this models directory instead of `[models].dir`.
        #[arg(long, value_name = "PATH")]
        dir: Option<PathBuf>,
    },
}
