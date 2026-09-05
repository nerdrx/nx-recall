//! SQLite storage.
//!
//! Schema v1 (Step 1) was `sources` / `sessions` / `segments`. v2 adds the
//! analysis columns, the voicebank (`speakers`, `speaker_prototypes`,
//! `embeddings`, `golden_samples`) and a full-text index over transcripts.
//! v3 (Step 4) adds `session_roster` — who was in the instance, from VRChat's
//! output log — and `operations`, the audit trail that makes a rename, a merge
//! or a reassignment undoable. v4 adds `sources.kind`, because a source is no
//! longer always an application (the microphone is one too), and `settings`,
//! the small key/value table that pins the "You" speaker across restarts.
//! v5 adds `speakers.languages` (which languages this voice actually speaks,
//! so a wrong-language decode can be corrected rather than merely noticed) and
//! two provenance columns on `segments`: `label_via`, how the speaker got
//! there, and `lang_via`, how the language did.
//! v6 adds `threads` and `segments.thread_id` — the memory graph's Tier 1
//! (docs/GRAPH.md): which conversation a turn belongs to, derived
//! deterministically from turn-taking adjacency (`crate::threads`). It is the
//! only derived table in the schema, it is re-derivable from the transcript,
//! and the migration backfills it by replaying the same rule the live path
//! uses.
//! v7 adds the memory graph's Tiers 2 and 3: `time_refs` (when a turn was
//! talking about), `commitments` (who owes what to whom) and `threads.topic`.
//! All three are **derived, second-class and re-derivable** — every row carries
//! its provenance (`source`, `model_id`, `confidence`, `created_at`) and every
//! one of them is deleted when the segment, speaker or thread it hangs off is.
//! v9 adds `segment_vectors` — one sentence-embedding per transcribed turn, so
//! search can answer "what did she say about that world" without the words
//! (DESIGN §6/§7). The whole migration lives in `crate::semantic::migrate_v9`;
//! it is standalone (one table, three indexes, no backfill) and reads nothing
//! any other migration writes, so it applies to a v6, v7 or v8 database alike.
//! Existing databases are migrated in place.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::embed::Embedding;
use crate::threads::{OpenThread, RECENT_SPEAKERS, Threader, Turn};

// ---- 0.8.0 (schema v10) ---------------------------------------------------
// v10 adds four columns on `segments` (text provenance and the cross-check
// verdict, see `apply_v10`) and the `notes` table (a mic turn that opened with
// a wake phrase, see `apply_v10_notes`). Both halves are idempotent and
// independent; there is no backfill of either.
// ---- 0.9.0 (schema v11) ---------------------------------------------------
// v11 adds ground truth from Discord: `truth_speaking` (who was talking, when,
// as the Discord client itself saw it), `discord_users` (a Discord account,
// optionally linked to a voice), four columns on `segments` carrying the
// verdict that comparison reached, and one on `speaker_prototypes` saying a
// prototype was enrolled on Discord's word. See `apply_v11`; it is idempotent
// and there is no backfill — truth only exists from the day the plugin starts
// sending it.
//
// ---- 0.9.0 (schema v11), the night shift ----------------------------------
// v11 adds two columns on `segments` (`night_text`, `night_at_ns`, see
// `apply_v11_night`) for the overnight third reading. Additive, idempotent, no
// backfill: a NULL `night_at_ns` means the night shift has not looked at the
// row, which is true of every row written before it existed.
// v11 is the assistant round, and it is entirely additive: two columns on
// `notes` (when a reminder is due and when it fired), two on `segments` (a
// translation and the model that wrote it) and one new table, `digests`. See
// `apply_v11`. No backfill of any of it — a note captured before v11 had no
// due date to lose, and a translation nobody has computed is correctly absent.
//
// ---- 0.10.0 (schema v12): worlds -----------------------------------------
// v12 adds `visits` (one row per VRChat world entry, closed by the next one)
// and `threads.world_id` (the visit that was open when the conversation
// began). Both live in `crate::worlds`; both are additive and idempotent, and
// neither is backfilled from anything already in the database. The turn-taking
// half of 0.10.0 adds no schema at all — every number it reports is a query
// over turns that were already there.
//
// ---- 0.11.6 (schema v13): the simultaneous fraction -----------------------
// v13 adds `segments.truth_overlap_frac`: what share of a turn two or more
// Discord users were speaking across *at the same time*, as opposed to the
// `overlap` verdict's "two users each covered a fifth of it somewhere". The
// column lives in `crate::truth::migrate_v13`, which also backfills it for
// verdicts already on disk — but only where the speaking spans survive, since
// a purged span and a quiet turn would otherwise both read 0.0.
//
// ---- 0.12.0 (schema v14): which instance a session was ---------------------
// v14 adds `sessions.instance_key`: which *copy* of an application opened the
// session, from `object.serial` or `application.process.id`. Two Vesktop
// clients share one `sources` row because the key is the process binary, and
// §29 is what that costs when only one of them carries the ground-truth
// plugin. Nullable, no backfill — for a session already on disk the answer is
// unknown and NULL is the only honest way to say so. See
// `Store::begin_session_for`, including why the discriminator is not allowed
// anywhere near `match_key`.
//
// ---- 0.12.0 (schema v15): highlighted people ------------------------------
// v15 adds `speakers.colour` and `speakers.icon`: a palette token and a short
// emoji a person pins to a voice so it can be picked out of a wall of names.
// Both nullable, both NULL for every voice that exists today, and NULL is the
// whole of "not highlighted" — which is why there is no backfill and nothing to
// undo. `colour` is a token from `crate::palette`, not a hex, so the same
// highlight is legible on both of NX Clear's grounds and in the headset
// overlay, which has no CSS to resolve one with. See `apply_v15`.
//
// ---- 0.12.3 (schema v17): which bridge sent a speaking span ----------------
// Two nullable columns on `truth_speaking`: `account_id` (the Discord account
// the RecallBridge plugin was signed in as) and `client_kind` (`vesktop`,
// `discord`, `web`). They exist because the user runs TWO Discord clients, in
// two different calls, and once both carry the plugin every span from both
// arrives in one table. A verdict computed over "every span that overlaps this
// turn" then mixes two conversations, and nothing on disk could say so.
//
// **NULL is not a default, it is a fact**: it means the line came from a plugin
// that predates the field, i.e. from the only bridge there was, so a NULL span
// is evidence about whatever was being recorded and every scope matches it.
// There is no backfill for the same reason there was none for v11 — the answer
// for a row already on disk is genuinely unknown. See `apply_v17`.
//
// ---- 0.12.4 (schema v18): how a turn sounded ------------------------------
// Three nullable columns on `segments`, written by `crate::mood`'s background
// pass and by nothing else: `mood` (one of `crate::mood::Mood`'s four words, or
// NULL when the model abstained), `events` (a sorted, comma-separated subset of
// `crate::mood::Event`'s closed set, or NULL for none) and `mood_at_ns` (when
// the pass looked).
//
// **`mood_at_ns` is the queue and the other two are the answer**, which is why
// there are three columns and not two. The pass is resumable in the shape every
// other walk in this daemon is — `WHERE mood_at_ns IS NULL` — and it stamps the
// row whatever it concluded, including "the model had no opinion" and
// "retention has taken the audio". Without the third column a row the model
// abstained on would be indistinguishable from a row nothing had reached, and
// the pass would re-read it every night for the life of the archive.
//
// No backfill, for the usual reason: for a row already on disk nobody has
// listened, and NULL is the only honest way to say so. See `apply_v18`.
// ---- 0.12.4 (schema v19): every correction is word-level ground truth ------
// One table, `text_truth`. When a person retypes a line they produce the only
// reference transcript this machine will ever have for that turn, and until
// now it existed only as a `segments.correct` operation — recoverable, but
// only by walking a log and joining three other tables to find out which
// decoders had read the same audio. `text_truth` is that join, written once,
// at the moment the truth is made: the corrected words, what each pass had
// read (`live_text`, `context_text`, `night_text`, `canary_text`), the
// cross-check verdict standing over the words being replaced, and the three
// facets a measurement is cut by — voice, source kind, duration.
//
// It is **derived** data and it is deliberately not a cache: nothing reads it
// to render a transcript, and dropping the table loses no user-visible state.
// It is backfilled on migration from the operations history, which is where
// every one of its fields already lived. See `apply_v19` and
// `crate::text_truth`.
//
// v18 belongs to a sibling build in the same release; this one is v19 so the
// two never claim the same number. The migration chain is unconditional and
// idempotent, so a v17 database moving straight to v19 is the ordinary path.
pub const SCHEMA_VERSION: i64 = 19;

/// `sources.kind` for an application playback stream — the only kind before v4.
pub const KIND_APP: &str = "app";
/// `sources.kind` for the user's own microphone.
pub const KIND_MIC: &str = "mic";
/// `sources.kind` for the room microphone (0.10.0): a second, physical input
/// that hears the people sitting in the room rather than the user.
///
/// No migration comes with it. `kind` has been a free-text column since v4 and
/// a row is written with this value the first time the room tap exists; a
/// client that has never heard of it renders the row by its `kind` string,
/// which is exactly what the versioning rule asks of it.
pub const KIND_ROOM: &str = "room";
/// `sources.kind` for ONE Discord user's own audio stream (0.12.1).
///
/// Not a device and not an application: a row per account, fed by the
/// RecallBridge plugin over the truth ingest from the `MediaStream` Vesktop
/// gives it for that person. Its defining property is that the audio is
/// **single-speaker by construction** — there is no mixture to un-mix and
/// nothing to identify, because the stream *is* the identity.
///
/// Like [`KIND_ROOM`] it needs no migration: `kind` has been free text since
/// v4, and a client that has never heard of it renders the row by its string.
/// Unlike every other kind, rows of it are created on demand — the first frame
/// for an account creates its source — so there can be as many as there are
/// people the user has been in a call with.
pub const KIND_DISCORD_USER: &str = "discord-user";

/// The source kinds whose turns may join a conversation from ANOTHER session.
///
/// The microphone bridges because the user is one voice across every session
/// (2026-09-02: pinning them to their own session made every thread a
/// monologue). The room bridges for the same reason and it is the same reason
/// literally: the room and the headset are one physical evening, and a person
/// sitting next to the user answering somebody in the instance is in that
/// conversation whatever device carried their voice.
///
/// A per-user Discord stream bridges for the third time and the same reason
/// (0.12.1), and here it is not merely defensible but required: one Discord
/// call is now N sessions, one per person in it, and a conversation that could
/// not cross a session boundary would render an entire call as N parallel
/// monologues that never answer each other.
///
/// **`Store::segment_turn` repeats this test in SQL.** Adding a kind here and
/// not there compiles, passes this function's own assertions, and silently
/// does not bridge.
pub fn kind_bridges_threads(kind: &str) -> bool {
    kind == KIND_MIC || kind == KIND_ROOM || kind == KIND_DISCORD_USER
}

/// `segments.label_via` — how this row's *speaker* came to be what it is (v5).
/// Before v5 the same distinction was carried implicitly by `match_score`
/// being NULL, which could not tell a microphone pin from a hand reassignment;
/// this says it out loud.
pub mod label_via {
    /// The voicebank matched it. The default, and what every pre-v5 labelled
    /// row is backfilled to.
    pub const MATCH: &str = "match";
    /// The user's own microphone: provenance, never a comparison (DESIGN §5).
    pub const MIC: &str = "mic";
    /// Inherited from the confident turns either side of it (0.6.1). Carries
    /// `match_score` NULL, because nothing was compared — and a client must
    /// render it as uncertain, because nothing was heard either.
    pub const PROXIMITY: &str = "proximity";
    /// A person said so.
    pub const MANUAL: &str = "manual";
    // ---- 0.12.0: retro-labelling from ground truth -------------------------
    /// Discord said so (0.12.0). The segment carries a `single` verdict, the
    /// Discord user who owned it is linked to a voice, and the voicebank had
    /// declined to name the row at all.
    ///
    /// It is deliberately **not** `MATCH`: nothing was compared, so
    /// `match_score` stays NULL for the same reason [`PROXIMITY`]'s does, and a
    /// client that renders provenance must be able to say where the name came
    /// from. It is deliberately not `MANUAL` either — a person did not look at
    /// this row, and `MANUAL` is the one value the auto-linker is forbidden to
    /// overwrite (see `truth::link_batch`). Giving Discord its own value keeps
    /// that promise intact and keeps this pass reversible as a class.
    pub const TRUTH: &str = "truth";
    // ---- end 0.12.0 --------------------------------------------------------
    // ---- 0.12.1: per-user Discord audio ------------------------------------
    /// The turn arrived on ONE Discord user's own stream, so the speaker is a
    /// fact about the wire rather than a reading of the audio
    /// ([`super::KIND_DISCORD_USER`]).
    ///
    /// This is [`MIC`]'s claim made about somebody else, and it is exactly as
    /// strong: the headset is one person because one person wears it, and a
    /// per-user WebRTC stream is one person because Discord decoded it from one
    /// person's packets. `match_score` is NULL for the same reason it is on a
    /// mic turn — there was no comparison to score.
    ///
    /// It is deliberately not [`TRUTH`], which means something weaker and
    /// retroactive: "Discord's speaking ring says this mixed turn was probably
    /// them". Here Discord did not say whose voice it was, it *handed over the
    /// voice*.
    pub const DISCORD_STREAM: &str = "discord-stream";
    // ---- end 0.12.1 --------------------------------------------------------
}

/// `segments.lang_via` — how this row's *language* came to be what it is (v5).
pub mod lang_via {
    /// The ASR export only speaks one language, so the tag is a fact about the
    /// model rather than a guess about the audio.
    pub const MODEL: &str = "model";
    /// The text classifier (`crate::lang`) read the transcript.
    pub const CLASSIFIED: &str = "classified";
    /// The transcript was re-decoded under a hard language constraint because
    /// it disagreed with the speaker's declared language, and the new text won.
    pub const REDECODE: &str = "re-decode";
    /// The transcript disagrees with the speaker's declared language and no
    /// constrained decoder for that language exists, so the row is *marked* and
    /// its text left alone. `lang` stays NULL: the honest answer is that we do
    /// not know which of the two is wrong.
    pub const MISMATCH: &str = "mismatch";
    /// The conversational language prior (0.7.7): the classifier could not read
    /// a language out of these words, so the row took the one the rest of its
    /// **thread** was speaking. An inference, and marked as one — the text was
    /// not re-decoded and did not change.
    ///
    /// Never counted as evidence for another row's context
    /// (`Store::thread_language_stamps`): a context that fed on its own
    /// inferences would confirm itself.
    pub const CONTEXT: &str = "context";
    /// The third-language guesser read a script or a stopword majority the
    /// two-language classifier has no vocabulary for (0.10.2,
    /// `crate::lang::guess_other`). Written only on a *confident* guess — a
    /// non-Latin script, or four stopwords — because unlike the two-way
    /// classifier this one is choosing between nineteen answers.
    ///
    /// Like [`CONTEXT`] it is an inference and the words were not re-decoded.
    pub const GUESSED: &str = "guessed";
    /// The archive sweep (0.12.0, `crate::sweep`) heard `de` or `en` on a row
    /// whose words nobody could read — the two languages the routes
    /// deliberately leave alone, so the honest record is the reading itself and
    /// no new words.
    ///
    /// An eighth value rather than a seventh use of [`crate::asr_cjk::
    /// LANG_VIA_LID`], and the difference is what was *done*: `lid` means a
    /// decoder re-read the turn and its words are on the row, and this means
    /// nothing was re-decoded at all. A consumer that treats `lid` as "these
    /// words came out of a language-specific decoder" would be wrong about
    /// every one of these rows.
    ///
    /// **Always with `lang` set.** 0.11.9 also wrote it bare, with `lang` still
    /// NULL, as an "asked, nothing to say" mark — and that second shape is why
    /// the sweep could not finish: a `lang_via` cannot say both "this is how
    /// the language got here" and "this pass has been here", and the rows that
    /// needed only the second answer got neither. Schema v16 moved the
    /// bookkeeping to `segments.sweep_at_ns` and renamed the existing bare
    /// marks into it (`Store::apply_v16`), leaving this value meaning one
    /// thing.
    ///
    /// Deliberately **not** counted as evidence by
    /// [`super::Store::thread_language_stamps`]: one second of audio nobody
    /// could read is not a fact about the conversation, and letting it vote
    /// would turn the sweep's own uncertainty into the prior that decides other
    /// rows.
    pub const SWEEP: &str = "sweep";
}

/// Which pass produced a row's words (v10, on the wire as `text_via`).
///
/// A separate axis from [`lang_via`], which says how the row's *language* got
/// there. The two moved together until 0.8.0 because the only thing that ever
/// rewrote a transcript was the language arbiter; the context re-decode rewrites
/// words without touching the language at all, which is exactly why it needed
/// its own column rather than a sixth value in that one.
pub mod text_via {
    /// The first pass, on the way in.
    pub const LIVE: &str = "live";
    /// Re-decoded with the session's surrounding audio (`crate::quality`).
    pub const CONTEXT: &str = "context";
    /// Re-decoded by a language arbiter (`crate::arbiter`).
    pub const ARBITER: &str = "arbiter";
    /// Re-read overnight by the night shift's third decoder (`crate::night`,
    /// 0.9.0), and only ever where two of the three readings agreed.
    pub const NIGHT: &str = "night";
    /// Re-decoded by the Japanese decoder after the spoken-language identifier
    /// heard Japanese (`crate::asr_cjk`, 0.11.0). Its own value since 0.11.4: a
    /// row that says `arbiter` was a German/English flip; this is a different
    /// claim about the audio.
    pub const LID: &str = "lid";
}

/// What a second decoder made of a transcript (v10, on the wire as
/// `asr_confidence`). Flag only: the cross-check never replaces a word.
pub mod asr_confidence {
    pub const SOLID: &str = "solid";
    pub const SHAKY: &str = "shaky";
}

// ---- 0.9.0: ground truth from Discord -------------------------------------

/// What Discord's own speaking rings said about one segment (v11, on the wire
/// as `truth_verdict`).
///
/// This is **not** a label and never becomes one: the daemon's speaker for a
/// row is still whatever the voicebank decided. The verdict is the yardstick
/// that decision gets measured against.
pub mod truth_verdict {
    /// One Discord user covered at least [`SINGLE_MIN`] of the segment and
    /// nobody else reached [`PRESENT_MIN`]. The only verdict identity is
    /// scored on: it is the only one where "the right answer" is a single
    /// name.
    pub const SINGLE: &str = "single";
    /// Two or more users each reached [`PRESENT_MIN`]. What the overlap gate
    /// exists to catch, and therefore what it is scored against.
    pub const OVERLAP: &str = "overlap";
    /// Exactly one user was heard, but they covered less than [`SINGLE_MIN`]
    /// of the span.
    ///
    /// **The case the four-way verdict list did not name**, and it is common:
    /// a VAD span whose edges run past the words. Folding it into `single`
    /// would lower a bar that was set at 0.8 deliberately, and folding it into
    /// `overlap` would claim a second voice that is not there — so it gets its
    /// own name and is excluded from both scores.
    pub const PARTIAL: &str = "partial";
    /// Truth data covers this moment and says nobody in it was talking. The
    /// honest reading is usually "the local user, on a mic Discord is not
    /// carrying" — but the plugin reports the local user too, so a `nobody`
    /// that is not explained by a muted mic is a real disagreement worth
    /// looking at.
    pub const NOBODY: &str = "nobody";
    /// No truth data anywhere near this segment — the plugin was not running.
    /// Not a measurement, and never counted as one.
    pub const UNKNOWN: &str = "unknown";

    pub const ALL: [&str; 5] = [SINGLE, OVERLAP, PARTIAL, NOBODY, UNKNOWN];

    /// A user must cover this much of a segment to own it outright.
    pub const SINGLE_MIN: f64 = 0.8;
    /// A user counts as present in a segment at all from here up.
    pub const PRESENT_MIN: f64 = 0.2;

    pub fn parse(s: &str) -> Option<&'static str> {
        ALL.into_iter().find(|v| *v == s)
    }
}

/// How a Discord user came to be linked to a speaker (v11,
/// `discord_users.via`), and how a prototype came to be enrolled
/// (`speaker_prototypes.via`).
pub mod truth_via {
    /// The auto-linker: this user's clean turns were labelled as one voice
    /// often enough, for long enough, that the two are the same person.
    pub const TRUTH: &str = "truth";
    /// A person said so.
    pub const MANUAL: &str = "manual";
    // ---- 0.11.0: learned identity -----------------------------------------
    /// The calibration pass fitted this from the install's own ground truth
    /// and it cleared the held-out gate (`speakers.threshold_via`).
    pub const LEARNED: &str = "learned";
    // ---- end 0.11.0 -------------------------------------------------------
    // ---- 0.12.1: per-user Discord audio ------------------------------------
    /// A voice minted the first time this account's **own audio stream**
    /// arrived, and linked to it on the spot
    /// ([`super::KIND_DISCORD_USER`]).
    ///
    /// Nothing was decided, which is why it is not [`TRUTH`]: the auto-linker
    /// weighs 20 labelled turns at 90% agreement before it dares connect an
    /// account to a voice it did not create. This link has no agreement to
    /// measure, because the voice exists *for* this account and holds nothing
    /// else. A person may still re-point it, and a [`MANUAL`] link is never
    /// overwritten by this any more than by the auto-linker.
    pub const DISCORD_STREAM: &str = "discord-stream";
    // ---- end 0.12.1 --------------------------------------------------------
}

// ---- 0.12.0: retro-labelling from ground truth -----------------------------

/// A turn Discord can name that the voicebank left blank.
#[derive(Debug, Clone, PartialEq)]
pub struct TruthLabelCandidate {
    pub id: i64,
    pub user_id: String,
    pub user_name: String,
    pub speaker_id: i64,
    pub t_start_ns: i64,
    pub t_end_ns: i64,
    pub coverage: Option<f64>,
}

// ---- end 0.12.0 -----------------------------------------------------------

/// `settings` key holding the id of the pinned "You" speaker.
pub const YOU_SPEAKER_KEY: &str = "you_speaker_id";

/// `settings` key for the learned prototype aggregate (0.12.0). Absent means
/// [`crate::calib::Aggregate::Max`], which is what every earlier version did.
pub const AGGREGATE_KEY: &str = "identity_aggregate";
/// The generated label the pinned speaker is minted with. It survives a rename
/// (the user may call themselves anything), so it is also how a database that
/// somehow lost its settings row re-adopts the existing voice instead of
/// minting a second one.
pub const YOU_AUTO_LABEL: &str = "You";

#[derive(Debug, Clone)]
pub struct SourceRow {
    pub id: i64,
    pub match_key: String,
    pub display_name: String,
    /// `"app"` or `"mic"` (v4). The microphone is a source like any other in
    /// the schema and nothing like one in the consent model, so clients need to
    /// be able to tell them apart without string-matching the key.
    pub kind: String,
    pub allowed: bool,
    pub first_seen: i64,
    /// Last time the source was seen on the graph. Equal to `first_seen` for a
    /// source that has only ever been seen once.
    pub last_seen: i64,
    /// Capture sessions currently open for it — the "capturing now" light.
    pub streams: i64,
}

/// What the analysis leg learned about one turn.
#[derive(Debug, Clone, Default)]
pub struct SegmentAnalysis {
    pub text: Option<String>,
    pub lang: Option<String>,
    /// Where `lang` came from (`store::lang_via`). NULL when there is no
    /// language: a transcript nobody could classify says so by leaving both
    /// columns empty rather than by claiming a language it did not read.
    pub lang_via: Option<String>,
    pub asr_model_id: Option<String>,
    pub overlap_frac: Option<f32>,
}

/// Every highlighted voice's `(colour, icon)`, by speaker id (v15).
///
/// Named rather than spelled out at each use: the payload builders in
/// `service.rs`, `brief.rs`, `digest.rs`, `replay.rs` and `truth.rs` all hold
/// one of these and pass it to `service::style_of`, and five copies of the same
/// nested `Option` pair is the shape clippy calls a complex type — rightly, in
/// the sense that "a map of ids to highlights" is what it means and the tuple
/// is only how it is stored.
pub type SpeakerStyles = std::collections::HashMap<i64, (Option<String>, Option<String>)>;

#[derive(Debug, Clone)]
pub struct SpeakerSummary {
    pub id: i64,
    pub display_name: String,
    /// The generated `Speaker_07` label, kept even after a rename so a client
    /// can tell an unnamed voice from a named one — which is the whole
    /// onboarding question (DESIGN §5).
    pub auto_label: String,
    /// When the user named this voice. `None` means nobody has.
    pub named_at: Option<i64>,
    pub created_at: i64,
    pub segments: i64,
    pub speech_ns: i64,
    /// Which languages this voice actually speaks (v5). `None` is *any*, the
    /// default: nothing is corrected until somebody says what to expect.
    pub languages: Option<Vec<String>>,
    /// The palette token this voice is highlighted with (v15), or `None` for
    /// the overwhelming majority of voices, which nobody has picked out.
    pub colour: Option<String>,
    /// The emoji that goes before this voice's name (v15), or `None`.
    pub icon: Option<String>,
}

impl SpeakerSummary {
    /// The user's name for this voice, or `None` while it is still anonymous.
    pub fn name(&self) -> Option<&str> {
        self.named_at.map(|_| self.display_name.as_str())
    }
}

/// Everything a client needs about one segment, in one row.
#[derive(Debug, Clone)]
pub struct SegmentRow {
    pub id: i64,
    pub session_id: i64,
    /// The source's match key (`VRChat.exe`).
    pub source: String,
    pub t_start_ns: i64,
    pub t_end_ns: i64,
    pub speaker_id: Option<i64>,
    pub speaker_name: Option<String>,
    /// The palette token this voice is highlighted with, or `None` (v15). On
    /// the row rather than looked up per render because a caption surface
    /// resolves a name once and must not need a second query to know what
    /// colour to draw it in.
    pub speaker_colour: Option<String>,
    /// The emoji that goes before this voice's name, or `None` (v15).
    pub speaker_icon: Option<String>,
    pub text: Option<String>,
    pub overlap_frac: Option<f32>,
    pub match_score: Option<f32>,
    pub audio_path: String,
    /// The transcript's language, when one is known (v5).
    pub lang: Option<String>,
    /// How the *language* got here (`store::lang_via`). On the wire since
    /// 0.7.7: `"re-decode"` means these words came from an arbiter re-reading
    /// the audio rather than from the primary ASR, and a client that shows
    /// provenance for the speaker should be able to show it for the words too.
    pub lang_via: Option<String>,
    /// How the speaker got here (`store::label_via`), so a client can distrust
    /// an inherited label without distrusting a matched one.
    pub label_via: Option<String>,
    /// Which conversation this turn belongs to (v6). `None` on a row written
    /// before threading existed and never backfilled, which a client renders
    /// exactly as it always did.
    pub thread_id: Option<i64>,
    /// Which pass produced these words (v10, `store::text_via`): `"live"` on
    /// the first pass, `"context"` after the re-decode worker read the turn
    /// with the audio around it, `"arbiter"` after a constrained re-decode.
    /// `None` on a row written before the column existed.
    pub text_via: Option<String>,
    /// What a second decoder made of them (v10): `"solid"`, `"shaky"`, or
    /// `None` when no cross-check has run — which is not the same as "checked
    /// and fine", and is why the null is on the wire.
    pub asr_confidence: Option<String>,
    /// What the night shift read on its own pass (v11, 0.9.0), whether or not
    /// it was allowed to replace the words. `None` on a row no night has
    /// reached — and on every row until `[night].enabled` is turned on.
    pub night_text: Option<String>,
    // ---- 0.9.0, the assistant (schema v11) --------------------------------
    /// This turn, in the language the person reading is expected to have
    /// (`[assist] translate_to`). `None` on every row until the idle pass has
    /// looked, and on every row it decided not to translate — a turn already
    /// in that language, a turn under three words, or one whose translation
    /// failed a guard.
    pub translation: Option<String>,
    /// Which model wrote it, so a translation from one model is never mistaken
    /// for a translation from another. Always set when `translation` is.
    pub translation_via: Option<String>,
    // ---- end 0.9.0 --------------------------------------------------------
    // ---- 0.12.4 (schema v18): how it sounded ------------------------------
    /// One of [`crate::mood::Mood`]'s four words, or `None`.
    ///
    /// `None` means one of two things and the row cannot tell them apart:
    /// nothing has listened yet, or the model listened and abstained — which it
    /// does on about three quarters of real turns (FINDINGS §42). Both are
    /// correctly rendered the same way, which is *not at all*, so the
    /// distinction stays in `mood_at_ns` and off the wire.
    pub mood: Option<String>,
    /// The audio events on this turn, as the sorted comma-joined closed set
    /// [`crate::mood::Event`] defines, or `None` for a turn that carried none.
    pub events: Option<String>,
    // ---- end 0.12.4 -------------------------------------------------------
}

/// One row the mood pass may listen to (0.12.4, `crate::mood`).
#[derive(Debug, Clone)]
pub struct MoodCandidate {
    pub id: i64,
    pub duration_s: f32,
    /// Relative to the data dir, and never empty: the query filters those out.
    pub audio_path: String,
    /// What the row says. The pass does not read it — it is carried for the
    /// `spike/mood_score.py` scored the tag against, and what a `sqlite3`
    /// query wants beside a tag it is trying to make sense of (FINDINGS §42.8).
    pub text: Option<String>,
}

/// How a person, or a conversation, has sounded (0.12.4).
///
/// Counts and a denominator, never a verdict. Turning these into a sentence is
/// [`crate::mood::summary`]'s job, and it refuses to write one from too few
/// rows — see [`crate::mood::MIN_ROWS_FOR_A_SUMMARY`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MoodTotals {
    /// Rows of theirs the pass has actually listened to. The denominator, and
    /// the reason none of the numbers below is ever published alone.
    pub read: i64,
    pub happy: i64,
    pub sad: i64,
    pub angry: i64,
    pub neutral: i64,
    pub laughter: i64,
    pub music: i64,
    /// The newest row the pass has read of theirs, so a client can say how
    /// current the picture is. `None` when `read` is zero.
    pub last_ns: Option<i64>,
}

/// One hand-made correction, as `text_truth` keeps it (v19, 0.12.4).
///
/// `truth_text` is what a person typed and is therefore the reference; every
/// other text field is what some decoder read of the same audio. See
/// `crate::text_truth` for how the three readings are attributed, and
/// `apply_v19` for the table.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextTruth {
    pub segment_id: i64,
    pub truth_text: String,
    pub live_text: Option<String>,
    pub context_text: Option<String>,
    pub night_text: Option<String>,
    /// Always `None` today — the cross-check stores a verdict, not words.
    pub canary_text: Option<String>,
    pub asr_confidence: Option<String>,
    pub speaker_id: Option<i64>,
    pub source_kind: String,
    pub duration_ns: i64,
    pub created_ns: i64,
}

/// One `segments.redecode` operation, read back: when a machine rewrote this
/// row, and the words and route it wrote over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedecodePrior {
    pub at_utc_ns: i64,
    pub text: Option<String>,
    pub text_via: Option<String>,
}

/// What a segment is, for the purpose of bucketing a measurement about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentFacets {
    pub speaker_id: Option<i64>,
    pub source_kind: String,
    pub duration_ns: i64,
    pub asr_confidence: Option<String>,
    pub night_text: Option<String>,
    /// The words on the row right now, and which pass wrote them.
    pub text: Option<String>,
    pub text_via: Option<String>,
}

/// A turn the idle quality worker may act on: enough to find its audio, place
/// it in its session, and compare what comes back with what is there now.
#[derive(Debug, Clone)]
pub struct RedecodeCandidate {
    pub id: i64,
    pub session_id: i64,
    pub t_start_ns: i64,
    pub t_end_ns: i64,
    /// Relative to the data dir, and never empty: the query filters those out.
    pub audio_path: String,
    pub text: Option<String>,
}

/// A row the archive language sweep may look at (0.12.0, `crate::sweep`).
///
/// Everything [`crate::asr_cjk::pre_route`] needs and nothing else, so the
/// cheap half of the decision — is this transcript already readable, is this
/// voice pinned to one language — can be made without touching the disk.
#[derive(Debug, Clone)]
pub struct SweepCandidate {
    pub id: i64,
    pub duration_s: f32,
    /// Relative to the data dir, and never empty: the query filters those out.
    pub audio_path: String,
    pub text: Option<String>,
    /// The speaker's declared languages, resolved through merges exactly as
    /// [`Store::speaker_languages`] resolves them.
    pub declared: Option<Vec<String>>,
}

/// A row the audio-language route already rewrote, and what it wrote over
/// (0.12.0, `crate::unroute`).
///
/// The prior three fields come out of the row's own `segments.redecode`
/// operation rather than out of a second column, because that operation *is*
/// the record — `Store::set_segment_text_via` has written it since 0.11.0
/// exactly so that a machine's edit of a transcript can be compared and undone.
/// `None` for all three means the row was routed by a build that predates the
/// operation, or that a person has since deleted it; either way there is
/// nothing to restore and the repair leaves the row alone.
#[derive(Debug, Clone)]
pub struct RoutedRow {
    pub id: i64,
    pub duration_s: f32,
    /// Relative to the data dir. May be empty — retention takes clips and
    /// leaves rows.
    pub audio_path: String,
    /// What the row says now: the decoder's words.
    pub text: Option<String>,
    pub lang: Option<String>,
    /// The speaker's declared languages, resolved through merges.
    pub declared: Option<Vec<String>>,
    /// The words the route replaced, and the model and route that had produced
    /// them.
    pub prior_text: Option<String>,
    pub prior_model: Option<String>,
    pub prior_via: Option<String>,
    /// Whether the row has a `segments.redecode` operation at all.
    pub has_prior: bool,
}

/// One stored turn's audio, as the window builder sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clip {
    pub id: i64,
    pub t_start_ns: i64,
    pub t_end_ns: i64,
    pub audio_path: String,
}

/// One candidate clip for naming a voice: enough to rank it, label it in a
/// list, and fetch its audio. Deliberately not a `SegmentRow` — the point of
/// `speakers.sample` is to be cheap enough to call from a naming prompt.
#[derive(Debug, Clone)]
pub struct SampleRow {
    pub segment_id: i64,
    pub t_start_ns: i64,
    pub t_end_ns: i64,
    pub text: Option<String>,
    pub match_score: Option<f32>,
    /// Relative to the data dir, and never empty: the query filters those out.
    pub audio_path: String,
}

/// One segment as proximity inheritance sees it: when it happened, who it was
/// labelled as, and how confidently. Deliberately not a `SegmentRow` — this is
/// asked once per stored turn and must not cost three joins.
#[derive(Debug, Clone, PartialEq)]
pub struct NeighbourSegment {
    pub id: i64,
    pub t_start_ns: i64,
    pub t_end_ns: i64,
    pub speaker_id: Option<i64>,
    pub match_score: Option<f32>,
    pub label_via: Option<String>,
}

impl NeighbourSegment {
    pub fn duration_s(&self) -> f32 {
        (self.t_end_ns - self.t_start_ns).max(0) as f32 / 1e9
    }
}

/// What deleting one voice removed — the sweep (`speakers.prune`) and the
/// deliberate delete (`speakers.delete`) both report through this, because they
/// are the same cascade with one switch. `goldens` are paths the caller unlinks
/// — the rows are already gone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpeakerDeleteReport {
    pub speaker_id: i64,
    /// Every segment that pointed at the voice, live or already soft-deleted.
    pub segments: Vec<i64>,
    /// The ones this call was the one to soft-delete — exactly the ids the
    /// `purge` event names, and never the rows a previous delete already took.
    pub soft_deleted: Vec<i64>,
    pub prototypes: usize,
    /// Rows out of `embeddings` for this voice's segments. Only the nuke path
    /// takes these: they are what a re-cluster and a re-enrolment rest on.
    pub embeddings: usize,
    pub goldens: Vec<String>,
    /// Conversations left with nothing live in them, cleaned as the purge path
    /// cleans them (DESIGN §0's deletion rule).
    pub threads: usize,
    /// Derived rows that went with the voice (schema v7): commitments it made
    /// or was owed, and the time references on its turns. Reported because a
    /// cascade nobody can see is a cascade nobody can check.
    pub commitments: usize,
    pub time_refs: usize,
    /// False when the voiceprint was kept: the identity is still in the bank
    /// and still matches future audio.
    pub removed_speaker: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeReport {
    pub from: i64,
    pub into: i64,
    pub segments: usize,
    pub prototypes: usize,
    pub golden_samples: usize,
    /// Speakers already tombstoned onto `from`, re-pointed at `into`. Merge
    /// chains are never created, so a lookup is always one hop.
    pub tombstones_repointed: usize,
}

/// The rows one accepted split hands to the new speaker, plus the ones that
/// stay behind with a reduced score. Produced by `split::plan`, applied by
/// `Store::split_speaker`, so the decision and the write stay separable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SplitWrite {
    /// Prototype ids that move to the minted speaker.
    pub prototypes: Vec<i64>,
    /// Segments that move, with the score to record for them.
    pub segments: Vec<(i64, f32)>,
    /// Segments that keep the existing speaker but are no longer trusted:
    /// only their `match_score` changes.
    pub ambiguous: Vec<(i64, f32)>,
}

/// What one segment was labelled with before an operation touched it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmentLabel {
    pub segment_id: i64,
    pub speaker_id: Option<i64>,
    pub match_score: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SplitReport {
    /// The speaker that kept its id, and therefore its name and its history.
    pub kept: i64,
    pub minted: i64,
    /// The generated label of the minted voice — the onboarding flow's
    /// "who is this?" handle (DESIGN §5).
    pub auto_label: String,
    pub prototypes: usize,
    pub segments: usize,
    pub ambiguous: usize,
}

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub row: SegmentRow,
    /// The matched words with their context, marked up by FTS5.
    pub snippet: String,
}

impl SearchHit {
    pub fn segment_id(&self) -> i64 {
        self.row.id
    }
    pub fn speaker(&self) -> Option<&str> {
        self.row.speaker_name.as_deref()
    }
}

/// What [`Store::roster_reconcile`] had to change to make the table agree with
/// the log: rows closed for people who are no longer here, rows opened for
/// people who are.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RosterReconcile {
    pub closed: usize,
    pub opened: usize,
}

impl RosterReconcile {
    pub fn is_empty(self) -> bool {
        self.closed == 0 && self.opened == 0
    }
}

/// One person's presence in a VRChat instance, as read from the output log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterRow {
    pub id: i64,
    pub world_id: Option<String>,
    pub instance: Option<String>,
    pub display_name: String,
    pub joined_at_utc_ns: i64,
    pub left_at_utc_ns: Option<i64>,
}

/// One kept clip of a voice, exempt from the audio retention window: a golden
/// is what a future embedding model gets re-enrolled from (DESIGN §5/§6), so
/// forgetting it would cost the identity, not just the recording.
#[derive(Debug, Clone, PartialEq)]
pub struct GoldenRow {
    pub id: i64,
    /// Relative to the data dir, under `goldens/`, which the retention sweeper
    /// never walks.
    pub audio_path: String,
    pub duration_s: f32,
}

/// One entry in the audit trail. `prior_state` is JSON holding enough to undo
/// the operation; writing the undo *method* is a later step, keeping the record
/// is this one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRow {
    pub id: i64,
    pub op: String,
    pub target_ids: String,
    pub prior_state: String,
    pub at_utc_ns: i64,
}

/// Which segments an operation is about. Every field is a narrowing `AND`; all
/// of them `None` means "every live segment", which is why the delete path
/// insists on a preview first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SegmentFilter {
    pub speaker: Option<i64>,
    pub session: Option<i64>,
    /// A source's match key (`VRChat.exe`), not its row id.
    pub source: Option<String>,
    pub from: Option<i64>,
    pub to: Option<i64>,
    /// 0.10.0 — the world facet, already resolved to the ids it selects
    /// (`crate::worlds::resolve_facet`). `None` is "anywhere"; an EMPTY vector
    /// is impossible by construction, because a facet that matches no world
    /// resolves to one id that cannot exist rather than to nothing.
    pub worlds: Option<Vec<String>>,
}

impl SegmentFilter {
    pub fn is_everything(&self) -> bool {
        *self == Self::default()
    }

    /// The world facet as a JSON array, which is how it reaches SQL: a
    /// `json_each` subquery is the one way to bind a *list* to a prepared
    /// statement without building the SQL out of the values.
    pub fn worlds_json(&self) -> Option<String> {
        self.worlds
            .as_ref()
            .map(|ids| serde_json::Value::from(ids.clone()).to_string())
    }
}

/// The world predicate, spelled once so no read path can spell it differently.
/// `{n}` is the parameter index carrying [`SegmentFilter::worlds_json`].
pub(crate) fn world_clause(n: usize) -> String {
    format!(
        "AND (?{n} IS NULL OR g.thread_id IN (
              SELECT t.id FROM threads t
               WHERE t.world_id IN (SELECT value FROM json_each(?{n}))))"
    )
}

/// What one person's page adds up to. Every number here is a `COUNT` or a
/// `SUM` over live segments — nothing is stored, nothing is cached, and a
/// deleted segment stops counting the moment it is deleted (DESIGN §0).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PersonTotals {
    pub segments: i64,
    pub speech_ns: i64,
    /// Distinct capture sessions this voice was heard in.
    pub sessions: i64,
    /// Distinct conversation threads it took part in.
    pub threads: i64,
    /// First and last time it was heard at all. `None` on a voice whose every
    /// segment has been deleted, which is a real state and not an error.
    pub first_ns: Option<i64>,
    pub last_ns: Option<i64>,
}

/// One co-presence edge: somebody this voice actually talks *with*.
///
/// Not a stored table (GRAPH.md sketched `person_edges`; the query turned out
/// to be cheap enough that a cache would only be a way to be wrong). It is
/// computed from threads, so it means "we were in the same conversation",
/// which is a far stronger statement than "we were in the same instance".
#[derive(Debug, Clone, PartialEq)]
pub struct PersonEdge {
    pub speaker_id: i64,
    pub display_name: String,
    pub auto_label: String,
    pub named_at: Option<i64>,
    /// Conversations the two shared.
    pub threads: i64,
    /// How much the *other* person spoke in those conversations. Their speech,
    /// not the overlap of two speech timelines: people take turns, so an
    /// intersection would be near zero and would say nothing.
    pub speech_ns: i64,
    /// The last time they were in a conversation together.
    pub last_ns: i64,
    /// Seconds the two were in the same VRChat instance, from the roster —
    /// `None` when either voice has no name that matches a roster entry, which
    /// is the common case and is reported rather than guessed at.
    pub roster_ns: Option<i64>,
    /// The highlight this voice wears (v15), so the "people they talk with"
    /// list picks the same person out that the transcript does.
    pub colour: Option<String>,
    pub icon: Option<String>,
}

/// One conversation, summarised for a list.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadSummary {
    pub id: i64,
    pub session_id: i64,
    pub started_ns: i64,
    pub ended_ns: i64,
    pub segments: i64,
    /// Canonical speaker ids, most talkative first.
    pub participants: Vec<i64>,
    /// The first thing anybody said in it, as the list's one line of content.
    pub preview: Option<String>,
}

// ---- the memory graph, Tiers 2 and 3 (schema v7, GRAPH.md) ----------------

/// `commitments.state`. A row only ever leaves `CANDIDATE` because a person
/// clicked: nothing in this program acts on a guess, and nothing nags.
pub mod commitment_state {
    pub const CANDIDATE: &str = "candidate";
    pub const CONFIRMED: &str = "confirmed";
    pub const DONE: &str = "done";
    pub const DISMISSED: &str = "dismissed";
    pub const ALL: [&str; 4] = [CANDIDATE, CONFIRMED, DONE, DISMISSED];

    pub fn parse(s: &str) -> Option<&'static str> {
        ALL.into_iter().find(|k| *k == s)
    }
}

/// `commitments.source` — which tier claimed this, and therefore how much a
/// surface is allowed to imply. They are rendered differently because they are
/// not the same claim.
pub mod commitment_source {
    /// A modal-pattern match. Cheap, wrong sometimes, always a guess.
    pub const RULES: &str = "rules";
    /// The tiny local model, under a verdict-first grammar (GRAPH.md Tier 3).
    pub const LLM: &str = "llm";
}

/// One resolved time reference, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeRefRow {
    pub id: i64,
    pub segment_id: i64,
    pub raw: String,
    pub resolved_utc_ns: i64,
    pub kind: String,
}

/// A commitment as it goes to a client: the row, plus the names and the
/// transcript line it points at, so the list needs no second query per row.
#[derive(Debug, Clone)]
pub struct CommitmentRow {
    pub id: i64,
    pub segment_id: i64,
    pub thread_id: Option<i64>,
    pub who_speaker_id: Option<i64>,
    pub who_name: Option<String>,
    pub who_auto: Option<String>,
    pub to_speaker_id: Option<i64>,
    pub to_name: Option<String>,
    pub to_auto: Option<String>,
    /// The highlight (v15) on each side of the promise.
    pub who_colour: Option<String>,
    pub who_icon: Option<String>,
    pub to_colour: Option<String>,
    pub to_icon: Option<String>,
    pub what: String,
    pub due_utc_ns: Option<i64>,
    pub due_raw: Option<String>,
    pub due_kind: Option<String>,
    pub state: String,
    pub source: String,
    pub model_id: Option<String>,
    pub confidence: Option<f64>,
    pub created_at: i64,
    pub updated_at: i64,
    /// The segment's own start, so a row can land in the transcript.
    pub t_start_ns: i64,
    /// What was actually said. A commitment is a claim *about* a line, and the
    /// line has to be readable next to it or there is no way to disagree.
    pub said: Option<String>,
}

/// What a client needs to write a commitment down. `due` is whichever time
/// reference the extractor settled on, kept in both forms.
#[derive(Debug, Clone)]
pub struct NewCommitment {
    pub segment_id: i64,
    pub thread_id: Option<i64>,
    pub who_speaker_id: Option<i64>,
    pub to_speaker_id: Option<i64>,
    pub what: String,
    pub due_utc_ns: Option<i64>,
    pub due_raw: Option<String>,
    pub due_kind: Option<String>,
    pub source: &'static str,
    pub model_id: Option<String>,
    pub confidence: f64,
}

/// What `upsert_commitment` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitmentWrite {
    /// There was nothing on this segment; a candidate was filed.
    Inserted(i64),
    /// A row was already there and this one carries more authority, so it was
    /// upgraded in place — same id, same state, better provenance.
    Upgraded(i64),
    /// A row was already there and this one does not outrank it. Nothing moved.
    /// A rules pass must never overwrite what the model said, and neither pass
    /// may ever undo a person's click.
    Kept(i64),
}

impl CommitmentWrite {
    pub fn id(self) -> i64 {
        match self {
            CommitmentWrite::Inserted(id)
            | CommitmentWrite::Upgraded(id)
            | CommitmentWrite::Kept(id) => id,
        }
    }
}

/// One turn of a conversation, as a Tier 3 window needs it: the words, who said
/// them, and when. Not a `SegmentRow` — the model is shown a transcript, not a
/// database row, and keeping the shapes apart is what stops the prompt growing
/// fields nobody meant to send it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadLine {
    pub segment_id: i64,
    pub speaker_id: Option<i64>,
    pub t_start_ns: i64,
    pub text: String,
}

/// One topic label, with the conversations wearing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicRow {
    pub topic: String,
    pub threads: i64,
    pub segments: i64,
    pub last_ns: i64,
    /// Newest first, capped by the caller — a topic is a way into conversations,
    /// not a list of every one that ever mentioned it.
    pub thread_ids: Vec<i64>,
}

/// The one-glance state of the graph: what has been derived, and how much is
/// left to derive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GraphCounts {
    pub time_refs: i64,
    pub commitments: i64,
    pub open: i64,
    pub candidates: i64,
    pub confirmed: i64,
    pub done: i64,
    pub dismissed: i64,
    pub from_rules: i64,
    pub from_llm: i64,
    pub topics: i64,
    pub threads: i64,
    pub threads_enriched: i64,
    /// Unenriched conversations with at least one turn that has words.
    ///
    /// **Not the worker's queue** — see [`Self::threads_waiting`]. Kept
    /// because it is the honest answer to "how many are unread", and because
    /// splitting it is the whole point of the pair below.
    pub threads_pending: i64,
    /// Unenriched conversations the worker will actually take: `pending`,
    /// narrowed to those with at least `[graph].min_thread_segments` turns
    /// that have words. This is [`Store::unenriched_threads`]'s filter, term
    /// for term.
    pub threads_waiting: i64,
    /// The rest of `pending`: unread, and short enough that the worker will
    /// never read them. `waiting + too_short == pending`.
    ///
    /// These exist because 0.12.4 counted `pending` and called it "waiting",
    /// so a card could say *"919 waiting"* beside a chip that honestly said
    /// *"idle — nothing left to read"*. Both were true and together they were
    /// a bug report. A conversation of two turns is not a backlog; it is a
    /// conversation the model was never going to be shown.
    pub threads_too_short: i64,
}

#[derive(Debug, Clone)]
pub struct TranscriptRow {
    pub segment_id: i64,
    pub session_id: i64,
    pub t_start_ns: i64,
    pub t_end_ns: i64,
    pub speaker: Option<String>,
    pub overlap_frac: Option<f32>,
    pub text: Option<String>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) `<data_dir>/recall.db`.
    pub fn open(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating data dir {}", data_dir.display()))?;
        let db_path = data_dir.join("recall.db");
        let conn =
            Connection::open(&db_path).with_context(|| format!("opening {}", db_path.display()))?;
        let store = Self { conn };
        store.init()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let store = Self {
            conn: Connection::open_in_memory()?,
        };
        store.init()?;
        Ok(store)
    }

    /// The open connection, for the modules that own their own tables.
    ///
    /// `crate::semantic` is one: `segment_vectors` is queried by nothing else
    /// in the schema, and putting its dozen statements here would make this
    /// file the place every feature goes to grow.
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    fn init(&self) -> Result<()> {
        // WAL keeps the writer from blocking readers, so the future GUI can
        // browse while the daemon is still appending.
        let _: String = self
            .conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .context("enabling WAL")?;
        self.conn.execute_batch(
            "PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;

             CREATE TABLE IF NOT EXISTS schema_version (
                 version INTEGER NOT NULL
             );

             CREATE TABLE IF NOT EXISTS sources (
                 id            INTEGER PRIMARY KEY,
                 match_key     TEXT    NOT NULL UNIQUE,
                 display_name  TEXT    NOT NULL,
                 allowed       INTEGER NOT NULL DEFAULT 0,
                 first_seen    INTEGER NOT NULL
             );

             CREATE TABLE IF NOT EXISTS sessions (
                 id                INTEGER PRIMARY KEY,
                 source_id         INTEGER NOT NULL REFERENCES sources(id),
                 started_at_utc_ns INTEGER NOT NULL,
                 ended_at_utc_ns   INTEGER
             );

             CREATE TABLE IF NOT EXISTS segments (
                 id          INTEGER PRIMARY KEY,
                 session_id  INTEGER NOT NULL REFERENCES sessions(id),
                 t_start_ns  INTEGER NOT NULL,
                 t_end_ns    INTEGER NOT NULL,
                 audio_path  TEXT    NOT NULL,
                 created_at  INTEGER NOT NULL
             );

             CREATE INDEX IF NOT EXISTS idx_sessions_source ON sessions(source_id);
             CREATE INDEX IF NOT EXISTS idx_segments_session ON segments(session_id);
             CREATE INDEX IF NOT EXISTS idx_segments_start ON segments(t_start_ns);",
        )?;

        let current: Option<i64> = self
            .conn
            .query_row("SELECT version FROM schema_version LIMIT 1", [], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(v) = current
            && v > SCHEMA_VERSION
        {
            bail!(
                "database schema version {v} was written by a different recalld \
                 (this build speaks version {SCHEMA_VERSION}); refusing to touch it"
            );
        }

        let migrating = current == Some(1);
        self.apply_v2()?;
        if migrating {
            // Backfill the index for rows that predate it. New rows arrive
            // through the triggers.
            //
            // Before the later migrations and not after, which is load-bearing:
            // the v2 triggers turn every UPDATE of a segment into a delete and
            // an insert against this external-content index, and doing that to
            // an index that has never been populated corrupts it. v5's
            // provenance backfill and v6's threading backfill are both such
            // updates, so the index has to be real before they run.
            self.conn
                .execute(
                    "INSERT INTO segments_fts(segments_fts) VALUES('rebuild')",
                    [],
                )
                .context("rebuilding the transcript index")?;
        }
        self.apply_v3()?;
        self.apply_v4()?;
        self.apply_v5()?;
        self.apply_v6()?;
        self.apply_v7()?;
        // v9: semantic search. Unconditional and idempotent like every
        // migration above it, and deliberately dependent on none of them.
        crate::semantic::migrate_v9(&self.conn)?;
        self.apply_v10()?;
        // v10 (0.8.0), second half: notes to self. Standalone like v9 — one
        // table that references `segments` and nothing else, and no backfill.
        self.apply_v10_notes()?;
        // v11 (0.9.0): ground truth from Discord. Standalone for the same
        // reason — two new tables, five new columns, nothing rewritten.
        // v11 (0.9.0): the night shift's two columns. Standalone and additive.
        self.apply_v11()?;
        self.apply_v11_night()?;
        // ---- 0.9.0, the assistant (schema v11) ----------------------------
        // Standalone like v9 and v10's second half: additive columns and one
        // table, reading nothing another migration writes.
        self.apply_v11_assist()?;
        // ---- end 0.9.0 ----------------------------------------------------

        // ---- 0.10.0 (schema v12): worlds ----------------------------------
        // Standalone like v9: one table (`visits`) and one column
        // (`threads.world_id`), reading nothing another migration writes.
        // Additive, idempotent, and with no backfill against `segments` — see
        // `crate::worlds::migrate_v12` for why, and for the one backfill that
        // IS honest (the VRChat logs still on disk).
        crate::worlds::migrate_v12(&self.conn)?;
        // ---- end 0.10.0 ---------------------------------------------------

        // ---- 0.11.0: source-aware identity --------------------------------
        // One index and nothing else. No column, no table, no backfill — the
        // source history is derived from `segments` and `sessions` on demand,
        // so there is no shape change and no version to bump.
        self.apply_source_prior_index()?;
        // ---- end 0.11.0 ---------------------------------------------------

        // ---- 0.11.0: learned identity -------------------------------------
        // Five nullable columns on `speakers` and one single-row table. All
        // NULL/absent means "use the globals", which is exactly what 0.10.2
        // did, so there is no backfill and nothing to undo.
        self.apply_learned_identity()?;
        // ---- end 0.11.0 ---------------------------------------------------

        // ---- 0.11.6 (schema v13): the simultaneous fraction ----------------
        // One nullable column on `segments`, and a backfill that only writes
        // where the speaking spans it is computed from are still on disk. See
        // `crate::truth::migrate_v13`.
        crate::truth::migrate_v13(&self.conn)?;
        // ---- 0.11.6: a digest names people --------------------------------
        // Three nullable columns on `digests`, no backfill and no version
        // bump: a row written before this change has none of them, and
        // `digest::digest_json` renders those the legacy way at read time.
        self.apply_digest_names()?;
        // ---- end 0.11.6 ---------------------------------------------------

        // ---- 0.12.0 (schema v14): which instance a session was -------------
        // One nullable column on `sessions`, no backfill: for every session
        // that already exists the answer is genuinely unknown, and NULL is the
        // only honest way to say so. See `apply_session_instance`.
        self.apply_session_instance()?;

        // ---- 0.12.0 (schema v15): highlighted people ----------------------
        // Two nullable columns on `speakers`. Additive, idempotent, no
        // backfill: NULL is exactly "this voice is not highlighted", which is
        // true of every voice that predates the feature.
        //
        // After v14 and not before: both are additive and neither reads what
        // the other writes, but the order the calls are written in is the
        // order the chain is documented in, and a reader tracing v13 → v14 →
        // v15 should find them in that order.
        self.apply_v15()?;

        // ---- 0.12.1 (schema v16): when the language sweep was last here ----
        // One nullable column on `segments`, and a backfill that is a rename
        // rather than a computation. See `apply_v16`.
        self.apply_v16()?;
        // ---- end 0.12.0 ---------------------------------------------------

        // ---- 0.12.3 (schema v17): whose bridge a speaking span came from ---
        // Two nullable columns on `truth_speaking`, no backfill. See the
        // banner at the top of this file and `crate::bridge::Scope`.
        self.apply_v17()?;
        // ---- end 0.12.3 ---------------------------------------------------

        // ---- 0.12.4 (schema v18): how a turn sounded ----------------------
        // Three nullable columns on `segments`, no backfill. See the banner at
        // the top of this file and `crate::mood`.
        self.apply_v18()?;
        // ---- 0.12.4 (schema v19): word-level ground truth ------------------
        // One table and one backfill, both in `apply_v19`. The backfill reads
        // the `segments.correct` and `segments.redecode` operations that are
        // already on disk and writes nothing back to them, so it is safe to
        // run on every open: the table's UNIQUE key makes a second pass a
        // no-op.
        self.apply_v19()?;
        // ---- end 0.12.4 ---------------------------------------------------

        match current {
            None => {
                self.conn.execute(
                    "INSERT INTO schema_version (version) VALUES (?1)",
                    params![SCHEMA_VERSION],
                )?;
            }
            Some(v) if v < SCHEMA_VERSION => {
                self.conn.execute(
                    "UPDATE schema_version SET version = ?1",
                    params![SCHEMA_VERSION],
                )?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Everything schema v2 adds, written so it is a no-op on a v2 database.
    fn apply_v2(&self) -> Result<()> {
        // Tables before columns: `segments.speaker_id` carries a foreign key
        // into `speakers`, which must therefore already exist.
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS speakers (
                 id           INTEGER PRIMARY KEY,
                 display_name TEXT    NOT NULL,
                 created_at   INTEGER NOT NULL,
                 -- Tombstone: this speaker was merged into another. Never a
                 -- chain; `merge` re-points existing tombstones instead.
                 merged_into  INTEGER REFERENCES speakers(id)
             );

             CREATE TABLE IF NOT EXISTS speaker_prototypes (
                 id                INTEGER PRIMARY KEY,
                 speaker_id        INTEGER NOT NULL REFERENCES speakers(id),
                 vector            BLOB    NOT NULL,
                 embed_model_id    TEXT    NOT NULL,
                 source_segment_id INTEGER REFERENCES segments(id),
                 is_golden         INTEGER NOT NULL DEFAULT 0,
                 created_at        INTEGER NOT NULL DEFAULT 0
             );

             CREATE TABLE IF NOT EXISTS embeddings (
                 id             INTEGER PRIMARY KEY,
                 segment_id     INTEGER NOT NULL REFERENCES segments(id),
                 vector         BLOB    NOT NULL,
                 embed_model_id TEXT    NOT NULL
             );

             CREATE TABLE IF NOT EXISTS golden_samples (
                 id          INTEGER PRIMARY KEY,
                 speaker_id  INTEGER NOT NULL REFERENCES speakers(id),
                 audio_path  TEXT    NOT NULL,
                 duration_s  REAL    NOT NULL
             );

             CREATE INDEX IF NOT EXISTS idx_prototypes_speaker
                 ON speaker_prototypes(speaker_id, embed_model_id);
             CREATE INDEX IF NOT EXISTS idx_embeddings_segment ON embeddings(segment_id);
             CREATE INDEX IF NOT EXISTS idx_speakers_merged ON speakers(merged_into);

             -- One hop resolves any speaker to its canonical row, because
             -- merges never chain.
             CREATE VIEW IF NOT EXISTS speaker_resolved AS
                 SELECT s.id                                AS id,
                        COALESCE(t.id, s.id)                AS canonical_id,
                        COALESCE(t.display_name, s.display_name) AS display_name
                 FROM speakers s
                 LEFT JOIN speakers t ON t.id = s.merged_into;

             CREATE VIRTUAL TABLE IF NOT EXISTS segments_fts
                 USING fts5(text, content='segments', content_rowid='id');",
        )?;

        for (name, decl) in [
            ("text", "TEXT"),
            ("lang", "TEXT"),
            ("asr_model_id", "TEXT"),
            ("overlap_frac", "REAL"),
            ("speaker_id", "INTEGER REFERENCES speakers(id)"),
            ("match_score", "REAL"),
            ("deleted_at", "INTEGER"),
        ] {
            self.add_column_if_missing("segments", name, decl)?;
        }

        // Triggers reference `segments.text`, so they come after the column.
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_segments_speaker ON segments(speaker_id);

             CREATE TRIGGER IF NOT EXISTS segments_fts_insert AFTER INSERT ON segments BEGIN
                 INSERT INTO segments_fts(rowid, text) VALUES (new.id, new.text);
             END;
             CREATE TRIGGER IF NOT EXISTS segments_fts_delete AFTER DELETE ON segments BEGIN
                 INSERT INTO segments_fts(segments_fts, rowid, text)
                     VALUES ('delete', old.id, old.text);
             END;
             -- Transcripts arrive by UPDATE (the row is written the moment the
             -- audio is, long before ASR runs), so the update trigger is not
             -- optional bookkeeping — it is the one that indexes everything.
             CREATE TRIGGER IF NOT EXISTS segments_fts_update AFTER UPDATE ON segments BEGIN
                 INSERT INTO segments_fts(segments_fts, rowid, text)
                     VALUES ('delete', old.id, old.text);
                 INSERT INTO segments_fts(rowid, text) VALUES (new.id, new.text);
             END;",
        )?;
        Ok(())
    }

    /// Everything schema v3 adds, written so it is a no-op on a v3 database.
    ///
    /// Neither table references `sessions`: the roster is observed from
    /// VRChat's log, which knows nothing about capture sessions, and the audit
    /// trail has to outlive the rows it describes (that is the point of it).
    fn apply_v3(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_roster (
                 id               INTEGER PRIMARY KEY,
                 world_id         TEXT,
                 instance         TEXT,
                 display_name     TEXT    NOT NULL,
                 joined_at_utc_ns INTEGER NOT NULL,
                 left_at_utc_ns   INTEGER
             );

             CREATE TABLE IF NOT EXISTS operations (
                 id          INTEGER PRIMARY KEY,
                 op          TEXT    NOT NULL,
                 target_ids  TEXT    NOT NULL,
                 prior_state TEXT    NOT NULL,
                 at_utc_ns   INTEGER NOT NULL
             );

             CREATE INDEX IF NOT EXISTS idx_roster_open
                 ON session_roster(display_name, left_at_utc_ns);
             CREATE INDEX IF NOT EXISTS idx_roster_joined
                 ON session_roster(joined_at_utc_ns);
             CREATE INDEX IF NOT EXISTS idx_operations_at ON operations(at_utc_ns);",
        )?;

        // Columns the first real client asked for: an unnamed voice has to be
        // distinguishable from a named one, and a source has to say when it was
        // last seen, not only when it was first.
        let fresh_auto = self.add_column_if_missing("speakers", "auto_label", "TEXT")?;
        self.add_column_if_missing("speakers", "named_at", "INTEGER")?;
        let fresh_last_seen = self.add_column_if_missing("sources", "last_seen", "INTEGER")?;
        if fresh_auto {
            // Backfill: the generated label for an existing row is the one it
            // would have been minted with, and a display name that differs from
            // it is a name the user chose.
            self.conn.execute_batch(
                "UPDATE speakers SET auto_label = 'Speaker_' || printf('%02d', id)
                     WHERE auto_label IS NULL;
                 UPDATE speakers SET named_at = created_at
                     WHERE named_at IS NULL AND display_name <> auto_label;",
            )?;
        }
        if fresh_last_seen {
            self.conn.execute_batch(
                "UPDATE sources SET last_seen = first_seen WHERE last_seen IS NULL",
            )?;
        }
        Ok(())
    }

    /// Everything schema v4 adds, written so it is a no-op on a v4 database.
    ///
    /// Two small things, both for the microphone: a `kind` on `sources`, so a
    /// client can tell the room-listening device from an application without
    /// string-matching a key, and a `settings` table, so the pinned "You"
    /// speaker is a fact that survives a restart rather than a guess made from
    /// whatever the voicebank happens to contain.
    fn apply_v4(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS settings (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
        // The default is what backfills every pre-v4 row: everything the
        // daemon could capture before v4 was an application.
        let fresh_kind = self.add_column_if_missing(
            "sources",
            "kind",
            &format!("TEXT NOT NULL DEFAULT '{KIND_APP}'"),
        )?;
        if fresh_kind {
            self.conn.execute(
                "UPDATE sources SET kind = ?1 WHERE kind IS NULL OR kind = ''",
                params![KIND_APP],
            )?;
        }
        self.conn
            .execute_batch("CREATE INDEX IF NOT EXISTS idx_sources_kind ON sources(kind);")?;
        Ok(())
    }

    /// Everything schema v5 adds, written so it is a no-op on a v5 database.
    ///
    /// `speakers.languages` is a JSON array (`["de"]`, `["de","en"]`) and NULL
    /// means *any*, which is what every existing voice is. Storing it on the
    /// speaker rather than deriving it per segment is the whole point: one turn
    /// is 1-3 s of audio and the ASR flips language on 12% of those (FINDINGS
    /// §10 / `spike/lang_flip.py`), while a person's languages are stable, so
    /// the standing fact is the one worth writing down.
    ///
    /// The two `segments` columns are provenance. Before v5 the only marker was
    /// `match_score IS NULL`, which conflated three unrelated things — a
    /// microphone pin, a hand reassignment and a split's softened score — and
    /// 0.6.1 adds a fourth (proximity inheritance) that a client has to be able
    /// to distrust specifically. The backfill reads the old convention as
    /// faithfully as it can: a labelled row is `match`, except the pinned "You"
    /// speaker's scoreless rows, which are `mic`.
    fn apply_v5(&self) -> Result<()> {
        let fresh_languages = self.add_column_if_missing("speakers", "languages", "TEXT")?;
        let fresh_label_via = self.add_column_if_missing("segments", "label_via", "TEXT")?;
        self.add_column_if_missing("segments", "lang_via", "TEXT")?;
        let _ = fresh_languages; // NULL is the correct value for every old row.

        if fresh_label_via {
            self.conn.execute(
                "UPDATE segments SET label_via = ?1 WHERE speaker_id IS NOT NULL",
                params![label_via::MATCH],
            )?;
            // The one pre-v5 case that was not a match: the microphone's pin,
            // which is provenance and has always carried a NULL score.
            if let Some(you) = self
                .setting(YOU_SPEAKER_KEY)?
                .and_then(|v| v.parse::<i64>().ok())
            {
                self.conn.execute(
                    "UPDATE segments SET label_via = ?2
                     WHERE speaker_id = ?1 AND match_score IS NULL",
                    params![you, label_via::MIC],
                )?;
            }
        }
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_segments_label_via ON segments(label_via);",
        )?;
        Ok(())
    }

    /// Everything schema v6 adds, written so it is a no-op on a v6 database.
    ///
    /// One table and one column: which conversation a turn belongs to. The
    /// indexes are the whole performance story of the person page — every graph
    /// query below is a group-by over `segments` keyed on `thread_id` or on
    /// `speaker_id`, and GRAPH.md's rule is that a slow query gets a covering
    /// index rather than a cache, because a cache of something derived is just
    /// a second thing that can be wrong.
    ///
    /// The backfill replays `crate::threads` over every existing session in
    /// time order, with the same rule the live path uses.
    ///
    /// It is **not** guaranteed to reproduce what live threading would have
    /// produced, and the difference is worth naming (audit finding #24). A turn
    /// is threaded once, from what was known when it was stored, and
    /// `crate::threads::assign` deliberately never re-threads it afterwards: a
    /// later relabel, merge or split does not move it. The backfill has no
    /// "when it was stored" to work from — it sees today's labels — so a
    /// database whose speakers were merged or split after capture threads
    /// differently on the way up than it did on the way in. That is the price
    /// of not rewriting history on every rename, and it is paid once, at the
    /// migration, rather than continuously.
    fn apply_v6(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS threads (
                 id         INTEGER PRIMARY KEY,
                 session_id INTEGER NOT NULL REFERENCES sessions(id),
                 started_ns INTEGER NOT NULL,
                 ended_ns   INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_threads_session
                 ON threads(session_id, ended_ns);",
        )?;
        let fresh = self.add_column_if_missing("segments", "thread_id", "INTEGER")?;
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_segments_thread
                 ON segments(thread_id, t_start_ns);
             -- The threading write path asks one question per stored turn:
             -- 'what has been said in this session lately'. Without this it is
             -- a scan of every segment ever captured.
             CREATE INDEX IF NOT EXISTS idx_segments_session_start
                 ON segments(session_id, t_start_ns);
             -- person.edges groups a person's threads by the other voices in
             -- them; this is the covering index for that group-by.
             CREATE INDEX IF NOT EXISTS idx_segments_speaker_thread
                 ON segments(speaker_id, thread_id);",
        )?;
        if fresh {
            self.backfill_threads()?;
        }
        Ok(())
    }

    /// Everything schema v7 adds, written so it is a no-op on a v7 database.
    ///
    /// The memory graph's Tiers 2 and 3 (GRAPH.md). Two tables and three
    /// columns, and every one of them is an **annotation referencing a
    /// segment**, never a modification of one: the transcript stays the source
    /// of truth and all of this is re-derivable from it.
    ///
    /// There is no backfill. Tier 2 runs on the way in (the pipeline extracts
    /// each turn as it is stored) and Tier 3 is opt-in and idle-only, so a
    /// database migrated up to v7 simply has no derived rows yet — which is the
    /// correct state for a feature that is off by default. Turning the
    /// enrichment on walks the history at its own pace.
    fn apply_v7(&self) -> Result<()> {
        self.conn.execute_batch(
            // When a turn was talking about, resolved against the moment it was
            // said (`crate::timeref`). `raw` is the phrase as spoken, because a
            // date nobody can trace back to a word is not evidence of anything.
            "CREATE TABLE IF NOT EXISTS time_refs (
                 id              INTEGER PRIMARY KEY,
                 segment_id      INTEGER NOT NULL REFERENCES segments(id),
                 raw             TEXT    NOT NULL,
                 resolved_utc_ns INTEGER NOT NULL,
                 kind            TEXT    NOT NULL,
                 extractor       TEXT    NOT NULL,
                 version         INTEGER NOT NULL,
                 created_at      INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_time_refs_segment ON time_refs(segment_id);
             CREATE INDEX IF NOT EXISTS idx_time_refs_due ON time_refs(resolved_utc_ns);

             -- Who owes what to whom. ONE row per segment, deliberately: the
             -- LLM pass upgrades a rule candidate in place rather than filing a
             -- second opinion next to it, so a person never has to read the
             -- same promise twice and decide which copy is real.
             CREATE TABLE IF NOT EXISTS commitments (
                 id             INTEGER PRIMARY KEY,
                 segment_id     INTEGER NOT NULL UNIQUE REFERENCES segments(id),
                 -- SET NULL rather than a plain reference: a thread is only an
                 -- index into the transcript and is deleted the moment nothing
                 -- live is left in it, which must not be able to take a
                 -- commitment whose own segment is merely hidden.
                 thread_id      INTEGER REFERENCES threads(id) ON DELETE SET NULL,
                 who_speaker_id INTEGER REFERENCES speakers(id),
                 to_speaker_id  INTEGER REFERENCES speakers(id),
                 what           TEXT    NOT NULL,
                 due_utc_ns     INTEGER,
                 due_raw        TEXT,
                 due_kind       TEXT,
                 -- candidate | confirmed | done | dismissed. Nothing but a
                 -- human click ever moves a row off `candidate`.
                 state          TEXT    NOT NULL,
                 -- rules | llm. Rendered differently, because they are not the
                 -- same claim.
                 source         TEXT    NOT NULL,
                 model_id       TEXT,
                 confidence     REAL,
                 created_at     INTEGER NOT NULL,
                 updated_at     INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_commitments_state
                 ON commitments(state, due_utc_ns);
             CREATE INDEX IF NOT EXISTS idx_commitments_thread ON commitments(thread_id);
             CREATE INDEX IF NOT EXISTS idx_commitments_who ON commitments(who_speaker_id);
             CREATE INDEX IF NOT EXISTS idx_commitments_to ON commitments(to_speaker_id);",
        )?;

        // A short label for a conversation, written by the Tier 3 pass. On the
        // thread rather than in a table of its own: it is one string per
        // conversation, it dies with the conversation, and a `topics` table
        // would be a second thing that can disagree with `threads`.
        self.add_column_if_missing("threads", "topic", "TEXT")?;
        self.add_column_if_missing("threads", "topic_model_id", "TEXT")?;
        // When the enrichment pass last walked this conversation. NULL means
        // "never", which is what the worker's queue is: threads with no stamp.
        self.add_column_if_missing("threads", "enriched_at", "INTEGER")?;
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_threads_enriched ON threads(enriched_at);
             CREATE INDEX IF NOT EXISTS idx_threads_topic ON threads(topic);",
        )?;
        Ok(())
    }

    /// The accuracy round (0.8.0). Four columns on `segments`, no tables.
    ///
    /// Two of them are on the wire (`text_via`, `asr_confidence`) and two are
    /// the idle worker's queue: a NULL `redecode_at_ns` means no context
    /// re-decode has *considered* this row, and a NULL `confidence_at_ns` means
    /// no cross-check has. They are separate from the answers on purpose — a
    /// re-decode that ran and decided to keep the original words has to be
    /// distinguishable from one that never ran, or the worker walks the same
    /// segment for ever.
    ///
    /// No backfill, deliberately. Every existing row keeps NULLs, which read as
    /// "nothing has looked at this" — true — and the worker walks the history
    /// at idle priority from newest to oldest. Writing `"live"` over rows the
    /// live pass produced before the column existed would be a guess about
    /// provenance, and provenance is the one thing this column exists to state.
    fn apply_v10(&self) -> Result<()> {
        self.add_column_if_missing("segments", "text_via", "TEXT")?;
        self.add_column_if_missing("segments", "asr_confidence", "TEXT")?;
        self.add_column_if_missing("segments", "redecode_at_ns", "INTEGER")?;
        self.add_column_if_missing("segments", "confidence_at_ns", "INTEGER")?;
        // The two worker queues are "the newest rows with no stamp", so the
        // index is on the stamp and the clock together.
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_segments_redecode
                 ON segments(redecode_at_ns, t_start_ns);
             CREATE INDEX IF NOT EXISTS idx_segments_confidence
                 ON segments(confidence_at_ns, t_start_ns);",
        )?;
        Ok(())
    }

    // ---- 0.9.0 (schema v11): the night shift ----------------------------
    //
    /// Two columns on `segments`, both additive and both NULL on every existing
    /// row.
    ///
    /// `night_text` is what the overnight decoder read, kept whether or not it
    /// was allowed to replace anything — the annotate-only rule is a shipped
    /// outcome, not a fallback, and a client renders it as "the night shift
    /// read:". `night_at_ns` is the queue stamp, and it is separate from the
    /// text for the same reason `redecode_at_ns` is separate from `text`: a
    /// night that ran and decided to keep the row's words has to be
    /// distinguishable from a night that never reached it, or the worker walks
    /// the same segment for ever.
    fn apply_v11_night(&self) -> Result<()> {
        self.add_column_if_missing("segments", "night_text", "TEXT")?;
        self.add_column_if_missing("segments", "night_at_ns", "INTEGER")?;
        // The queue is "shaky rows with no night stamp", so the index is on the
        // verdict and the stamp together.
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_segments_night
                 ON segments(night_at_ns, asr_confidence, t_start_ns);",
        )?;
        Ok(())
    }

    /// Thread every session's existing segments by replaying the live rule.
    ///
    /// Deliberately not clever: sessions in id order, turns in time order, the
    /// same [`Threader`] the pipeline uses. A backfill that took a shortcut
    /// would be a second implementation of the rule, and two implementations of
    /// a rule are two rules.
    fn backfill_threads(&self) -> Result<usize> {
        let gap_s = crate::config::GraphConfig::default().thread_gap_s;
        let sessions: Vec<i64> = self
            .conn
            .prepare("SELECT id FROM sessions ORDER BY id ASC")?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;

        let tx = self.conn.unchecked_transaction()?;
        let mut threaded = 0usize;
        for session_id in sessions {
            let turns: Vec<(i64, Turn)> = tx
                .prepare(
                    "SELECT id, t_start_ns, t_end_ns, speaker_id FROM segments
                     WHERE session_id = ?1 AND deleted_at IS NULL
                     ORDER BY t_start_ns ASC, id ASC",
                )?
                .query_map(params![session_id], |r| {
                    Ok((
                        r.get(0)?,
                        Turn {
                            t_start_ns: r.get(1)?,
                            t_end_ns: r.get(2)?,
                            speaker: r.get(3)?,
                        },
                    ))
                })?
                .collect::<rusqlite::Result<_>>()?;

            let mut threader = Threader::new(gap_s);
            for (segment_id, turn) in turns {
                let thread_id = threader.push(&turn, |t| -> Result<i64> {
                    tx.execute(
                        "INSERT INTO threads (session_id, started_ns, ended_ns)
                         VALUES (?1, ?2, ?3)",
                        params![session_id, t.t_start_ns, t.t_end_ns],
                    )?;
                    Ok(tx.last_insert_rowid())
                })?;
                tx.execute(
                    "UPDATE segments SET thread_id = ?2 WHERE id = ?1",
                    params![segment_id, thread_id],
                )?;
                tx.execute(
                    "UPDATE threads SET ended_ns = MAX(ended_ns, ?2) WHERE id = ?1",
                    params![thread_id, turn.t_end_ns],
                )?;
                threaded += 1;
            }
        }
        tx.commit()?;
        Ok(threaded)
    }

    /// Returns whether the column had to be added, so a caller can backfill it.
    fn add_column_if_missing(&self, table: &str, column: &str, decl: &str) -> Result<bool> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let existing: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<_>>()?;
        if existing.iter().any(|c| c == column) {
            return Ok(false);
        }
        self.conn
            .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))
            .with_context(|| format!("adding {table}.{column}"))?;
        Ok(true)
    }

    // ---- sources / sessions / segments (Step 1) --------------------------

    /// Record that a source exists, without changing an existing `allowed`
    /// flag. Returns its row id.
    ///
    /// Unknown programs are written here on sight — that is what makes
    /// default-deny usable: the user can see what was refused and opt in.
    pub fn upsert_source(
        &self,
        match_key: &str,
        display_name: &str,
        first_seen: i64,
    ) -> Result<i64> {
        self.upsert_source_kind(match_key, display_name, KIND_APP, first_seen)
    }

    /// `upsert_source` for a source that is not an application. The kind is
    /// overwritten on conflict, so a row that predates v4 and happens to carry
    /// the mic's key is corrected rather than left lying.
    pub fn upsert_source_kind(
        &self,
        match_key: &str,
        display_name: &str,
        kind: &str,
        first_seen: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO sources (match_key, display_name, kind, allowed, first_seen, last_seen)
             VALUES (?1, ?2, ?3, 0, ?4, ?4)
             ON CONFLICT(match_key) DO UPDATE SET
                 display_name = excluded.display_name,
                 kind = excluded.kind,
                 last_seen = MAX(COALESCE(sources.last_seen, 0), excluded.last_seen)",
            params![match_key, display_name, kind, first_seen],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM sources WHERE match_key = ?1",
            params![match_key],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Which kind of source a capture session belongs to — `"app"` or `"mic"`.
    /// The pipeline asks once per session: a mic turn takes a different route
    /// through identity, and it must be the *row* that says so, not a flag the
    /// capture side hoped would survive the queue.
    pub fn session_source_kind(&self, session_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT COALESCE(sc.kind, ?2) FROM sessions ss
                 JOIN sources sc ON sc.id = ss.source_id
                 WHERE ss.id = ?1",
                params![session_id, KIND_APP],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    // ---- 0.12.1: per-user Discord audio ------------------------------------

    /// The voice a per-user Discord session is pinned to, read from the row
    /// rather than remembered by whoever opened the session.
    ///
    /// The user id lives in `sessions.instance_key`, not in the source's match
    /// key: `instance_key` is what the schema already means by "which instance
    /// of this source", and parsing an identity back out of a display string
    /// would be a second place to get it wrong. `NULL` for every other kind of
    /// session, and for a per-user session whose account is somehow unlinked —
    /// both of which the caller must read as "take the ordinary route", never
    /// as "pin it to nobody".
    pub fn discord_session_speaker(&self, session_id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT d.speaker_id FROM sessions ss
                 JOIN sources sc ON sc.id = ss.source_id
                 JOIN discord_users d ON d.user_id = ss.instance_key
                 WHERE ss.id = ?1 AND sc.kind = ?2",
                params![session_id, KIND_DISCORD_USER],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten())
    }

    // ---- end 0.12.1 ---------------------------------------------------------

    /// The session's source match key (`VRChat.exe`) — the same string
    /// `SegmentRow::source` carries.
    ///
    /// Added for 0.11.0's partial turns, which have to name their source before
    /// any row of that session exists. Read once per session and cached by the
    /// caller, exactly like [`Self::session_source_kind`] beside it.
    pub fn session_source_key(&self, session_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT sc.match_key FROM sessions ss
                 JOIN sources sc ON sc.id = ss.source_id
                 WHERE ss.id = ?1",
                params![session_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Which *copy* of the application this session was opened for (v14):
    /// `serial:<object.serial>`, `pid:<pid>`, or NULL for a session opened
    /// before v14 or by a node that advertised neither.
    ///
    /// NULL is not "instance one". 0.12.2's mute reads this to say *which*
    /// client it silenced, and a row that cannot say is shown as unknown
    /// rather than folded in with the others.
    pub fn session_instance_key(&self, session_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT instance_key FROM sessions WHERE id = ?1",
                params![session_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Mirror a config rule into the DB so `sources` can show it.
    pub fn set_allowed(&self, match_key: &str, allowed: bool, first_seen: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sources (match_key, display_name, allowed, first_seen, last_seen)
             VALUES (?1, ?1, ?2, ?3, ?3)
             ON CONFLICT(match_key) DO UPDATE SET allowed = excluded.allowed",
            params![match_key, allowed as i64, first_seen],
        )?;
        Ok(())
    }

    /// The columns of one `sources` row, in the order [`Self::source_row_from`]
    /// reads them. One string so `sources.list` and the single-row read behind
    /// a `source` event cannot disagree about the shape (finding #2).
    const SOURCE_COLUMNS: &'static str =
        "s.id, s.match_key, s.display_name, COALESCE(s.kind, ?1), s.allowed,
         s.first_seen, COALESCE(s.last_seen, s.first_seen),
         (SELECT COUNT(*) FROM sessions ss
           WHERE ss.source_id = s.id AND ss.ended_at_utc_ns IS NULL)";

    fn source_row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<SourceRow> {
        Ok(SourceRow {
            id: r.get(0)?,
            match_key: r.get(1)?,
            display_name: r.get(2)?,
            kind: r.get(3)?,
            allowed: r.get::<_, i64>(4)? != 0,
            first_seen: r.get(5)?,
            last_seen: r.get(6)?,
            streams: r.get(7)?,
        })
    }

    pub fn list_sources(&self) -> Result<Vec<SourceRow>> {
        let sql = format!(
            "SELECT {} FROM sources s ORDER BY s.allowed DESC, s.match_key ASC",
            Self::SOURCE_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![KIND_APP], Self::source_row_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// One source by its match key, in the same shape `sources.list` returns.
    /// The capture thread reads this back after every change it makes to the
    /// table, so the `source` event it publishes says what a client would have
    /// seen had it re-listed (finding #2).
    pub fn source_row(&self, match_key: &str) -> Result<Option<SourceRow>> {
        let sql = format!(
            "SELECT {} FROM sources s WHERE s.match_key = ?2",
            Self::SOURCE_COLUMNS
        );
        Ok(self
            .conn
            .query_row(&sql, params![KIND_APP, match_key], Self::source_row_from)
            .optional()?)
    }

    pub fn begin_session(&self, source_id: i64, started_at_utc_ns: i64) -> Result<i64> {
        self.begin_session_for(source_id, started_at_utc_ns, None)
    }

    // ---- 0.12.0: which instance a session was ------------------------------

    /// `begin_session`, remembering *which copy of the application* opened it.
    ///
    /// ## The failure this exists to end
    ///
    /// `sources.match_key` is derived from `application.process.binary`, so two
    /// simultaneously-running copies of one app — two Vesktop clients signed
    /// into two Discord accounts — collapse into a single `sources` row and a
    /// single name on every turn. §29 is the bill for that: one account's call
    /// carried the RecallBridge plugin and one did not, both landed under
    /// `"vesktop"`, and Discord's ground truth was therefore asked about turns
    /// it had never been able to see. It answered `nobody`, correctly and
    /// uselessly, 190 times, and four real people came within one `--apply` of
    /// having their labels stripped on the strength of it.
    ///
    /// ## Why the discriminator goes here and not on the source
    ///
    /// The obvious fix — suffix `match_key` with the pid — is a trap, and the
    /// blast radius is worth writing down because it is not obvious:
    /// `allowlist::decide` looks the key up in the `[rules]` table by exact
    /// string, so `"vesktop#4711"` matches no rule, falls through to
    /// default-deny, and **capture silently stops for an app the user allowed**.
    /// Every `[rules.X]` entry, `recalld allow <KEY>`, the GUI's source card and
    /// its per-source search filter are keyed the same way.
    ///
    /// A session is already per-PipeWire-node — `Shared::captures` is keyed by
    /// `node_id`, so two instances already open two concurrent `sessions` rows
    /// against the one source. The instance identity was in `NodeInfo` at that
    /// exact call site and was thrown away. This column stops throwing it away.
    /// Nothing keyed on `match_key` changes, so nothing above breaks.
    ///
    /// ## What it is, and what it deliberately is not
    ///
    /// `object.serial` where PipeWire gave one — never reused within a boot —
    /// else `application.process.id`. It identifies a *stream*, not an account:
    /// it cannot say which instance is the bridge's, and it must not be read as
    /// if it could. Deciding that needs the plugin to name the call it is
    /// watching, which is a wire change and is not in this round. What the
    /// column buys today is that the question becomes *answerable* from data
    /// the daemon is already collecting, where before tonight it was not.
    ///
    /// NULL means a session recorded before this column existed, or one opened
    /// by a node that carried neither property. NULL is not "one instance".
    pub fn begin_session_for(
        &self,
        source_id: i64,
        started_at_utc_ns: i64,
        instance_key: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO sessions (source_id, started_at_utc_ns, instance_key)
             VALUES (?1, ?2, ?3)",
            params![source_id, started_at_utc_ns, instance_key],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    fn apply_session_instance(&self) -> Result<()> {
        self.add_column_if_missing("sessions", "instance_key", "TEXT")?;
        Ok(())
    }

    // ---- end 0.12.0 --------------------------------------------------------

    pub fn end_session(&self, session_id: i64, ended_at_utc_ns: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET ended_at_utc_ns = ?2 WHERE id = ?1 AND ended_at_utc_ns IS NULL",
            params![session_id, ended_at_utc_ns],
        )?;
        Ok(())
    }

    /// Close any session left open by a crash or a kill, so orphan rows do not
    /// accumulate across restarts.
    pub fn close_dangling_sessions(&self, at_utc_ns: i64) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE sessions SET ended_at_utc_ns = ?1 WHERE ended_at_utc_ns IS NULL",
            params![at_utc_ns],
        )?;
        Ok(n)
    }

    /// `audio_path` is stored relative to the data dir so the tree can be moved.
    pub fn insert_segment(
        &self,
        session_id: i64,
        t_start_ns: i64,
        t_end_ns: i64,
        audio_path: &str,
        created_at: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO segments (session_id, t_start_ns, t_end_ns, audio_path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, t_start_ns, t_end_ns, audio_path, created_at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    // ---- 0.12.4: cutting a turn where the speaker changes -----------------

    /// Archive turns worth looking for a speaker change in
    /// (`crate::turnsplit`).
    ///
    /// `partial` and `overlap` and nothing else, because those are the two
    /// verdicts that *mean* the row holds more than one person's audio, and a
    /// pass that walked `single` rows would be re-deciding turns Discord has
    /// already settled. A row already produced by a split is skipped by the
    /// same rule the live path uses — it has its own verdict now.
    ///
    /// Rows whose clip retention has taken are still returned: the caller
    /// counts them, because on an archive older than `[retention].audio_days`
    /// "no audio" is the whole answer and an empty table would not say so.
    pub fn segments_for_resplit(&self, limit: usize) -> Result<Vec<crate::turnsplit::Candidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.session_id, g.t_start_ns, g.t_end_ns, g.audio_path, g.truth_verdict
               FROM segments g
              WHERE g.deleted_at IS NULL
                AND g.truth_verdict IN (?1, ?2)
              ORDER BY g.t_start_ns ASC, g.id ASC
              LIMIT ?3",
        )?;
        let rows = stmt
            .query_map(
                params![truth_verdict::PARTIAL, truth_verdict::OVERLAP, limit as i64],
                |r| {
                    Ok(crate::turnsplit::Candidate {
                        segment_id: r.get(0)?,
                        session_id: r.get(1)?,
                        t_start_ns: r.get(2)?,
                        t_end_ns: r.get(3)?,
                        audio_path: r.get(4)?,
                        verdict: r.get(5)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Cut one archive turn into pieces: the original row keeps the first one
    /// and every other piece becomes a new row.
    ///
    /// **The original row survives**, shortened, rather than being replaced by
    /// two new ones. That is the whole shape of this call and it is not a
    /// convenience: a segment id is referenced by `threads`, `commitments`,
    /// `time_refs`, `notes`, `speaker_prototypes.source_segment_id`,
    /// `segment_vectors` and every correction the user has ever made. Deleting
    /// it and minting two would orphan all of them to save one row.
    ///
    /// Everything the analysis leg owns — the words, the language, the
    /// speaker, the overlap reading, the vectors, the verdict — is cleared on
    /// the original, because all of it describes audio the row no longer
    /// covers. The caller re-runs the analysis on each piece afterwards, which
    /// is the same order the live path uses.
    ///
    /// The `operations` row is [`crate::turnsplit::OP_RESPLIT`] and carries
    /// everything [`Self::unsplit_segment`] needs to put the turn back.
    pub fn resplit_segment(
        &self,
        segment_id: i64,
        pieces: &[(i64, i64, String)],
        at_utc_ns: i64,
    ) -> Result<Vec<i64>> {
        if pieces.len() < 2 {
            return Ok(Vec::new());
        }
        // Read the row back inside the call rather than trusting the caller's
        // snapshot: the pass gathers unlocked and something may have moved the
        // row since. A row that has gone, or has been split already, is left
        // exactly as it is.
        type Prior = (i64, i64, String, Option<String>, Option<String>);
        let prior: Option<Prior> = self
            .conn
            .query_row(
                "SELECT t_start_ns, t_end_ns, audio_path, text, truth_verdict
                   FROM segments WHERE id = ?1 AND deleted_at IS NULL",
                params![segment_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((t_start_ns, t_end_ns, audio_path, text, verdict)) = prior else {
            return Ok(Vec::new());
        };

        let (first_start, first_end, first_path) = &pieces[0];
        let mut minted = Vec::with_capacity(pieces.len() - 1);
        for (start, end, path) in &pieces[1..] {
            self.conn.execute(
                "INSERT INTO segments (session_id, t_start_ns, t_end_ns, audio_path, created_at)
                 SELECT session_id, ?2, ?3, ?4, ?5 FROM segments WHERE id = ?1",
                params![segment_id, start, end, path, at_utc_ns],
            )?;
            minted.push(self.conn.last_insert_rowid());
        }
        self.conn.execute(
            "UPDATE segments
                SET t_start_ns = ?2, t_end_ns = ?3, audio_path = ?4,
                    text = NULL, lang = NULL, lang_via = NULL, text_via = NULL,
                    asr_model_id = NULL, asr_confidence = NULL, confidence_at_ns = NULL,
                    overlap_frac = NULL, speaker_id = NULL, match_score = NULL,
                    label_via = NULL, translation = NULL, translation_via = NULL,
                    night_text = NULL, night_at_ns = NULL, redecode_at_ns = NULL,
                    sweep_at_ns = NULL,
                    truth_user_id = NULL, truth_verdict = NULL, truth_coverage = NULL,
                    truth_overlap_frac = NULL, truth_enrol_ns = NULL
              WHERE id = ?1",
            params![segment_id, first_start, first_end, first_path],
        )?;
        // The vectors go with the words: a semantic vector of a sentence the
        // row no longer holds is a wrong answer waiting to be given.
        self.conn.execute(
            "DELETE FROM segment_vectors WHERE segment_id = ?1",
            params![segment_id],
        )?;
        self.conn.execute(
            "DELETE FROM embeddings WHERE segment_id = ?1",
            params![segment_id],
        )?;

        let mut ids = vec![segment_id];
        ids.extend(&minted);
        self.log_operation(
            crate::turnsplit::OP_RESPLIT,
            &serde_json::to_string(&ids)?,
            &serde_json::json!({
                "segment_id": segment_id,
                "t_start_ns": t_start_ns,
                "t_end_ns": t_end_ns,
                "audio_path": audio_path,
                "text": text,
                "truth_verdict": verdict,
                "minted": minted,
            })
            .to_string(),
            at_utc_ns,
        )?;
        Ok(minted)
    }

    /// Put one split turn back: the original row regains its whole span and
    /// its clip, and the rows the split minted are soft-deleted.
    ///
    /// The words are *not* restored from `prior_state` and that is deliberate:
    /// the row's span is right again but nothing has re-read the audio, so the
    /// honest state is a turn waiting for the analysis leg — the same state
    /// [`Self::resplit_segment`] left it in. Restoring a transcript nothing
    /// just decoded would be asserting a reading of audio this call has not
    /// looked at.
    ///
    /// Returns false when the operation has already been undone or the row is
    /// gone. Idempotent by the span rather than by a flag: a row that already
    /// covers what `prior_state` says it covered was never split, or has been
    /// put back once already, and either way there is nothing to do. Undoing
    /// twice must not delete a second generation of pieces.
    pub fn unsplit_segment(&self, prior: &serde_json::Value, at_utc_ns: i64) -> Result<bool> {
        let Some(segment_id) = prior["segment_id"].as_i64() else {
            return Ok(false);
        };
        let (Some(t_start_ns), Some(t_end_ns), Some(path)) = (
            prior["t_start_ns"].as_i64(),
            prior["t_end_ns"].as_i64(),
            prior["audio_path"].as_str(),
        ) else {
            return Ok(false);
        };
        let n = self.conn.execute(
            "UPDATE segments SET t_start_ns = ?2, t_end_ns = ?3, audio_path = ?4,
                    text = NULL, lang = NULL, lang_via = NULL, text_via = NULL,
                    asr_model_id = NULL, overlap_frac = NULL, speaker_id = NULL,
                    match_score = NULL, label_via = NULL,
                    truth_user_id = NULL, truth_verdict = NULL, truth_coverage = NULL,
                    truth_overlap_frac = NULL
              WHERE id = ?1 AND deleted_at IS NULL
                AND (t_start_ns != ?2 OR t_end_ns != ?3)",
            params![segment_id, t_start_ns, t_end_ns, path],
        )?;
        if n == 0 {
            return Ok(false);
        }
        self.conn.execute(
            "DELETE FROM segment_vectors WHERE segment_id = ?1",
            params![segment_id],
        )?;
        self.conn.execute(
            "DELETE FROM embeddings WHERE segment_id = ?1",
            params![segment_id],
        )?;
        for id in prior["minted"].as_array().into_iter().flatten() {
            let Some(id) = id.as_i64() else { continue };
            self.conn.execute(
                "UPDATE segments SET deleted_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
                params![id, at_utc_ns],
            )?;
        }
        Ok(true)
    }

    /// Every applied `turns.resplit`, newest first — the order an undo wants.
    pub fn resplit_operations(&self, limit: usize) -> Result<Vec<OperationRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, op, target_ids, prior_state, at_utc_ns
               FROM operations WHERE op = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![crate::turnsplit::OP_RESPLIT, limit as i64], |r| {
                Ok(OperationRow {
                    id: r.get(0)?,
                    op: r.get(1)?,
                    target_ids: r.get(2)?,
                    prior_state: r.get(3)?,
                    at_utc_ns: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- end 0.12.4 -------------------------------------------------------

    pub fn segment_count(&self, session_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM segments WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?)
    }

    // ---- analysis --------------------------------------------------------

    pub fn set_segment_analysis(&self, segment_id: i64, a: &SegmentAnalysis) -> Result<()> {
        // `sweep_at_ns = NULL` for [`Self::clear_segment_sweep`]'s reason: the
        // archive sweep's pre-filter reads this text, so a row whose words have
        // just been written is a row nobody has pre-filtered. Almost always a
        // no-op — a segment normally reaches this call once, before any sweep
        // has been near it — and it is here so that the invariant holds on the
        // one path where it does not: a row re-analysed after `replay`.
        self.conn.execute(
            "UPDATE segments
             SET text = ?2, lang = ?3, lang_via = ?4, asr_model_id = ?5, overlap_frac = ?6,
                 text_via = 'live', sweep_at_ns = NULL
             WHERE id = ?1",
            params![
                segment_id,
                a.text,
                a.lang,
                a.lang_via,
                a.asr_model_id,
                a.overlap_frac.map(|v| v as f64)
            ],
        )?;
        Ok(())
    }

    /// Replace a transcript after a constrained re-decode, keeping the row's
    /// provenance honest: the ASR that actually produced these words is the one
    /// recorded, not the one that produced the words being replaced.
    pub fn set_segment_text_from_redecode(
        &self,
        segment_id: i64,
        text: &str,
        lang: &str,
        asr_model_id: &str,
    ) -> Result<()> {
        // 0.11.4: the same three rules as every other machine edit of a
        // transcript (`set_segment_text_via`): keep the words being replaced on
        // the record, clear the cross-check verdict that was about them, and
        // drop the translation of them. This path had none of the three.
        let prior: Option<(Option<String>, Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT text, asr_model_id, text_via FROM segments
                 WHERE id = ?1 AND deleted_at IS NULL",
                params![segment_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((prior_text, prior_model, prior_via)) = prior else {
            return Ok(());
        };
        self.conn.execute(
            "UPDATE segments SET text = ?2, lang = ?3, lang_via = ?4, asr_model_id = ?5,
                 text_via = ?6, asr_confidence = NULL, confidence_at_ns = NULL
             WHERE id = ?1",
            params![
                segment_id,
                text,
                lang,
                lang_via::REDECODE,
                asr_model_id,
                text_via::ARBITER
            ],
        )?;
        self.clear_segment_translation(segment_id)?;
        self.log_operation(
            "segments.redecode",
            &format!("[{segment_id}]"),
            &serde_json::json!({
                "segment_id": segment_id,
                "text": prior_text,
                "asr_model_id": prior_model,
                "text_via": prior_via,
                "route": text_via::ARBITER,
            })
            .to_string(),
            crate::clock::utc_now_ns(),
        )?;
        Ok(())
    }

    // ---- the accuracy round (0.8.0, `crate::quality`) --------------------

    /// Turns short enough to be worth re-decoding with their neighbours, newest
    /// first, that no re-decode has considered yet.
    ///
    /// The filters are the guards, in SQL because they are cheap there and
    /// because a worker that read rows it must not touch would be one bug away
    /// from touching them: live rows only, with audio still on disk, shorter
    /// than `below_s`, and never a row a person has corrected by hand —
    /// `text_via` is NULL or `live`, so a turn is re-decoded once and an
    /// arbiter's or a person's words are never overwritten.
    ///
    /// **A turn with no transcript at all is in the queue, and is the point.**
    /// The live pass stores an empty decode as NULL text
    /// (`crate::analysis::Analyzer::prepare`), and a 1.4 s fragment that the
    /// decoder made nothing of is exactly the case the context window rescues —
    /// measured here, not argued: on the middle third of `clean_single_0.wav`
    /// parakeet v3 returns nothing alone and "and a violin were" in its
    /// neighbours' company. What the queue does need is evidence that the live
    /// pass has *been* here, which is `asr_model_id`; a row without one is
    /// still waiting for the inference thread.
    pub fn segments_for_context_redecode(
        &self,
        below_s: f32,
        limit: usize,
    ) -> Result<Vec<RedecodeCandidate>> {
        let below_ns = (below_s.max(0.0) as f64 * 1e9) as i64;
        let rows = self
            .conn
            .prepare(
                "SELECT g.id, g.session_id, g.t_start_ns, g.t_end_ns, g.audio_path, g.text
                 FROM segments g
                 WHERE g.deleted_at IS NULL
                   AND g.redecode_at_ns IS NULL
                   AND g.asr_model_id IS NOT NULL
                   AND g.audio_path <> ''
                   AND (g.t_end_ns - g.t_start_ns) < ?1
                   AND (g.text_via IS NULL OR g.text_via = 'live')
                   AND NOT EXISTS (
                       SELECT 1 FROM operations o
                       WHERE o.op = 'segments.correct'
                         AND o.target_ids = '[' || g.id || ']')
                 ORDER BY g.t_start_ns DESC
                 LIMIT ?2",
            )?
            .query_map(params![below_ns, limit as i64], |r| {
                Ok(RedecodeCandidate {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    t_start_ns: r.get(2)?,
                    t_end_ns: r.get(3)?,
                    audio_path: r.get(4)?,
                    text: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The stored clips of one session that touch `[from_ns, to_ns]`, in time
    /// order, so the worker can rebuild the audio around a turn.
    ///
    /// There is no continuous recording — the daemon stores one WAV per turn —
    /// so this is the whole of what "surrounding audio" can mean.
    pub fn session_clips_between(
        &self,
        session_id: i64,
        from_ns: i64,
        to_ns: i64,
    ) -> Result<Vec<Clip>> {
        let rows = self
            .conn
            .prepare(
                "SELECT id, t_start_ns, t_end_ns, audio_path FROM segments
                 WHERE session_id = ?1 AND deleted_at IS NULL AND audio_path <> ''
                   AND t_end_ns >= ?2 AND t_start_ns <= ?3
                 ORDER BY t_start_ns ASC, id ASC",
            )?
            .query_map(params![session_id, from_ns, to_ns], |r| {
                Ok(Clip {
                    id: r.get(0)?,
                    t_start_ns: r.get(1)?,
                    t_end_ns: r.get(2)?,
                    audio_path: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Replace a transcript with the words a context re-decode read out of the
    /// turn's own span. The language is untouched: the words moved, and which
    /// language they are in did not.
    pub fn set_segment_text_from_context(
        &self,
        segment_id: i64,
        text: &str,
        asr_model_id: &str,
        at_utc_ns: i64,
    ) -> Result<()> {
        self.set_segment_text_via(segment_id, text, asr_model_id, text_via::CONTEXT, at_utc_ns)
    }

    /// The same replacement, for any offline route that produces words
    /// (0.9.0). Generalised rather than copied: the night shift writes
    /// `text_via = 'night'` and everything else about the operation — the
    /// prior text kept in `operations` as `segments.redecode`, the cleared
    /// cross-check verdict, the stamp — has to be identical, because a person
    /// comparing or reverting two machine edits should not have to know which
    /// worker made them.
    pub fn set_segment_text_via(
        &self,
        segment_id: i64,
        text: &str,
        asr_model_id: &str,
        via: &str,
        at_utc_ns: i64,
    ) -> Result<()> {
        // The words being replaced are kept, the same way `segments.correct`
        // keeps them: a re-decode is a machine's edit of the transcript, and an
        // edit nobody can see or undo is not a provenance story, it is a
        // rewrite. The row goes to `operations` as `segments.redecode` with the
        // prior text, model and route, so a person can compare, revert, and
        // the real-audio benefit of the pass can be measured after the fact.
        let prior: Option<(Option<String>, Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT text, asr_model_id, text_via FROM segments
                 WHERE id = ?1 AND deleted_at IS NULL",
                params![segment_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((prior_text, prior_model, prior_via)) = prior else {
            return Ok(());
        };
        // The cross-check is cleared with the words it was about: a `solid`
        // flag on a transcript that has since been replaced is a claim nobody
        // ever checked. The confidence pass picks the row up again on its next
        // walk, against the new text.
        // `sweep_at_ns` goes with them, and for the third version of the same
        // reason (0.12.1). The archive sweep's cheapest decision is a read of
        // this very text — `pre_route` declines a row whose transcript already
        // reads as something — so a row whose words have just changed is a row
        // nobody has pre-filtered. Left set, a turn re-decoded into readable
        // German would be declined for ever on the strength of the unreadable
        // mumble it used to be.
        self.conn.execute(
            "UPDATE segments
             SET text = ?2, asr_model_id = ?3, text_via = ?4, redecode_at_ns = ?5,
                 asr_confidence = NULL, confidence_at_ns = NULL, sweep_at_ns = NULL
             WHERE id = ?1 AND deleted_at IS NULL",
            params![segment_id, text, asr_model_id, via, at_utc_ns],
        )?;
        // …and so is the translation, for exactly the same reason and one
        // sharper. `clear_segment_translation` documents it — "a turn whose
        // words changed has a translation of words nobody said any more" — and
        // until 0.11.x it had no caller outside a test. That is worse than a
        // stale confidence flag: `segments_for_translation` queues on
        // `translation_via IS NULL`, so a row translated once was never
        // revisited, and the translation of the *pre-re-decode* text was
        // served on the wire for ever, under a `via` naming a model that had
        // translated something else. Cleared here, the ordinary pass picks the
        // row up again against the words it now says.
        self.clear_segment_translation(segment_id)?;
        self.log_operation(
            "segments.redecode",
            &format!("[{segment_id}]"),
            &serde_json::json!({
                "segment_id": segment_id,
                "text": prior_text,
                "asr_model_id": prior_model,
                "text_via": prior_via,
            })
            .to_string(),
            at_utc_ns,
        )?;
        Ok(())
    }

    /// Record that the re-decode worker has considered a row and left it alone.
    /// Without this a rejected re-decode is retried for ever.
    pub fn mark_redecode_considered(&self, segment_id: i64, at_utc_ns: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE segments SET redecode_at_ns = ?2 WHERE id = ?1",
            params![segment_id, at_utc_ns],
        )?;
        Ok(())
    }

    /// Turns waiting for a cross-check, newest first: transcribed, audio still
    /// on disk, never checked, and not hand-corrected — a person's own words
    /// need no second opinion.
    pub fn segments_for_confidence(&self, limit: usize) -> Result<Vec<RedecodeCandidate>> {
        let rows = self
            .conn
            .prepare(
                "SELECT g.id, g.session_id, g.t_start_ns, g.t_end_ns, g.audio_path, g.text
                 FROM segments g
                 WHERE g.deleted_at IS NULL
                   AND g.confidence_at_ns IS NULL
                   AND g.text IS NOT NULL AND LENGTH(TRIM(g.text)) > 0
                   AND g.audio_path <> ''
                   AND NOT EXISTS (
                       SELECT 1 FROM operations o
                       WHERE o.op = 'segments.correct'
                         AND o.target_ids = '[' || g.id || ']')
                 ORDER BY g.t_start_ns DESC
                 LIMIT ?1",
            )?
            .query_map(params![limit as i64], |r| {
                Ok(RedecodeCandidate {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    t_start_ns: r.get(2)?,
                    t_end_ns: r.get(3)?,
                    audio_path: r.get(4)?,
                    text: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- the night shift (0.9.0) -----------------------------------------

    /// Turns waiting for the overnight third reading, newest first.
    ///
    /// The queue is deliberately narrow: **only rows the cross-check called
    /// `shaky`**. That is the whole measured case for the feature (FINDINGS
    /// §12: shaky rows disagree with large-v3 76% of the time against 27% on
    /// solid ones), and it is also the only place where the cost is justified —
    /// a third reading of a row two decoders already agree on buys nothing and
    /// costs a GPU.
    ///
    /// The rest of the filters are the same guards the other two queues carry,
    /// in SQL for the same reason: audio still on disk, a transcript to
    /// compare against, never a row a person has corrected by hand, and a NULL
    /// `night_at_ns` so a row is read once and not every night.
    pub fn segments_for_night(&self, limit: usize) -> Result<Vec<RedecodeCandidate>> {
        let rows = self
            .conn
            .prepare(
                "SELECT g.id, g.session_id, g.t_start_ns, g.t_end_ns, g.audio_path, g.text
                 FROM segments g
                 WHERE g.deleted_at IS NULL
                   AND g.night_at_ns IS NULL
                   AND g.asr_confidence = 'shaky'
                   AND g.text IS NOT NULL AND LENGTH(TRIM(g.text)) > 0
                   AND g.audio_path <> ''
                   AND NOT EXISTS (
                       SELECT 1 FROM operations o
                       WHERE o.op = 'segments.correct'
                         AND o.target_ids = '[' || g.id || ']')
                 ORDER BY g.t_start_ns DESC
                 LIMIT ?1",
            )?
            .query_map(params![limit as i64], |r| {
                Ok(RedecodeCandidate {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    t_start_ns: r.get(2)?,
                    t_end_ns: r.get(3)?,
                    audio_path: r.get(4)?,
                    text: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Record what the night shift read, and that it has been here.
    ///
    /// Always called, on every row the worker reaches — including the ones
    /// whose words it was not allowed to touch. `night_text` is the annotation
    /// a client shows as "the night shift read:", and storing it for a row that
    /// was *not* replaced is the point of the annotate-only outcome: the reader
    /// is shown the disagreement and decides, which is the honest answer where
    /// no rule earned the right to decide for them.
    pub fn set_segment_night(
        &self,
        segment_id: i64,
        night_text: Option<&str>,
        at_utc_ns: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE segments SET night_text = ?2, night_at_ns = ?3
             WHERE id = ?1 AND deleted_at IS NULL",
            params![segment_id, night_text, at_utc_ns],
        )?;
        Ok(())
    }

    /// Flag a transcript with what the second decoder made of it. `None` marks
    /// the row checked without a verdict — the cross-check ran and had nothing
    /// to say, usually because it returned no words at all (§11: canary is
    /// empty on 11% of real turns).
    pub fn set_segment_confidence(
        &self,
        segment_id: i64,
        confidence: Option<&str>,
        at_utc_ns: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE segments SET asr_confidence = ?2, confidence_at_ns = ?3
             WHERE id = ?1 AND deleted_at IS NULL",
            params![segment_id, confidence, at_utc_ns],
        )?;
        Ok(())
    }

    /// The transcript's language and its thread's, for feeding the cross-check
    /// a source language. Both may be NULL, which is the caller's cue to decode
    /// in both and keep the better agreement.
    pub fn segment_lang_hint(&self, segment_id: i64) -> Result<(Option<String>, Option<i64>)> {
        Ok(self
            .conn
            .query_row(
                "SELECT lang, thread_id FROM segments WHERE id = ?1",
                params![segment_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, None)))
    }

    // ---- the vocabulary (0.8.0, `crate::vocab`) --------------------------

    /// Display names seen in the VRChat roster, most recently joined first.
    pub fn recent_roster_names(&self, limit: usize) -> Result<Vec<String>> {
        Ok(self
            .conn
            .prepare(
                "SELECT display_name, MAX(joined_at_utc_ns) AS seen
                 FROM session_roster
                 GROUP BY display_name
                 ORDER BY seen DESC
                 LIMIT ?1",
            )?
            .query_map(params![limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Voices the user has actually named. A generated `Speaker_07` is not
    /// vocabulary — it is the absence of it.
    pub fn named_speakers(&self) -> Result<Vec<String>> {
        Ok(self
            .conn
            .prepare(
                "SELECT display_name FROM speakers
                 WHERE named_at IS NOT NULL AND merged_into IS NULL
                 ORDER BY named_at DESC",
            )?
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Words the user's corrections *added* to a transcript, newest first.
    ///
    /// The pre-correction text is in the operation's `prior_state` and the
    /// corrected text is on the row, so the difference is exactly "words the
    /// model did not know" — which is what a glossary is.
    pub fn correction_terms(&self, limit: usize) -> Result<Vec<String>> {
        let rows: Vec<(String, Option<String>)> = self
            .conn
            .prepare(
                "SELECT o.prior_state, g.text
                 FROM operations o
                 JOIN segments g ON '[' || g.id || ']' = o.target_ids
                 WHERE o.op = 'segments.correct' AND g.deleted_at IS NULL
                 ORDER BY o.at_utc_ns DESC
                 LIMIT ?1",
            )?
            .query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut out: Vec<String> = Vec::new();
        for (prior, after) in rows {
            let before = serde_json::from_str::<serde_json::Value>(&prior)
                .ok()
                .and_then(|v| v.get("text").and_then(|t| t.as_str()).map(str::to_string))
                .unwrap_or_default();
            let Some(after) = after else { continue };
            for word in crate::vocab::corrected_words(&before, &after) {
                if !out.iter().any(|w| w.eq_ignore_ascii_case(&word)) {
                    out.push(word);
                }
            }
        }
        Ok(out)
    }

    /// Mark a segment whose transcript disagrees with its speaker's declared
    /// language when nothing can be done about it. The text stays — it is the
    /// only record of what was said — and `lang` goes back to NULL, because the
    /// classifier's answer and the speaker's declaration cannot both be right
    /// and this daemon cannot tell which is wrong.
    pub fn mark_segment_language_mismatch(&self, segment_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE segments SET lang = NULL, lang_via = ?2 WHERE id = ?1",
            params![segment_id, lang_via::MISMATCH],
        )?;
        Ok(())
    }

    /// Stamp a language the *conversation* supplied (0.7.7, `crate::langctx`).
    ///
    /// Text and `asr_model_id` are untouched, and that is the point: nothing
    /// was re-decoded, so nothing about what was said has changed. Only the
    /// answer to "which language was this" moved, from NULL to an inference
    /// that says out loud that it is one.
    pub fn set_segment_language_from_context(&self, segment_id: i64, lang: &str) -> Result<()> {
        self.set_segment_language(segment_id, lang, lang_via::CONTEXT)
    }

    /// Set a row's language and say where it came from, leaving the transcript
    /// and its `asr_model_id` alone. The one write that moves `lang` without
    /// moving the words — used by the context stamp above and by
    /// `recalld lang repair` when it finds a mark whose disagreement has since
    /// gone away.
    pub fn set_segment_language(&self, segment_id: i64, lang: &str, via: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE segments SET lang = ?2, lang_via = ?3 WHERE id = ?1",
            params![segment_id, lang, via],
        )?;
        Ok(())
    }

    /// The last `limit` **clear** language stamps of one conversation, newest
    /// first, as the raw tags. This is the evidence the thread's language
    /// context is read from (`crate::langctx`).
    ///
    /// Three filters, each load-bearing:
    ///
    /// * `lang IS NOT NULL` — only turns something actually read a language
    ///   out of vote. A NULL is "nobody could tell", not a vote for the other
    ///   side.
    /// * `id != exclude` — the turn being decided does not vote on itself. A
    ///   flip that helped confirm itself would be unfalsifiable.
    /// * `lang_via != 'context'` — an inherited stamp is an echo of this very
    ///   query, not new evidence. Counting it would let three real German turns
    ///   inherit their way to a hundred, and the hundredth would look exactly
    ///   as certain as the first.
    /// * `lang_via != 'sweep'` (0.12.0) — the archive sweep's de/en stamp is
    ///   one second of audio nobody could read, judged by nothing. It is a
    ///   record of what the identifier said, not a reading of the words, and
    ///   the whole point of the conversational prior is that it is built out of
    ///   turns something actually read.
    pub fn thread_language_stamps(
        &self,
        thread_id: i64,
        exclude: i64,
        limit: usize,
    ) -> Result<Vec<String>> {
        Ok(self
            .conn
            .prepare(
                "SELECT lang FROM segments
                 WHERE thread_id = ?1 AND id != ?2 AND lang IS NOT NULL
                   AND (lang_via IS NULL OR lang_via NOT IN (?3, ?5))
                   AND deleted_at IS NULL
                 ORDER BY t_start_ns DESC, id DESC LIMIT ?4",
            )?
            .query_map(
                params![
                    thread_id,
                    exclude,
                    lang_via::CONTEXT,
                    limit as i64,
                    lang_via::SWEEP
                ],
                |r| r.get(0),
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Everything the language prior needs about one row, in one query: which
    /// conversation it is in, what it says, how its language got there, and —
    /// reduced here rather than in the caller — whether its speaker is pinned
    /// to exactly one language.
    pub fn language_subject(&self, segment_id: i64) -> Result<Option<crate::langctx::Subject>> {
        // The declaration is read through `speaker_resolved`, exactly as
        // `speaker_languages` does, so a voice that was merged away still
        // answers with the surviving voice's declaration rather than its own
        // stale one.
        /// `(thread_id, text, lang_via, the speaker's raw languages column)`.
        type Raw = (Option<i64>, Option<String>, Option<String>, Option<String>);
        let row: Option<Raw> = self
            .conn
            .query_row(
                "SELECT g.thread_id, g.text, g.lang_via,
                        (SELECT s.languages FROM speakers s
                         JOIN speaker_resolved r ON r.canonical_id = s.id
                         WHERE r.id = g.speaker_id)
                 FROM segments g
                 WHERE g.id = ?1 AND g.deleted_at IS NULL",
                params![segment_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        Ok(row.map(|(thread_id, text, lang_via, languages)| {
            let parsed = crate::lang::parse_languages(languages.as_deref());
            crate::langctx::Subject {
                thread_id,
                text,
                lang_via,
                declared: crate::lang::sole_language(parsed.as_ref()).map(str::to_string),
            }
        }))
    }

    // ---- the archive sweep (0.12.0, `crate::sweep`) ----------------------

    /// One untagged archive row, with everything
    /// [`crate::asr_cjk::pre_route`] needs to decide whether to ask about it.
    ///
    /// Deliberately not a [`RedecodeCandidate`]: that struct carries the
    /// session and the timestamps because the quality worker builds a *window*
    /// out of the clips around a turn, and this pass reads exactly one clip.
    /// What it needs instead is the speaker's declaration, which is the one
    /// thing `pre_route` cannot be run without.
    pub fn segments_for_lang_sweep(
        &self,
        min_duration_s: f32,
        limit: usize,
    ) -> Result<Vec<SweepCandidate>> {
        let min_ns = (min_duration_s.max(0.0) as f64 * 1e9) as i64;
        Ok(self
            .conn
            .prepare(
                "SELECT g.id, g.t_end_ns - g.t_start_ns, g.audio_path, g.text,
                        (SELECT s.languages FROM speakers s
                         JOIN speaker_resolved r ON r.canonical_id = s.id
                         WHERE r.id = g.speaker_id)
                 FROM segments g
                 WHERE g.deleted_at IS NULL
                   AND g.lang IS NULL
                   AND g.sweep_at_ns IS NULL
                   AND (g.lang_via IS NULL OR g.lang_via != ?1)
                   AND g.audio_path <> ''
                   AND g.t_end_ns - g.t_start_ns >= ?2
                   AND NOT EXISTS (
                       SELECT 1 FROM operations o
                       WHERE o.op = 'segments.correct'
                         AND o.target_ids = '[' || g.id || ']')
                 ORDER BY g.t_start_ns
                 LIMIT ?3",
            )?
            .query_map(
                params![lang_via::MISMATCH, min_ns, limit.max(1) as i64],
                |r| {
                    let languages: Option<String> = r.get(4)?;
                    Ok(SweepCandidate {
                        id: r.get(0)?,
                        duration_s: r.get::<_, i64>(1)? as f32 / 1e9,
                        audio_path: r.get(2)?,
                        text: r.get(3)?,
                        declared: crate::lang::parse_languages(languages.as_deref()),
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How much archive the sweep still owes at a given floor, and how much it
    /// has already been over: `(owed, swept)`.
    ///
    /// The first half is [`Self::segments_for_lang_sweep`]'s own filter, term
    /// for term, so a status line can never claim a backlog the walk would not
    /// actually visit — and, since 0.12.1, so that "0 owed" is reachable at
    /// all. Up to 0.12.0 this counted every row the pass had *declined for
    /// free*, which on the live install was 1,729 of 1,782: the same number was
    /// printed after every run, and there was no way to tell a sweep that had
    /// finished from one that had not started.
    ///
    /// The second half counts visits, not stamps, and deliberately carries no
    /// `lang IS NULL` test: a row the sweep stamped `de` has a language now,
    /// and counting it as unswept would make a finished sweep look idle.
    pub fn lang_sweep_counts(&self, min_duration_s: f32) -> Result<(i64, i64)> {
        let min_ns = (min_duration_s.max(0.0) as f64 * 1e9) as i64;
        Ok(self
            .conn
            .query_row(
                "SELECT
                   SUM(CASE WHEN g.lang IS NULL
                             AND g.sweep_at_ns IS NULL
                             AND (g.lang_via IS NULL OR g.lang_via != ?1)
                             AND NOT EXISTS (
                                 SELECT 1 FROM operations o
                                 WHERE o.op = 'segments.correct'
                                   AND o.target_ids = '[' || g.id || ']')
                            THEN 1 ELSE 0 END),
                   SUM(CASE WHEN g.sweep_at_ns IS NOT NULL THEN 1 ELSE 0 END)
                 FROM segments g
                 WHERE g.deleted_at IS NULL
                   AND g.audio_path <> ''
                   AND g.t_end_ns - g.t_start_ns >= ?2",
                params![lang_via::MISMATCH, min_ns],
                |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .map(|(a, b)| (a.unwrap_or(0), b.unwrap_or(0)))?)
    }

    /// The sweep has been to this row — **whatever** it concluded.
    ///
    /// Written for every outcome, including the free one where the text
    /// pre-filter declined the row without opening its audio, because that is
    /// the outcome the 0.12.0 bookkeeping lost: a row nothing can be done about
    /// today is not a row the sweep owes today.
    ///
    /// It touches `sweep_at_ns` and nothing else. That is the property that
    /// makes marking a declined row safe, and it is the answer to the reason
    /// 0.11.9 refused to mark them at all: no other pass reads this column, so
    /// a mark blocks no future decision about the row, and the mark is cleared
    /// the moment either thing [`crate::asr_cjk::pre_route`] reads actually
    /// moves — see [`Self::clear_segment_sweep`] and
    /// [`Self::clear_speaker_sweep`].
    pub fn mark_segment_swept(&self, segment_id: i64, at_utc_ns: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE segments SET sweep_at_ns = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![segment_id, at_utc_ns],
        )?;
        Ok(())
    }

    /// This row is owed a sweep again: something [`crate::asr_cjk::pre_route`]
    /// reads has changed.
    ///
    /// Called from the two writes that can change it — the transcript
    /// ([`Self::set_segment_text_via`], [`Self::set_segment_analysis`]) and the
    /// row's voice ([`Self::set_segment_speaker_via`]) — rather than from the
    /// sweep, which is the only way a mark can be safe: the pass that
    /// invalidates the answer is the pass that knows it did.
    ///
    /// Cheap enough to call unconditionally. It is one indexed `UPDATE` against
    /// a column almost every row has as NULL already.
    pub fn clear_segment_sweep(&self, segment_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE segments SET sweep_at_ns = NULL WHERE id = ?1",
            params![segment_id],
        )?;
        Ok(())
    }

    /// The same, for every row belonging to one voice, because the voice's
    /// **declared languages** are the other half of what `pre_route` reads.
    ///
    /// Resolved through `speaker_resolved` for [`Self::speaker_languages`]'s
    /// reason: a row still pointing at a merged-away voice is declared whatever
    /// the surviving voice is declared, so it is that voice's declaration
    /// changing that owes it a second look.
    pub fn clear_speaker_sweep(&self, speaker_id: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE segments SET sweep_at_ns = NULL
             WHERE sweep_at_ns IS NOT NULL
               AND speaker_id IN (SELECT id FROM speaker_resolved WHERE canonical_id = ?1)",
            params![speaker_id],
        )?)
    }

    // ---- end the archive sweep -------------------------------------------

    // ---- undoing a route (0.12.0, `crate::unroute`) ----------------------

    /// Every row the audio-language route settled, oldest first, with the words
    /// it wrote over.
    ///
    /// Keyed on `lang_via = 'lid'` and **not** on `text_via`, and the
    /// difference is not academic: on this install eight of the 45 routed rows
    /// carry `text_via = 'arbiter'`, because 0.11.0 wrote the arbiter's value
    /// before `text_via::LID` existed. The language stamp is the claim this
    /// pass is undoing, so the language stamp is what it selects on.
    ///
    /// The prior state comes from the row's **last** `segments.redecode`
    /// operation, because that is the state immediately before the words the
    /// row carries now — which is what "put it back" means. The one way that
    /// could restore the wrong thing is a pass that rewrote the row *after* the
    /// route did, whose operation would then be carrying the route's own CJK
    /// text; that case is excluded by name rather than hoped about, in
    /// `has_prior` below. On this install all 45 routed rows have exactly one
    /// such operation and its `text_via` is `live`.
    pub fn segments_routed_by_lid(&self) -> Result<Vec<RoutedRow>> {
        Ok(self
            .conn
            .prepare(
                "SELECT g.id, g.t_end_ns - g.t_start_ns, g.audio_path, g.text, g.lang,
                        (SELECT s.languages FROM speakers s
                         JOIN speaker_resolved r ON r.canonical_id = s.id
                         WHERE r.id = g.speaker_id),
                        (SELECT o.prior_state FROM operations o
                          WHERE o.op = 'segments.redecode'
                            AND o.target_ids = '[' || g.id || ']'
                          ORDER BY o.at_utc_ns DESC, o.id DESC LIMIT 1)
                 FROM segments g
                 WHERE g.deleted_at IS NULL AND g.lang_via = ?1
                 ORDER BY g.t_start_ns, g.id",
            )?
            .query_map(params![crate::asr_cjk::LANG_VIA_LID], |r| {
                let languages: Option<String> = r.get(5)?;
                let prior: Option<String> = r.get(6)?;
                let parsed: Option<serde_json::Value> =
                    prior.as_deref().and_then(|s| serde_json::from_str(s).ok());
                let field = |key: &str| {
                    parsed
                        .as_ref()
                        .and_then(|v| v.get(key))
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                };
                Ok(RoutedRow {
                    id: r.get(0)?,
                    duration_s: r.get::<_, i64>(1)? as f32 / 1e9,
                    audio_path: r.get(2)?,
                    text: r.get(3)?,
                    lang: r.get(4)?,
                    declared: crate::lang::parse_languages(languages.as_deref()),
                    prior_text: field("text"),
                    prior_model: field("asr_model_id"),
                    prior_via: field("text_via"),
                    // A recorded prior whose own `text_via` is `lid` is the
                    // route's output, not the words it replaced: some later
                    // pass has rewritten this row and the trail no longer
                    // reaches the live transcript. Reported as "nothing to
                    // restore" rather than restored wrongly.
                    has_prior: parsed.is_some() && field("text_via").as_deref() != Some("lid"),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Put a routed row back the way it was, and record what was thrown away.
    ///
    /// Three things move together and they have to, because leaving any one of
    /// them would leave a row making a claim nothing stands behind:
    ///
    /// * the **words**, back to `prior_text` with the model and `text_via` that
    ///   produced them — usually `live`, sometimes `context`;
    /// * the **language**, back to NULL with `lang_via` NULL. Not
    ///   [`lang_via::SWEEP`]: a mark would say "asked, nothing to say", and
    ///   what actually happened is that the answer was withdrawn. A NULL puts
    ///   the row back on the sweep's work list, which is where a row nobody has
    ///   a reading for belongs;
    /// * the **translation and the cross-check flag**, cleared for
    ///   [`Self::set_segment_text_via`]'s reason: both were computed about
    ///   words that are no longer on the row. A `fr` translation of `うん` is
    ///   the exact failure that call documents.
    ///
    /// The `operations` row is `segments.unroute` and carries the state being
    /// discarded — the routed text, its model, its `text_via`, and the language
    /// stamp — so this pass is undoable in its turn. A repair that is not
    /// itself reversible is a rewrite.
    pub fn unroute_segment(&self, row: &RoutedRow, at_utc_ns: i64) -> Result<bool> {
        /// What the row says now, read back inside the same call rather than
        /// trusted from the caller's snapshot: the live pipeline may have moved
        /// the row between the gather and the write.
        type Now = (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let now: Option<Now> = self
            .conn
            .query_row(
                "SELECT text, asr_model_id, text_via, lang FROM segments
                 WHERE id = ?1 AND deleted_at IS NULL AND lang_via = ?2",
                params![row.id, crate::asr_cjk::LANG_VIA_LID],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((text, model, via, lang)) = now else {
            return Ok(false);
        };
        self.conn.execute(
            "UPDATE segments
             SET text = ?2, asr_model_id = ?3, text_via = ?4,
                 lang = NULL, lang_via = NULL,
                 asr_confidence = NULL, confidence_at_ns = NULL
             WHERE id = ?1 AND deleted_at IS NULL",
            params![row.id, row.prior_text, row.prior_model, row.prior_via],
        )?;
        self.clear_segment_translation(row.id)?;
        self.log_operation(
            "segments.unroute",
            &format!("[{}]", row.id),
            &serde_json::json!({
                "segment_id": row.id,
                "text": text,
                "asr_model_id": model,
                "text_via": via,
                "lang": lang,
                "lang_via": crate::asr_cjk::LANG_VIA_LID,
            })
            .to_string(),
            at_utc_ns,
        )?;
        Ok(true)
    }

    // ---- end undoing a route ----------------------------------------------

    /// Rows the arbiter could not settle, oldest first — the backlog
    /// `recalld lang repair` walks (0.7.7).
    ///
    /// Only rows that still have audio: the whole point of a repair is to read
    /// the *sound* again, and a row whose WAV the retention window took is one
    /// nothing can be done about. Ordered oldest-first so a bounded run always
    /// makes progress on the same end of the queue rather than re-visiting
    /// whatever happened to be recent.
    pub fn language_mismatch_backlog(&self, limit: usize) -> Result<Vec<(i64, String)>> {
        Ok(self
            .conn
            .prepare(
                "SELECT id, audio_path FROM segments
                 WHERE lang_via = ?1 AND deleted_at IS NULL AND audio_path != ''
                 ORDER BY t_start_ns ASC, id ASC LIMIT ?2",
            )?
            .query_map(params![lang_via::MISMATCH, limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// How many rows are still marked as an unsettled language disagreement,
    /// with and without audio to settle them from. The second number is the one
    /// a repair can never reduce.
    pub fn language_mismatch_counts(&self) -> Result<(i64, i64)> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), COUNT(*) FILTER (WHERE audio_path != '')
             FROM segments WHERE lang_via = ?1 AND deleted_at IS NULL",
            params![lang_via::MISMATCH],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// Label a segment from a voicebank match (or clear its label). The
    /// provenance follows: a row with a speaker was matched, a row without one
    /// has no provenance to record.
    pub fn set_segment_speaker(
        &self,
        segment_id: i64,
        speaker_id: Option<i64>,
        match_score: Option<f32>,
    ) -> Result<()> {
        let via = speaker_id.map(|_| label_via::MATCH);
        self.set_segment_speaker_via(segment_id, speaker_id, match_score, via)
    }

    /// `set_segment_speaker` for a label that did not come from the voicebank:
    /// the microphone's pin, an inheritance, a person's decision.
    pub fn set_segment_speaker_via(
        &self,
        segment_id: i64,
        speaker_id: Option<i64>,
        match_score: Option<f32>,
        label_via: Option<&str>,
    ) -> Result<()> {
        // …and the archive sweep owes this row again (0.12.1). Its pre-filter
        // reads the speaker's *declared* languages, so a row that changes hands
        // — or loses its label altogether — has had the other half of that
        // decision replaced under it.
        self.conn.execute(
            "UPDATE segments
             SET speaker_id = ?2, match_score = ?3, label_via = ?4, sweep_at_ns = NULL
             WHERE id = ?1",
            params![
                segment_id,
                speaker_id,
                match_score.map(|v| v as f64),
                label_via
            ],
        )?;
        Ok(())
    }

    pub fn store_embedding(&self, segment_id: i64, embedding: &Embedding) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO embeddings (segment_id, vector, embed_model_id) VALUES (?1, ?2, ?3)",
            params![segment_id, embedding.to_blob(), embedding.model_id],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn segment_embedding(&self, segment_id: i64) -> Result<Option<Embedding>> {
        let row: Option<(Vec<u8>, String)> = self
            .conn
            .query_row(
                "SELECT vector, embed_model_id FROM embeddings WHERE segment_id = ?1
                 ORDER BY id DESC LIMIT 1",
                params![segment_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.map(|(blob, id)| Embedding::from_blob(id, &blob))
            .transpose()
    }

    // ---- voicebank -------------------------------------------------------

    /// Every live speaker's prototypes in one embedding space, as
    /// `(speaker_id, vector)`. Tombstoned speakers are excluded, and so is any
    /// prototype from a different model — nothing downstream ever gets the
    /// chance to compare across spaces.
    pub fn prototypes(&self, embed_model_id: &str) -> Result<Vec<(i64, Embedding)>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.speaker_id, p.vector
             FROM speaker_prototypes p
             JOIN speakers s ON s.id = p.speaker_id
             WHERE p.embed_model_id = ?1 AND s.merged_into IS NULL
             ORDER BY p.speaker_id, p.id",
        )?;
        let rows = stmt
            .query_map(params![embed_model_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, blob)| Ok((id, Embedding::from_blob(embed_model_id, &blob)?)))
            .collect()
    }

    pub(crate) fn speaker_prototypes(
        &self,
        speaker_id: i64,
        embed_model_id: &str,
    ) -> Result<Vec<(i64, Embedding, bool)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, vector, is_golden FROM speaker_prototypes
             WHERE speaker_id = ?1 AND embed_model_id = ?2 ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![speaker_id, embed_model_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, i64>(2)? != 0,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(id, blob, golden)| {
                Ok((id, Embedding::from_blob(embed_model_id, &blob)?, golden))
            })
            .collect()
    }

    /// Create a voice that already has a name — the CLI's and the tests' path.
    /// The generated label is the same string, so nothing claims the user named
    /// it when they did not.
    pub fn create_speaker(&self, display_name: &str, created_at: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO speakers (display_name, auto_label, created_at) VALUES (?1, ?1, ?2)",
            params![display_name, created_at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// `Speaker_01`, `Speaker_02`, ... The number is a display convenience; the
    /// row id is the stable identity, which is what makes `name` retroactive.
    ///
    /// The number IS the row id. It used to be `COUNT(*) + 1`, which collides
    /// as soon as any voice is hard-deleted (prune, nuke): after removing two
    /// of five voices the next two mints would both read from a shrunken count
    /// and one of them re-issues a label that already exists (audit finding
    /// #6). The v3 migration always numbered by id; now the mint agrees.
    pub fn mint_speaker(&self, created_at: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO speakers (display_name, auto_label, created_at) VALUES ('', '', ?1)",
            params![created_at],
        )?;
        let id = self.conn.last_insert_rowid();
        let label = format!("Speaker_{id:02}");
        self.conn.execute(
            "UPDATE speakers SET display_name = ?1, auto_label = ?1 WHERE id = ?2",
            params![label, id],
        )?;
        Ok(id)
    }

    /// Add a prototype, evicting the most redundant one if the speaker is full.
    /// Returns the new prototype's id.
    pub fn add_prototype(
        &self,
        speaker_id: i64,
        embedding: &Embedding,
        source_segment_id: Option<i64>,
        is_golden: bool,
        cap: usize,
        created_at: i64,
    ) -> Result<Option<i64>> {
        let existing = self.speaker_prototypes(speaker_id, &embedding.model_id)?;
        if !is_golden && cap > 0 && existing.len() >= cap {
            match crate::identity::prototype_to_evict(embedding, &existing)? {
                Some(victim) => {
                    self.conn.execute(
                        "DELETE FROM speaker_prototypes WHERE id = ?1",
                        params![victim],
                    )?;
                }
                // Every slot is golden: hand-enrolled audio outranks anything
                // the daemon inferred, so the new vector is simply dropped.
                // `None` and not a rowid: 0 is a plausible id, and a caller
                // that reads it as one reports an enrolment that never
                // happened (audit finding #25).
                None => return Ok(None),
            }
        }
        self.conn.execute(
            "INSERT INTO speaker_prototypes
                 (speaker_id, vector, embed_model_id, source_segment_id, is_golden, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                speaker_id,
                embedding.to_blob(),
                embedding.model_id,
                source_segment_id,
                is_golden as i64,
                created_at
            ],
        )?;
        Ok(Some(self.conn.last_insert_rowid()))
    }

    pub fn prototype_count(&self, speaker_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM speaker_prototypes WHERE speaker_id = ?1",
            params![speaker_id],
            |r| r.get(0),
        )?)
    }

    /// Live voices holding **more** prototypes than the cap, largest first.
    ///
    /// `add_prototype` enforces `[identity].max_prototypes` on every write, so
    /// this can only be non-empty because something else put them there:
    /// `merge_speakers` re-points a collapsed voice's prototypes with a bare
    /// `UPDATE`, and nothing re-applies the cap afterwards. On the 2026-09-04
    /// archive that left one voice with 47 prototypes against a cap of 20, 24
    /// of them inherited from twenty minted voices that were merged in — and
    /// those 24 match that voice's own ground-truth turns at 0.167 where the
    /// enrolled ones manage 0.359 (FINDINGS §44).
    ///
    /// It reports and does not repair, deliberately. Every automatic trim was
    /// measured against the same held-out protocol: dropping the most
    /// *redundant* prototype — the shipping eviction rule with no incoming
    /// vector — is catastrophic (held-out precision 92.6% → 42.7%), because
    /// redundancy pruning keeps exactly the outliers that do not belong;
    /// dropping the most *outlying* one is safe and immaterial (+0.001 F-0.5).
    /// So the operator is told, and `identity repair --prototypes` remains the
    /// only thing that deletes.
    ///
    /// Tombstones are excluded: a merged-away id holds nothing.
    pub fn oversized_banks(&self, cap: usize) -> Result<Vec<(i64, String, i64)>> {
        if cap == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.display_name, COUNT(p.id) AS n
               FROM speakers s
               JOIN speaker_prototypes p ON p.speaker_id = s.id
              WHERE s.merged_into IS NULL
              GROUP BY s.id
             HAVING n > ?1
              ORDER BY n DESC, s.id ASC",
        )?;
        let rows = stmt
            .query_map(params![cap as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn add_golden_sample(
        &self,
        speaker_id: i64,
        audio_path: &str,
        duration_s: f32,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO golden_samples (speaker_id, audio_path, duration_s) VALUES (?1, ?2, ?3)",
            params![speaker_id, audio_path, duration_s as f64],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// This voice's kept clips, longest first. Resolved through the tombstone
    /// view, so a merged-away id still finds the surviving voice's goldens.
    pub fn golden_samples_for(&self, speaker_id: i64) -> Result<Vec<GoldenRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.audio_path, g.duration_s
             FROM golden_samples g
             LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
             WHERE sp.canonical_id = ?1
             ORDER BY g.duration_s DESC, g.id ASC",
        )?;
        let rows = stmt
            .query_map(params![speaker_id], |r| {
                Ok(GoldenRow {
                    id: r.get(0)?,
                    audio_path: r.get(1)?,
                    duration_s: r.get::<_, f64>(2)? as f32,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Forget one golden, returning its path so the caller can unlink the file.
    /// The row goes first: a file with no row is residue the reconciliation
    /// sweep understands, a row with no file is a lie.
    pub fn delete_golden_sample(&self, id: i64) -> Result<Option<String>> {
        let path: Option<String> = self
            .conn
            .query_row(
                "SELECT audio_path FROM golden_samples WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        if path.is_some() {
            self.conn
                .execute("DELETE FROM golden_samples WHERE id = ?1", params![id])?;
        }
        Ok(path)
    }

    // ---- settings (v4) ---------------------------------------------------

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// The pinned "You" speaker, if there is one. Never creates.
    ///
    /// Two self-healing steps, both about never growing a second You:
    ///
    /// * A merge moves the pin. If the user merges You into another voice, the
    ///   pinned row is a tombstone and every segment, prototype and golden it
    ///   owned now lives at the target — so the pin follows the merge and the
    ///   next mic segment lands on the surviving id. A tombstone is not a
    ///   reason to mint a replacement.
    /// * If the settings row is missing but a voice already carries the `You`
    ///   generated label, that voice is adopted rather than duplicated.
    pub fn you_speaker_id(&self) -> Result<Option<i64>> {
        if let Some(raw) = self.setting(YOU_SPEAKER_KEY)?
            && let Ok(id) = raw.trim().parse::<i64>()
        {
            // `resolve_speaker` errors only when the row is gone entirely (a
            // hard delete), which is the one case where re-adopting is right.
            if let Ok(canonical) = self.resolve_speaker(id) {
                if canonical != id {
                    self.set_setting(YOU_SPEAKER_KEY, &canonical.to_string())?;
                }
                return Ok(Some(canonical));
            }
        }
        let adopted: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM speakers
                 WHERE auto_label = ?1 AND merged_into IS NULL
                 ORDER BY id ASC LIMIT 1",
                params![YOU_AUTO_LABEL],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = adopted {
            self.set_setting(YOU_SPEAKER_KEY, &id.to_string())?;
        }
        Ok(adopted)
    }

    /// The pinned "You" speaker, minting it on first sight of the user's own
    /// voice. Idempotent: called on every mic segment, creates at most once.
    pub fn ensure_you_speaker(&self, created_at: i64) -> Result<i64> {
        if let Some(id) = self.you_speaker_id()? {
            return Ok(id);
        }
        // `create_speaker` sets auto_label = display_name and leaves `named_at`
        // NULL, so this reads as an unnamed voice the daemon labelled — which
        // is exactly what it is until the user calls themselves something else.
        let id = self.create_speaker(YOU_AUTO_LABEL, created_at)?;
        self.set_setting(YOU_SPEAKER_KEY, &id.to_string())?;
        Ok(id)
    }

    /// Name a voice. `named_at` is what tells a client this is a person the
    /// user has identified rather than a number the daemon made up.
    pub fn rename_speaker(
        &self,
        speaker_id: i64,
        display_name: &str,
        at_utc_ns: i64,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE speakers SET display_name = ?2, named_at = ?3 WHERE id = ?1",
            params![speaker_id, display_name, at_utc_ns],
        )?;
        if n == 0 {
            bail!("no speaker with id {speaker_id}");
        }
        Ok(())
    }

    /// Follow a tombstone to the canonical speaker. One hop by construction.
    pub fn resolve_speaker(&self, speaker_id: i64) -> Result<i64> {
        let merged: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT merged_into FROM speakers WHERE id = ?1",
                params![speaker_id],
                |r| r.get(0),
            )
            .optional()?;
        match merged {
            None => bail!("no speaker with id {speaker_id}"),
            Some(None) => Ok(speaker_id),
            Some(Some(target)) => Ok(target),
        }
    }

    /// Merge `from` into `into`: tombstone `from`, move everything that pointed
    /// at it, and re-point any tombstone that already targeted it so merge
    /// chains never form.
    pub fn merge_speakers(&self, from: i64, into: i64) -> Result<MergeReport> {
        if from == into {
            bail!("cannot merge speaker {from} into itself");
        }
        // Merging onto a tombstone would create the chain we are avoiding.
        let into = self.resolve_speaker(into)?;
        if from == into {
            bail!("speaker {from} is already merged into that speaker");
        }
        if self.resolve_speaker(from)? != from {
            bail!("speaker {from} is already merged into another speaker");
        }

        let tx = self.conn.unchecked_transaction()?;
        // `sweep_at_ns` is cleared with the re-point, in the same statement:
        // these rows are declared whatever the surviving voice is declared, and
        // the archive sweep's pre-filter reads that (0.12.1).
        let segments = tx.execute(
            "UPDATE segments SET speaker_id = ?2, sweep_at_ns = NULL WHERE speaker_id = ?1",
            params![from, into],
        )?;
        let prototypes = tx.execute(
            "UPDATE speaker_prototypes SET speaker_id = ?2 WHERE speaker_id = ?1",
            params![from, into],
        )?;
        let golden_samples = tx.execute(
            "UPDATE golden_samples SET speaker_id = ?2 WHERE speaker_id = ?1",
            params![from, into],
        )?;
        let tombstones_repointed = tx.execute(
            "UPDATE speakers SET merged_into = ?2 WHERE merged_into = ?1",
            params![from, into],
        )?;
        tx.execute(
            "UPDATE speakers SET merged_into = ?2 WHERE id = ?1",
            params![from, into],
        )?;
        tx.commit()?;

        Ok(MergeReport {
            from,
            into,
            segments,
            prototypes,
            golden_samples,
            tombstones_repointed,
        })
    }

    // ---- split -----------------------------------------------------------

    /// Which embedding space this speaker mostly lives in.
    ///
    /// A voice can carry vectors from more than one model across a migration,
    /// and comparing them is forbidden (DESIGN §5), so a split works in one
    /// space: the one holding the most evidence. Ties break on the model id, so
    /// the answer never depends on row order.
    pub fn speaker_embed_model(&self, speaker_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT embed_model_id, COUNT(*) AS n FROM (
                     SELECT p.embed_model_id
                     FROM speaker_prototypes p
                     WHERE p.speaker_id = ?1
                     UNION ALL
                     SELECT e.embed_model_id
                     FROM embeddings e
                     JOIN segments g ON g.id = e.segment_id
                     LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
                     WHERE sp.canonical_id = ?1 AND g.deleted_at IS NULL
                 )
                 GROUP BY embed_model_id
                 ORDER BY n DESC, embed_model_id ASC
                 LIMIT 1",
                params![speaker_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    /// Everything this speaker's identity rests on, in one embedding space:
    /// its prototypes, and the embeddings of the segments assigned to it.
    ///
    /// A prototype that still remembers its `source_segment_id` is returned as
    /// **one** vector carrying both ids — the pairing DESIGN §5 says makes a
    /// split possible at all. Without it the same audio would vote twice and
    /// then be moved half-way.
    pub fn speaker_vectors(
        &self,
        speaker_id: i64,
        embed_model_id: &str,
    ) -> Result<Vec<crate::split::Vector>> {
        let mut out: Vec<crate::split::Vector> = Vec::new();
        let mut from_prototypes: HashMap<i64, usize> = HashMap::new();

        // A prototype whose source segment has been deleted is evidence the
        // user threw away: it must not come back through a re-cluster, and the
        // segment id it carries must never reach a client as a live row
        // (audit finding #10). Prototypes with no source segment — a hand
        // enrolment, or one whose segment predates the column — are kept: they
        // are not evidence *of* anything that was deleted.
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.vector, p.is_golden, p.source_segment_id
             FROM speaker_prototypes p
             LEFT JOIN segments g ON g.id = p.source_segment_id
             WHERE p.speaker_id = ?1 AND p.embed_model_id = ?2
               AND (p.source_segment_id IS NULL OR (g.id IS NOT NULL AND g.deleted_at IS NULL))
             ORDER BY p.id",
        )?;
        let rows = stmt
            .query_map(params![speaker_id, embed_model_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, i64>(2)? != 0,
                    r.get::<_, Option<i64>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, blob, is_golden, source_segment_id) in rows {
            if let Some(seg) = source_segment_id {
                from_prototypes.insert(seg, out.len());
            }
            out.push(crate::split::Vector {
                prototype_id: Some(id),
                is_golden,
                segment_id: source_segment_id,
                embedding: Embedding::from_blob(embed_model_id, &blob)?,
            });
        }

        // The newest embedding per segment, matching what `segment_embedding`
        // reads, so the clusterer and the matcher never disagree about which
        // vector a segment *is*.
        let mut stmt = self.conn.prepare(
            "SELECT g.id, e.vector
             FROM segments g
             LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
             JOIN embeddings e ON e.id = (
                 SELECT MAX(x.id) FROM embeddings x
                 WHERE x.segment_id = g.id AND x.embed_model_id = ?2
             )
             WHERE sp.canonical_id = ?1 AND g.deleted_at IS NULL
             ORDER BY g.id",
        )?;
        let rows = stmt
            .query_map(params![speaker_id, embed_model_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (segment_id, blob) in rows {
            // Already present as the prototype it seeded: one piece of
            // evidence, not two.
            if from_prototypes.contains_key(&segment_id) {
                continue;
            }
            out.push(crate::split::Vector {
                prototype_id: None,
                is_golden: false,
                segment_id: Some(segment_id),
                embedding: Embedding::from_blob(embed_model_id, &blob)?,
            });
        }
        Ok(out)
    }

    /// The speaker and score each of these segments carries right now — what an
    /// undo of a split has to put back.
    pub fn segment_labels(&self, ids: &[i64]) -> Result<Vec<SegmentLabel>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, speaker_id, match_score FROM segments WHERE id = ?1")?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(row) = stmt
                .query_row(params![id], |r| {
                    Ok(SegmentLabel {
                        segment_id: r.get(0)?,
                        speaker_id: r.get(1)?,
                        match_score: r.get::<_, Option<f64>>(2)?.map(|v| v as f32),
                    })
                })
                .optional()?
            {
                out.push(row);
            }
        }
        Ok(out)
    }

    /// Apply an accepted split: mint a voice for the second cluster and move
    /// its rows onto it, in one transaction.
    ///
    /// The existing id is the one that survives, so every name, link and
    /// tombstone pointing at this speaker keeps meaning what it meant. Only the
    /// rows named in `write` are touched: the confident majority keeps the
    /// scores it already had rather than being restamped by a different
    /// algorithm.
    pub fn split_speaker(
        &self,
        from: i64,
        write: &SplitWrite,
        at_utc_ns: i64,
    ) -> Result<SplitReport> {
        if self.resolve_speaker(from)? != from {
            bail!("speaker {from} is a tombstone; split the speaker it was merged into");
        }
        if write.prototypes.is_empty() && write.segments.is_empty() {
            bail!("a split that moves nothing is not a split");
        }

        let tx = self.conn.unchecked_transaction()?;
        let minted = self.mint_speaker(at_utc_ns)?;
        let mut prototypes = 0usize;
        let mut segments = 0usize;
        let mut ambiguous = 0usize;
        {
            let mut move_prototype = tx.prepare(
                "UPDATE speaker_prototypes SET speaker_id = ?2
                 WHERE id = ?1 AND speaker_id = ?3",
            )?;
            for id in &write.prototypes {
                prototypes += move_prototype.execute(params![id, minted, from])?;
            }
            // `AND deleted_at IS NULL` on both segment writes: a soft-deleted
            // row is invisible to every read path, so moving it would be a
            // write nobody can see — and the event the caller publishes about
            // it would resurrect deleted words in every client (finding #10).
            // A moved row carries a score again, so its provenance is a match
            // whatever it was before — including an inherited label, which the
            // re-cluster has just replaced with a measured one.
            let mut move_segment = tx.prepare(
                "UPDATE segments SET speaker_id = ?2, match_score = ?3, label_via = 'match'
                 WHERE id = ?1 AND speaker_id = ?4 AND deleted_at IS NULL",
            )?;
            for (id, score) in &write.segments {
                segments += move_segment.execute(params![id, minted, *score as f64, from])?;
            }
            // The undecidable ones keep their speaker and lose their
            // confidence: the correction UI reads `match_score` to know which
            // labels to distrust (DESIGN §6).
            let mut soften = tx.prepare(
                "UPDATE segments SET match_score = ?2
                 WHERE id = ?1 AND speaker_id = ?3 AND deleted_at IS NULL",
            )?;
            for (id, score) in &write.ambiguous {
                ambiguous += soften.execute(params![id, *score as f64, from])?;
            }
        }
        tx.commit()?;

        let auto_label: String = self.conn.query_row(
            "SELECT COALESCE(auto_label, display_name) FROM speakers WHERE id = ?1",
            params![minted],
            |r| r.get(0),
        )?;
        Ok(SplitReport {
            kept: from,
            minted,
            auto_label,
            prototypes,
            segments,
            ambiguous,
        })
    }

    pub fn list_speakers(&self) -> Result<Vec<SpeakerSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.display_name, COALESCE(s.auto_label, s.display_name),
                    s.named_at, s.created_at,
                    COUNT(g.id),
                    COALESCE(SUM(g.t_end_ns - g.t_start_ns), 0),
                    s.languages,
                    -- Appended, not inserted: the ORDER BY below is POSITIONAL
                    -- and 7 is the speech total. A column added in the middle
                    -- would silently re-sort the speakers page.
                    s.colour, s.icon
             FROM speakers s
             LEFT JOIN segments g
                 ON g.speaker_id = s.id AND g.deleted_at IS NULL
             WHERE s.merged_into IS NULL
             GROUP BY s.id
             ORDER BY 7 DESC, s.id ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SpeakerSummary {
                    id: r.get(0)?,
                    display_name: r.get(1)?,
                    auto_label: r.get(2)?,
                    named_at: r.get(3)?,
                    created_at: r.get(4)?,
                    segments: r.get(5)?,
                    speech_ns: r.get(6)?,
                    languages: crate::lang::parse_languages(
                        r.get::<_, Option<String>>(7)?.as_deref(),
                    ),
                    colour: r.get(8)?,
                    icon: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- per-speaker languages (v5) --------------------------------------

    /// Which languages a voice speaks. `None` is *any* — the default, and the
    /// only answer until somebody says otherwise. Resolved through the
    /// tombstone view so a merged-away id answers for the surviving voice.
    pub fn speaker_languages(&self, speaker_id: i64) -> Result<Option<Vec<String>>> {
        let raw: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT s.languages FROM speakers s
                 JOIN speaker_resolved sp ON sp.canonical_id = s.id
                 WHERE sp.id = ?1",
                params![speaker_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(crate::lang::parse_languages(raw.flatten().as_deref()))
    }

    /// Declare (or clear) a voice's languages. `None` means any.
    pub fn set_speaker_languages(
        &self,
        speaker_id: i64,
        languages: Option<&[String]>,
    ) -> Result<()> {
        let encoded = match languages {
            None => None,
            Some([]) => None,
            Some(list) => Some(serde_json::to_string(list)?),
        };
        let n = self.conn.execute(
            "UPDATE speakers SET languages = ?2 WHERE id = ?1",
            params![speaker_id, encoded],
        )?;
        if n == 0 {
            bail!("no speaker with id {speaker_id}");
        }
        // Every turn this voice has ever spoken is owed a second look by the
        // archive sweep (0.12.1): its pre-filter hands a row pinned to one
        // language to `correct_language` and declines it, and that is now a
        // different answer. This is the retroactive half of `recalld languages`
        // that `lang repair` has always had and the sweep did not.
        self.clear_speaker_sweep(speaker_id)?;
        Ok(())
    }

    /// One voice, in the shape `speakers.list` returns.
    pub fn speaker_summary(&self, speaker_id: i64) -> Result<Option<SpeakerSummary>> {
        Ok(self
            .list_speakers()?
            .into_iter()
            .find(|s| s.id == speaker_id))
    }

    pub fn find_speaker_by_name(&self, name: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM speakers WHERE display_name = ?1 AND merged_into IS NULL",
                params![name],
                |r| r.get(0),
            )
            .optional()?)
    }

    // ---- reading ---------------------------------------------------------

    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        self.search_filtered(query, &SegmentFilter::default(), limit)
    }

    pub fn transcript(
        &self,
        session_id: Option<i64>,
        speaker_id: Option<i64>,
    ) -> Result<Vec<TranscriptRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.session_id, g.t_start_ns, g.t_end_ns,
                    sp.display_name, g.overlap_frac, g.text
             FROM segments g
             LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
             WHERE g.deleted_at IS NULL
               AND (?1 IS NULL OR g.session_id = ?1)
               AND (?2 IS NULL OR sp.canonical_id = ?2)
             ORDER BY g.t_start_ns ASC, g.id ASC",
        )?;
        let rows = stmt
            .query_map(params![session_id, speaker_id], |r| {
                Ok(TranscriptRow {
                    segment_id: r.get(0)?,
                    session_id: r.get(1)?,
                    t_start_ns: r.get(2)?,
                    t_end_ns: r.get(3)?,
                    speaker: r.get(4)?,
                    overlap_frac: r.get::<_, Option<f64>>(5)?.map(|v| v as f32),
                    text: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Segments with no transcript yet, oldest first — the backlog an offline
    /// re-analysis pass would work through.
    pub fn unanalysed_segments(&self, limit: usize) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, audio_path FROM segments
             WHERE asr_model_id IS NULL AND deleted_at IS NULL
             ORDER BY t_start_ns ASC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The columns every client-facing segment row is built from. Kept in one
    /// place so a transcript page, a search hit and a live event cannot drift
    /// into describing the same segment differently.
    /// How many columns [`Self::SEGMENT_COLUMNS`] selects. A query that appends
    /// its own — the search's snippet — indexes from here rather than from a
    /// number somebody has to remember to bump.
    const SEGMENT_COLUMN_COUNT: usize = 24;

    const SEGMENT_COLUMNS: &'static str =
        "g.id, g.session_id, sc.match_key, g.t_start_ns, g.t_end_ns,
         sp.canonical_id, sp.display_name, g.text, g.overlap_frac, g.match_score, g.audio_path,
         g.lang, g.label_via, g.thread_id, g.lang_via, g.text_via, g.asr_confidence,
         g.night_text, g.translation, g.translation_via, sp.colour, sp.icon,
         g.mood, g.events";

    fn segment_row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<SegmentRow> {
        Ok(SegmentRow {
            id: r.get(0)?,
            session_id: r.get(1)?,
            source: r.get(2)?,
            t_start_ns: r.get(3)?,
            t_end_ns: r.get(4)?,
            speaker_id: r.get(5)?,
            speaker_name: r.get(6)?,
            text: r.get(7)?,
            overlap_frac: r.get::<_, Option<f64>>(8)?.map(|v| v as f32),
            match_score: r.get::<_, Option<f64>>(9)?.map(|v| v as f32),
            audio_path: r.get(10)?,
            lang: r.get(11)?,
            label_via: r.get(12)?,
            thread_id: r.get(13)?,
            lang_via: r.get(14)?,
            text_via: r.get(15)?,
            asr_confidence: r.get(16)?,
            night_text: r.get(17)?,
            // 0.9.0 (v11), the assistant.
            translation: r.get(18)?,
            translation_via: r.get(19)?,
            // 0.12.0 (v15), the highlight. Resolved through the tombstone view
            // like the name beside it, so a merged-away voice wears the
            // surviving one's colour rather than the one it had before somebody
            // decided the two were the same person.
            speaker_colour: r.get(20)?,
            speaker_icon: r.get(21)?,
            // 0.12.4 (v18), how it sounded. NULL on every row until the mood
            // pass has been over it, which is every row on a machine where
            // `[mood].enabled` is false — the shipped default.
            mood: r.get(22)?,
            events: r.get(23)?,
        })
    }

    /// One segment, in the shape clients read.
    /// One live segment in the shape every read path returns.
    ///
    /// Soft-deleted rows are invisible here, like everywhere else. This is the
    /// row the `segment` event is built from, so without the filter a delete
    /// followed by a split or a reassign would broadcast the deleted words
    /// straight back into every client (audit finding #10).
    pub fn segment_row(&self, segment_id: i64) -> Result<Option<SegmentRow>> {
        let sql = format!(
            "SELECT {}
             FROM segments g
             JOIN sessions ss ON ss.id = g.session_id
             JOIN sources sc ON sc.id = ss.source_id
             LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
             WHERE g.id = ?1 AND g.deleted_at IS NULL",
            Self::SEGMENT_COLUMNS
        );
        Ok(self
            .conn
            .query_row(&sql, params![segment_id], Self::segment_row_from)
            .optional()?)
    }

    /// What `segments.audio` needs: the WAV's path (relative to the data dir)
    /// and the segment's span. Soft-deleted rows are invisible here, so a
    /// caller cannot play back something the user has already thrown away.
    ///
    /// `None` means there is no such live segment. `Some` with an empty path
    /// means the row is still there but its audio is not — retention took it
    /// (`retention::forget_audio` blanks the column), which is a different
    /// answer and gets a different error code.
    pub fn segment_audio(&self, segment_id: i64) -> Result<Option<(String, i64, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT audio_path, t_start_ns, t_end_ns FROM segments
                 WHERE id = ?1 AND deleted_at IS NULL",
                params![segment_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?)
    }

    /// Candidate clips for "play me this voice": that speaker's live segments
    /// that still have a WAV, best first.
    ///
    /// Best means *long and confidently matched*, in that order. A two-second
    /// "yeah" identifies nobody however sure the matcher was, so duration
    /// leads; among clips of similar length the one the matcher was surest
    /// about is the one least likely to be somebody else's voice. Bucketing
    /// duration to whole seconds is what keeps a 4.1 s clip from beating a
    /// 4.0 s one that scored far better.
    ///
    /// The speaker is resolved through `speaker_resolved`, so a merged-away id
    /// still finds the surviving voice's clips.
    pub fn speaker_sample_candidates(
        &self,
        speaker_id: i64,
        limit: usize,
    ) -> Result<Vec<SampleRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.t_start_ns, g.t_end_ns, g.text, g.match_score, g.audio_path
             FROM segments g
             LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
             WHERE g.deleted_at IS NULL
               AND g.audio_path <> ''
               AND sp.canonical_id = ?1
             ORDER BY (g.t_end_ns - g.t_start_ns) / 1000000000 DESC,
                      COALESCE(g.match_score, -1) DESC,
                      g.t_start_ns DESC, g.id DESC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![speaker_id, limit as i64], |r| {
                Ok(SampleRow {
                    segment_id: r.get(0)?,
                    t_start_ns: r.get(1)?,
                    t_end_ns: r.get(2)?,
                    text: r.get(3)?,
                    match_score: r.get::<_, Option<f64>>(4)?.map(|v| v as f32),
                    audio_path: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `search`, narrowed. The FTS query drives the match; the rest are `AND`ed
    /// filters, and each hit carries the whole row so a client can render the
    /// conversation around it without a second round trip.
    /// How many live segments match, before any LIMIT — what "total" means.
    pub fn search_count(&self, query: &str, filter: &SegmentFilter) -> Result<i64> {
        Ok(self.conn.query_row(
            &format!(
                "SELECT COUNT(*)
             FROM segments_fts
             JOIN segments g ON g.id = segments_fts.rowid
             JOIN sessions ss ON ss.id = g.session_id
             JOIN sources sc ON sc.id = ss.source_id
             LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
             WHERE segments_fts MATCH ?1 AND g.deleted_at IS NULL
               AND (?2 IS NULL OR sp.canonical_id = ?2)
               AND (?3 IS NULL OR g.session_id = ?3)
               AND (?4 IS NULL OR sc.match_key = ?4)
               AND (?5 IS NULL OR g.t_start_ns >= ?5)
               AND (?6 IS NULL OR g.t_start_ns < ?6)
               {}",
                world_clause(7)
            ),
            params![
                query,
                filter.speaker,
                filter.session,
                filter.source,
                filter.from,
                filter.to,
                filter.worlds_json()
            ],
            |r| r.get(0),
        )?)
    }

    pub fn search_filtered(
        &self,
        query: &str,
        filter: &SegmentFilter,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let sql = format!(
            "SELECT {}, snippet(segments_fts, 0, '[', ']', '…', 12)
             FROM segments_fts
             JOIN segments g ON g.id = segments_fts.rowid
             JOIN sessions ss ON ss.id = g.session_id
             JOIN sources sc ON sc.id = ss.source_id
             LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
             WHERE segments_fts MATCH ?1 AND g.deleted_at IS NULL
               AND (?2 IS NULL OR sp.canonical_id = ?2)
               AND (?3 IS NULL OR g.session_id = ?3)
               AND (?4 IS NULL OR sc.match_key = ?4)
               AND (?5 IS NULL OR g.t_start_ns >= ?5)
               AND (?6 IS NULL OR g.t_start_ns < ?6)
               {}
             ORDER BY g.t_start_ns DESC
             LIMIT ?8",
            Self::SEGMENT_COLUMNS,
            world_clause(7)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![
                    query,
                    filter.speaker,
                    filter.session,
                    filter.source,
                    filter.from,
                    filter.to,
                    filter.worlds_json(),
                    limit as i64
                ],
                |r| {
                    Ok(SearchHit {
                        row: Self::segment_row_from(r)?,
                        // One past SEGMENT_COLUMNS, which is the only place
                        // in this file that knows how wide that list is.
                        snippet: r.get(Self::SEGMENT_COLUMN_COUNT)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// A transcript page: chronological, filtered, capped.
    ///
    /// Which END the LIMIT bites off is the whole contract, and it is decided
    /// by `anchored` below. A `to` alone does not anchor, which makes
    /// `{to, limit}` mean "the newest `limit` rows strictly before T" — the
    /// backwards-paging primitive the GUI's infinite scrollback is built on,
    /// with no extra method. See
    /// `tests::a_to_only_transcript_page_is_the_newest_rows_before_it` for the
    /// expectation table the mock daemon is held to as well.
    pub fn segment_rows(&self, filter: &SegmentFilter, limit: usize) -> Result<Vec<SegmentRow>> {
        // A limited window with no explicit range means "the most recent
        // `limit` rows" — the live transcript. Selecting ASC LIMIT n here
        // returned the OLDEST n forever once the table outgrew the window
        // (audit finding #1: the user's live view opened on their first
        // evening, months of speech ago). Take the newest n, hand them back
        // ascending so callers still render chronologically.
        let anchored = filter.from.is_some() || filter.session.is_some();
        let order = if anchored { "ASC" } else { "DESC" };
        let sql = format!(
            "SELECT {}
             FROM segments g
             JOIN sessions ss ON ss.id = g.session_id
             JOIN sources sc ON sc.id = ss.source_id
             LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
             WHERE g.deleted_at IS NULL
               AND (?1 IS NULL OR sp.canonical_id = ?1)
               AND (?2 IS NULL OR g.session_id = ?2)
               AND (?3 IS NULL OR sc.match_key = ?3)
               AND (?4 IS NULL OR g.t_start_ns >= ?4)
               AND (?5 IS NULL OR g.t_start_ns < ?5)
               {world}
             ORDER BY g.t_start_ns {order}, g.id {order}
             LIMIT ?7",
            Self::SEGMENT_COLUMNS,
            world = world_clause(6)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt
            .query_map(
                params![
                    filter.speaker,
                    filter.session,
                    filter.source,
                    filter.from,
                    filter.to,
                    filter.worlds_json(),
                    limit as i64
                ],
                Self::segment_row_from,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if !anchored {
            rows.reverse();
        }
        Ok(rows)
    }

    /// Live segments, for the status line.
    pub fn segments_total(&self) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM segments WHERE deleted_at IS NULL",
            [],
            |r| r.get(0),
        )?)
    }

    /// Live segments a filter selects, as `(id, audio_path)`. The delete path's
    /// preview and its run read exactly the same set.
    pub fn segments_matching(&self, filter: &SegmentFilter) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT g.id, g.audio_path
             FROM segments g
             JOIN sessions ss ON ss.id = g.session_id
             JOIN sources sc ON sc.id = ss.source_id
             LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
             WHERE g.deleted_at IS NULL
               AND (?1 IS NULL OR sp.canonical_id = ?1)
               AND (?2 IS NULL OR g.session_id = ?2)
               AND (?3 IS NULL OR sc.match_key = ?3)
               AND (?4 IS NULL OR g.t_start_ns >= ?4)
               AND (?5 IS NULL OR g.t_start_ns < ?5)
               {}
             ORDER BY g.t_start_ns ASC, g.id ASC",
            world_clause(6)
        ))?;
        let rows = stmt
            .query_map(
                params![
                    filter.speaker,
                    filter.session,
                    filter.source,
                    filter.from,
                    filter.to,
                    filter.worlds_json()
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Soft delete: the row survives, hidden from every read path, until the
    /// retention sweeper passes the undo window (DESIGN §8).
    pub fn soft_delete_segments(&self, ids: &[i64], at_utc_ns: i64) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE segments SET deleted_at = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            )?;
            for id in ids {
                n += stmt.execute(params![id, at_utc_ns])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Soft-deleted rows whose undo window has closed, as `(id, audio_path)`.
    pub fn expired_soft_deletes(&self, before_utc_ns: i64) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, audio_path FROM segments
             WHERE deleted_at IS NOT NULL AND deleted_at < ?1
             ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![before_utc_ns], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Hard delete, cascading the rows that reference the segment. Deletion
    /// means deletion; the caller unlinks the audio.
    pub fn purge_segments(&self, ids: &[i64]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut drop_embeddings = tx.prepare("DELETE FROM embeddings WHERE segment_id = ?1")?;
            // v9: the text vector goes with the words. A purged turn must stop
            // being findable by meaning as well as by word.
            let mut drop_vectors =
                tx.prepare("DELETE FROM segment_vectors WHERE segment_id = ?1")?;
            let mut orphan_prototypes = tx.prepare(
                "UPDATE speaker_prototypes SET source_segment_id = NULL WHERE source_segment_id = ?1",
            )?;
            // Schema v7. Derived rows are second-class citizens of deletion
            // (GRAPH.md's charter guard): what was inferred from a line cannot
            // outlive the line. Both are re-derivable if the same words ever
            // come back, which is the whole reason this is safe to do.
            let mut drop_time_refs = tx.prepare("DELETE FROM time_refs WHERE segment_id = ?1")?;
            let mut drop_commitments =
                tx.prepare("DELETE FROM commitments WHERE segment_id = ?1")?;
            // v10: a note IS a turn, restated. The turn going means the note
            // going — and the foreign key would refuse the delete anyway.
            let mut drop_notes = tx.prepare("DELETE FROM notes WHERE segment_id = ?1")?;
            let mut drop_segment = tx.prepare("DELETE FROM segments WHERE id = ?1")?;
            for id in ids {
                drop_embeddings.execute(params![id])?;
                drop_vectors.execute(params![id])?;
                orphan_prototypes.execute(params![id])?;
                drop_time_refs.execute(params![id])?;
                drop_commitments.execute(params![id])?;
                drop_notes.execute(params![id])?;
                n += drop_segment.execute(params![id])?;
            }
            // A thread is an index into the transcript and nothing else, so a
            // thread whose last row just went is not an empty conversation —
            // it is not a conversation (DESIGN §0's deletion rule).
            tx.execute(
                "DELETE FROM threads WHERE NOT EXISTS (
                     SELECT 1 FROM segments g WHERE g.thread_id = threads.id
                 )",
                [],
            )?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Live segments whose audio has aged past the audio tier, as
    /// `(id, audio_path)`. The transcript stays; only the WAV goes.
    pub fn audio_older_than(&self, before_utc_ns: i64) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, audio_path FROM segments
             WHERE deleted_at IS NULL AND audio_path <> '' AND t_start_ns < ?1
             ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![before_utc_ns], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Mark a segment's audio as gone while keeping the row. An empty
    /// `audio_path` is the "text only" state.
    pub fn forget_audio(&self, ids: &[i64]) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare("UPDATE segments SET audio_path = '' WHERE id = ?1")?;
            for id in ids {
                n += stmt.execute(params![id])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Every audio path the database still expects to exist, soft-deleted rows
    /// included — the sweeper must not treat an undoable row's file as an
    /// orphan.
    pub fn all_audio_paths(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, audio_path FROM segments WHERE audio_path <> ''")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every golden clip the database still expects to exist, `(row id, path)`.
    ///
    /// The mirror of [`Self::all_audio_paths`], and it exists for the same
    /// reason: goldens live outside `segments/` precisely so the audio tier
    /// never ages them out, but "exempt from retention" was read as "exempt
    /// from reconciliation", and nothing ever compared the directory against
    /// the table in either direction (audit finding #19).
    pub fn all_golden_paths(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, audio_path FROM golden_samples WHERE audio_path <> ''")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    // ---- conversation threads (v6, GRAPH.md Tier 1) ----------------------

    /// The session's threads that a turn starting at `at_ns` could still
    /// continue, with the state [`crate::threads::place`] reads.
    ///
    /// Threading is incremental — one question per stored turn, never a batch
    /// job — so this has to be cheap: one indexed range over `threads`, then
    /// one bounded read of each candidate's recent turns. In practice a session
    /// has one or two open threads at any moment.
    /// `session_id = None` considers open threads from EVERY session — the
    /// microphone's rule, because the user's voice belongs to whatever
    /// conversation is live, not to a session of its own. App turns keep the
    /// per-session scope: two apps emitting at once are two conversations.
    pub fn open_threads(
        &self,
        session_id: Option<i64>,
        at_ns: i64,
        gap_ns: i64,
    ) -> Result<Vec<OpenThread>> {
        let since = at_ns.saturating_sub(gap_ns);
        let ids: Vec<(i64, i64)> = self
            .conn
            .prepare(
                "SELECT id, ended_ns FROM threads
                 WHERE (?1 IS NULL OR session_id = ?1) AND ended_ns >= ?2
                 ORDER BY ended_ns DESC, id DESC",
            )?
            .query_map(params![session_id, since], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;

        let mut out = Vec::with_capacity(ids.len());
        for (id, last_ns) in ids {
            // Turns and distinct voices decide whether the thread has found a
            // rhythm; only labelled turns count, because an anonymous one says
            // nothing about who is in the conversation.
            let turns: i64 = self.conn.query_row(
                "SELECT COUNT(speaker_id) FROM segments
                 WHERE thread_id = ?1 AND deleted_at IS NULL",
                params![id],
                |r| r.get(0),
            )?;
            let voices: BTreeSet<i64> = self
                .conn
                .prepare(
                    "SELECT DISTINCT speaker_id FROM segments
                     WHERE thread_id = ?1 AND speaker_id IS NOT NULL AND deleted_at IS NULL",
                )?
                .query_map(params![id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            // The recent participants, newest first. Read a small window and
            // de-duplicate here: SQLite cannot both DISTINCT and ORDER BY a
            // column it is not selecting, and the window is tiny.
            let mut recent: Vec<i64> = Vec::new();
            let mut seen = BTreeSet::new();
            let window: Vec<i64> = self
                .conn
                .prepare(
                    "SELECT speaker_id FROM segments
                     WHERE thread_id = ?1 AND speaker_id IS NOT NULL AND deleted_at IS NULL
                     ORDER BY t_start_ns DESC, id DESC LIMIT 64",
                )?
                .query_map(params![id], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for speaker in window {
                if seen.insert(speaker) {
                    recent.push(speaker);
                }
                if recent.len() == RECENT_SPEAKERS {
                    break;
                }
            }
            out.push(OpenThread {
                id,
                last_ns,
                recent,
                voices,
                turns: turns as usize,
            });
        }
        Ok(out)
    }

    /// Open a conversation.
    ///
    /// 0.10.0: the conversation is stamped with the world that was open when
    /// its FIRST turn happened, and never revisited. A conversation that ran
    /// across a world change belongs to the world it started in — the
    /// alternative is a thread with two places, which is not a thing a person
    /// can be shown. NULL for anything that is not VRChat: a Discord call and a
    /// microphone-only session happen nowhere.
    pub fn create_thread(&self, session_id: i64, started_ns: i64, ended_ns: i64) -> Result<i64> {
        let world = crate::worlds::visit_at(&self.conn, started_ns)?.map(|v| v.world_id);
        self.conn.execute(
            "INSERT INTO threads (session_id, started_ns, ended_ns, world_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, started_ns, ended_ns, world],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Which world a conversation happened in, if any (0.10.0).
    pub fn thread_world(&self, thread_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT world_id FROM threads WHERE id = ?1",
                params![thread_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Put a turn in a conversation and extend the conversation to cover it.
    pub fn set_segment_thread(&self, segment_id: i64, thread_id: i64, t_end_ns: i64) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        let n = tx.execute(
            "UPDATE segments SET thread_id = ?2 WHERE id = ?1",
            params![segment_id, thread_id],
        )?;
        if n == 0 {
            bail!("no segment with id {segment_id}");
        }
        tx.execute(
            "UPDATE threads SET ended_ns = MAX(ended_ns, ?2) WHERE id = ?1",
            params![thread_id, t_end_ns],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The threading rule's view of one stored segment: its session, whether
    /// that session's turns may bridge into another session's conversation,
    /// and the turn itself.
    ///
    /// The bridge flag exists because a conversation SPANS sessions: the user's
    /// half lives in the mic session while everyone else's lives in an app
    /// session, and threading them apart made every thread single-voiced —
    /// which starved the person graph of edges and the commitment extractor
    /// of counterparties (found live, 2026-09-02: 465 threads, roster of one
    /// in every window, found=0 forever).
    ///
    /// 0.10.0: the room microphone bridges too ([`kind_bridges_threads`]). The
    /// people physically in the room are in the same conversation as the people
    /// in the instance, and the only thing separating them is which device
    /// carried the sound.
    ///
    /// 0.12.1: and every per-user Discord stream, where it stops being a nicety.
    /// One call is one session per person, so without the bridge a four-handed
    /// conversation would thread as four monologues.
    ///
    /// The `IN` list must stay in step with [`kind_bridges_threads`]; the unit
    /// test `every_bridging_kind_is_in_the_sql` is what holds the two together.
    pub fn segment_turn(&self, segment_id: i64) -> Result<Option<(i64, bool, Turn)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT g.session_id, (sc.kind IN (?2, ?3, ?4)) AS bridges,
                        g.t_start_ns, g.t_end_ns, g.speaker_id
                 FROM segments g
                 JOIN sessions ss ON ss.id = g.session_id
                 JOIN sources sc ON sc.id = ss.source_id
                 WHERE g.id = ?1 AND g.deleted_at IS NULL",
                params![segment_id, KIND_MIC, KIND_ROOM, KIND_DISCORD_USER],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        Turn {
                            t_start_ns: r.get(2)?,
                            t_end_ns: r.get(3)?,
                            speaker: r.get(4)?,
                        },
                    ))
                },
            )
            .optional()?)
    }

    // ---- 0.10.0, the Markdown export ------------------------------------

    /// Every live turn in a window, oldest first — what `export.run` writes.
    ///
    /// Deliberately one query returning whole [`SegmentRow`]s rather than the
    /// paged `transcript` shape: the export is the one reader that wants the
    /// *whole* range at once and every column of it (the translation, the
    /// shaky flag, the thread), and paging it would mean deciding what a page
    /// boundary does to a conversation heading.
    ///
    /// `from` is inclusive, `to` exclusive, both in UTC nanoseconds; `None` is
    /// unbounded on that side. `speaker` matches the *canonical* id, so a merged
    /// voice exports under the voice it was merged into, exactly as every other
    /// read path resolves it.
    pub fn export_segments(
        &self,
        from: Option<i64>,
        to: Option<i64>,
        speaker: Option<i64>,
        thread: Option<i64>,
    ) -> Result<Vec<SegmentRow>> {
        let sql = format!(
            "SELECT {}
             FROM segments g
             JOIN sessions ss ON ss.id = g.session_id
             JOIN sources sc ON sc.id = ss.source_id
             LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
             WHERE g.deleted_at IS NULL
               AND (?1 IS NULL OR g.t_start_ns >= ?1)
               AND (?2 IS NULL OR g.t_start_ns < ?2)
               AND (?3 IS NULL OR sp.canonical_id = ?3)
               AND (?4 IS NULL OR g.thread_id = ?4)
             ORDER BY g.t_start_ns ASC, g.id ASC",
            Self::SEGMENT_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![from, to, speaker, thread], Self::segment_row_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// When each voice was last heard at all, as `(canonical id, t_end_ns)`.
    ///
    /// One query rather than a `person_totals` call per voice: `people.md` is
    /// written for every named voice in the bank, and the roster of a long-lived
    /// install is not two rows.
    pub fn export_last_heard(&self) -> Result<BTreeMap<i64, i64>> {
        let rows = self
            .conn
            .prepare(
                "SELECT sp.canonical_id, MAX(g.t_end_ns)
                 FROM segments g
                 JOIN speaker_resolved sp ON sp.id = g.speaker_id
                 WHERE g.deleted_at IS NULL
                 GROUP BY sp.canonical_id",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<BTreeMap<i64, i64>>>()?;
        Ok(rows)
    }

    // ---- end 0.10.0 -------------------------------------------------------

    /// One conversation, in order — what `thread.get` returns.
    pub fn thread_rows(&self, thread_id: i64) -> Result<Vec<SegmentRow>> {
        let sql = format!(
            "SELECT {}
             FROM segments g
             JOIN sessions ss ON ss.id = g.session_id
             JOIN sources sc ON sc.id = ss.source_id
             LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
             WHERE g.thread_id = ?1 AND g.deleted_at IS NULL
             ORDER BY g.t_start_ns ASC, g.id ASC",
            Self::SEGMENT_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![thread_id], Self::segment_row_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// One conversation's shape, without its words.
    pub fn thread_summary(&self, thread_id: i64) -> Result<Option<ThreadSummary>> {
        let base = self
            .conn
            .query_row(
                "SELECT id, session_id, started_ns, ended_ns FROM threads WHERE id = ?1",
                params![thread_id],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, session_id, started_ns, ended_ns)) = base else {
            return Ok(None);
        };
        Ok(Some(ThreadSummary {
            id,
            session_id,
            started_ns,
            ended_ns,
            segments: self.conn.query_row(
                "SELECT COUNT(*) FROM segments WHERE thread_id = ?1 AND deleted_at IS NULL",
                params![id],
                |r| r.get(0),
            )?,
            participants: self.thread_participants(id)?,
            preview: self.thread_preview(id)?,
        }))
    }

    /// Who spoke in a conversation, most talkative first — the order a person
    /// reads a participant list in.
    pub fn thread_participants(&self, thread_id: i64) -> Result<Vec<i64>> {
        let rows = self
            .conn
            .prepare(
                "SELECT sp.canonical_id, SUM(g.t_end_ns - g.t_start_ns) AS spoke
                 FROM segments g
                 JOIN speaker_resolved sp ON sp.id = g.speaker_id
                 WHERE g.thread_id = ?1 AND g.deleted_at IS NULL
                 GROUP BY sp.canonical_id
                 ORDER BY spoke DESC, sp.canonical_id ASC",
            )?
            .query_map(params![thread_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(rows)
    }

    /// The first thing anybody said in a conversation. A preview is a handle,
    /// not a summary — Tier 1 does not summarise, it points.
    fn thread_preview(&self, thread_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT g.text FROM segments g
                 WHERE g.thread_id = ?1 AND g.deleted_at IS NULL
                   AND g.speaker_id IS NOT NULL AND g.text IS NOT NULL AND g.text <> ''
                 ORDER BY g.t_start_ns ASC, g.id ASC LIMIT 1",
                params![thread_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten())
    }

    /// Conversations this voice took part in, newest first.
    pub fn person_threads(&self, speaker_id: i64, limit: usize) -> Result<Vec<ThreadSummary>> {
        let ids: Vec<i64> = self
            .conn
            .prepare(
                "SELECT DISTINCT g.thread_id FROM segments g
                 JOIN speaker_resolved sp ON sp.id = g.speaker_id
                 JOIN threads t ON t.id = g.thread_id
                 WHERE sp.canonical_id = ?1 AND g.deleted_at IS NULL
                 ORDER BY t.started_ns DESC, t.id DESC
                 LIMIT ?2",
            )?
            .query_map(params![speaker_id, limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(s) = self.thread_summary(id)? {
                out.push(s);
            }
        }
        Ok(out)
    }

    /// Everything one voice adds up to.
    pub fn person_totals(&self, speaker_id: i64) -> Result<PersonTotals> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*),
                    COALESCE(SUM(g.t_end_ns - g.t_start_ns), 0),
                    COUNT(DISTINCT g.session_id),
                    COUNT(DISTINCT g.thread_id),
                    MIN(g.t_start_ns),
                    MAX(g.t_end_ns)
             FROM segments g
             JOIN speaker_resolved sp ON sp.id = g.speaker_id
             WHERE sp.canonical_id = ?1 AND g.deleted_at IS NULL",
            params![speaker_id],
            |r| {
                Ok(PersonTotals {
                    segments: r.get(0)?,
                    speech_ns: r.get(1)?,
                    sessions: r.get(2)?,
                    threads: r.get(3)?,
                    first_ns: r.get(4)?,
                    last_ns: r.get(5)?,
                })
            },
        )?)
    }

    /// Who this voice actually talks *with*: the other voices in the
    /// conversations it took part in, ordered by how much of a habit it is.
    ///
    /// Sharing an instance is not a relationship — a VRChat public lobby has
    /// forty people in it and you spoke to two. Sharing a *thread* is, which is
    /// why this is computed from threads and why the roster only ever adds a
    /// column, never a row.
    pub fn person_edges(&self, speaker_id: i64) -> Result<Vec<PersonEdge>> {
        let mut edges: Vec<PersonEdge> = self
            .conn
            .prepare(
                "WITH mine AS (
                     SELECT DISTINCT g.thread_id AS tid
                     FROM segments g
                     JOIN speaker_resolved sp ON sp.id = g.speaker_id
                     WHERE sp.canonical_id = ?1
                       AND g.thread_id IS NOT NULL AND g.deleted_at IS NULL
                 )
                 SELECT other.canonical_id,
                        s.display_name,
                        COALESCE(s.auto_label, s.display_name),
                        s.named_at,
                        COUNT(DISTINCT g.thread_id),
                        COALESCE(SUM(g.t_end_ns - g.t_start_ns), 0),
                        MAX(g.t_end_ns),
                        -- Appended: the ORDER BY below is positional.
                        s.colour, s.icon
                 FROM segments g
                 JOIN mine ON mine.tid = g.thread_id
                 JOIN speaker_resolved other ON other.id = g.speaker_id
                 JOIN speakers s ON s.id = other.canonical_id
                 WHERE g.deleted_at IS NULL AND other.canonical_id <> ?1
                 GROUP BY other.canonical_id
                 ORDER BY 5 DESC, 6 DESC, 1 ASC",
            )?
            .query_map(params![speaker_id], |r| {
                Ok(PersonEdge {
                    speaker_id: r.get(0)?,
                    display_name: r.get(1)?,
                    auto_label: r.get(2)?,
                    named_at: r.get(3)?,
                    threads: r.get(4)?,
                    speech_ns: r.get(5)?,
                    last_ns: r.get(6)?,
                    colour: r.get(7)?,
                    icon: r.get(8)?,
                    roster_ns: None,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // The roster column, for the voices that can be linked to a name the
        // VRChat log wrote down. Most cannot, and `None` says so.
        let mine = self.roster_intervals(speaker_id)?;
        if !mine.is_empty() {
            for edge in &mut edges {
                let theirs = self.roster_intervals(edge.speaker_id)?;
                if theirs.is_empty() {
                    continue;
                }
                edge.roster_ns = Some(overlap_ns(&mine, &theirs));
            }
        }
        Ok(edges)
    }

    /// When a voice's *name* was present in a VRChat instance, per the roster.
    ///
    /// The link is the display name and nothing else: the roster knows names,
    /// the voicebank knows voices, and the only honest bridge between them is
    /// that the user typed the same string. An unnamed voice has no intervals,
    /// which is why `roster_ns` is so often null.
    fn roster_intervals(&self, speaker_id: i64) -> Result<Vec<(i64, i64)>> {
        let name: Option<String> = self
            .conn
            .query_row(
                "SELECT s.display_name FROM speakers s
                 JOIN speaker_resolved sp ON sp.canonical_id = s.id
                 WHERE sp.id = ?1 AND s.named_at IS NOT NULL",
                params![speaker_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(name) = name else {
            return Ok(Vec::new());
        };
        let open_end = crate::clock::utc_now_ns();
        let rows = self
            .conn
            .prepare(
                "SELECT joined_at_utc_ns, COALESCE(left_at_utc_ns, ?2)
                 FROM session_roster WHERE display_name = ?1
                 ORDER BY joined_at_utc_ns ASC",
            )?
            .query_map(params![name, open_end], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Threads with nothing live left in them. A thread is an index into the
    /// transcript, so when the transcript goes the index goes with it — DESIGN
    /// §0's deletion rule, applied to the one derived table the schema has.
    ///
    /// "Live" is the whole point: a soft-deleted row has already left every
    /// read path, so a thread made only of soft-deleted rows is a conversation
    /// no view can reach. It is also the one derived table in the schema and
    /// re-derivable from the transcript, which is what makes deleting it ahead
    /// of the sweeper safe rather than lossy.
    pub fn prune_empty_threads(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "DELETE FROM threads WHERE NOT EXISTS (
                 SELECT 1 FROM segments g
                 WHERE g.thread_id = threads.id AND g.deleted_at IS NULL
             )",
            [],
        )?)
    }

    // ---- the memory graph, Tiers 2 and 3 (v7, GRAPH.md) ------------------
    //
    // Everything here is derived and every read of it joins back to a LIVE
    // segment. That join is not an optimisation — it is how a soft delete works
    // for derived rows: hiding the transcript line hides what was inferred from
    // it, and undoing the delete brings both back. The hard cascades below
    // (`purge_segments`, `delete_speaker`) are the other half.

    /// Replace this segment's time references with `refs`.
    ///
    /// Replace, not append: the extractor is a pure function of the transcript,
    /// so re-running it after a correction must leave exactly what the new text
    /// says rather than the union of two readings.
    pub fn replace_time_refs(
        &self,
        segment_id: i64,
        refs: &[crate::timeref::TimeRef],
        at_utc_ns: i64,
    ) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM time_refs WHERE segment_id = ?1",
            params![segment_id],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO time_refs
                     (segment_id, raw, resolved_utc_ns, kind, extractor, version, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for r in refs {
                stmt.execute(params![
                    segment_id,
                    r.raw,
                    r.resolved_utc_ns,
                    r.kind,
                    crate::timeref::EXTRACTOR,
                    crate::timeref::VERSION,
                    at_utc_ns,
                ])?;
            }
        }
        tx.commit()?;
        Ok(refs.len())
    }

    pub fn time_refs_for(&self, segment_id: i64) -> Result<Vec<TimeRefRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, segment_id, raw, resolved_utc_ns, kind FROM time_refs
             WHERE segment_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map(params![segment_id], |r| {
                Ok(TimeRefRow {
                    id: r.get(0)?,
                    segment_id: r.get(1)?,
                    raw: r.get(2)?,
                    resolved_utc_ns: r.get(3)?,
                    kind: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// File a commitment against a segment, or upgrade the one already there.
    ///
    /// The precedence rule, in one place because it is the whole reason this is
    /// not two inserts:
    ///
    /// - **A person's decision is final.** A row that has left `candidate` is
    ///   never rewritten by either extractor. You confirmed it; a background
    ///   pass does not get to re-open that.
    /// - **The model outranks the rules.** An `llm` row replaces a `rules` one
    ///   in place, keeping the id so nothing a client is looking at jumps.
    /// - **The rules never outrank the model**, and never re-file what the model
    ///   already looked at and described.
    pub fn upsert_commitment(&self, c: &NewCommitment, at_utc_ns: i64) -> Result<CommitmentWrite> {
        let existing: Option<(i64, String, String)> = self
            .conn
            .query_row(
                "SELECT id, state, source FROM commitments WHERE segment_id = ?1",
                params![c.segment_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;

        let Some((id, state, source)) = existing else {
            self.conn.execute(
                "INSERT INTO commitments
                     (segment_id, thread_id, who_speaker_id, to_speaker_id, what,
                      due_utc_ns, due_raw, due_kind, state, source, model_id,
                      confidence, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
                params![
                    c.segment_id,
                    c.thread_id,
                    c.who_speaker_id,
                    c.to_speaker_id,
                    c.what,
                    c.due_utc_ns,
                    c.due_raw,
                    c.due_kind,
                    commitment_state::CANDIDATE,
                    c.source,
                    c.model_id,
                    c.confidence,
                    at_utc_ns,
                ],
            )?;
            return Ok(CommitmentWrite::Inserted(self.conn.last_insert_rowid()));
        };

        let outranks = c.source == commitment_source::LLM && source == commitment_source::RULES;
        if state != commitment_state::CANDIDATE || !outranks {
            return Ok(CommitmentWrite::Kept(id));
        }
        self.conn.execute(
            "UPDATE commitments SET
                 thread_id = ?2, who_speaker_id = ?3, to_speaker_id = ?4, what = ?5,
                 due_utc_ns = ?6, due_raw = ?7, due_kind = ?8, source = ?9,
                 model_id = ?10, confidence = ?11, updated_at = ?12
             WHERE id = ?1",
            params![
                id,
                c.thread_id,
                c.who_speaker_id,
                c.to_speaker_id,
                c.what,
                c.due_utc_ns,
                c.due_raw,
                c.due_kind,
                c.source,
                c.model_id,
                c.confidence,
                at_utc_ns,
            ],
        )?;
        Ok(CommitmentWrite::Upgraded(id))
    }

    /// Drop the rule candidate on a segment the model has now looked at and
    /// found nothing in.
    ///
    /// This is the other half of "the model outranks the rules", and the half
    /// that matters most: the bench's whole finding was that a small model under
    /// a verdict-first grammar refuses cleanly, so letting it *retract* a
    /// pattern match is worth more than letting it add one. A row a person has
    /// already touched is left alone.
    pub fn retract_rule_candidate(&self, segment_id: i64) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM commitments
             WHERE segment_id = ?1 AND source = ?2 AND state = ?3",
            params![
                segment_id,
                commitment_source::RULES,
                commitment_state::CANDIDATE
            ],
        )?;
        Ok(n > 0)
    }

    // The canonical id, never the stored one: a merge tombstones an id without
    // rewriting what pointed at it, and a client that is handed a tombstone
    // cannot open the person page behind it.
    const COMMITMENT_COLUMNS: &'static str = "
        c.id, c.segment_id, c.thread_id,
        COALESCE(who.canonical_id, c.who_speaker_id),
        CASE WHEN whos.named_at IS NULL THEN NULL ELSE who.display_name END, whos.auto_label,
        COALESCE(tow.canonical_id, c.to_speaker_id),
        CASE WHEN tows.named_at IS NULL THEN NULL ELSE tow.display_name END, tows.auto_label,
        c.what, c.due_utc_ns, c.due_raw, c.due_kind,
        c.state, c.source, c.model_id, c.confidence, c.created_at, c.updated_at,
        g.t_start_ns, g.text,
        -- The highlight (v15), through the same tombstone view the names came
        -- through, so a merged voice's promise wears the surviving voice's
        -- colour rather than the one it had before the merge.
        who.colour, who.icon, tow.colour, tow.icon";

    /// The joins [`Self::COMMITMENT_COLUMNS`] reads. The segment join is an
    /// INNER one on purpose: it is what makes a soft-deleted line take its
    /// commitment out of every read path, and put it back on undo.
    const COMMITMENT_JOINS: &'static str = "
        FROM commitments c
        JOIN segments g ON g.id = c.segment_id AND g.deleted_at IS NULL
        LEFT JOIN speaker_resolved who ON who.id = c.who_speaker_id
        LEFT JOIN speakers whos ON whos.id = who.canonical_id
        LEFT JOIN speaker_resolved tow ON tow.id = c.to_speaker_id
        LEFT JOIN speakers tows ON tows.id = tow.canonical_id";

    fn commitment_row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<CommitmentRow> {
        Ok(CommitmentRow {
            id: r.get(0)?,
            segment_id: r.get(1)?,
            thread_id: r.get(2)?,
            who_speaker_id: r.get(3)?,
            who_name: r.get(4)?,
            who_auto: r.get(5)?,
            to_speaker_id: r.get(6)?,
            to_name: r.get(7)?,
            to_auto: r.get(8)?,
            what: r.get(9)?,
            due_utc_ns: r.get(10)?,
            due_raw: r.get(11)?,
            due_kind: r.get(12)?,
            state: r.get(13)?,
            source: r.get(14)?,
            model_id: r.get(15)?,
            confidence: r.get(16)?,
            created_at: r.get(17)?,
            updated_at: r.get(18)?,
            t_start_ns: r.get(19)?,
            said: r.get(20)?,
            who_colour: r.get(21)?,
            who_icon: r.get(22)?,
            to_colour: r.get(23)?,
            to_icon: r.get(24)?,
        })
    }

    /// Commitments in one state, or all of them, soonest-due first.
    ///
    /// Undated rows sort last rather than first: a promise with no date is not
    /// overdue, it is merely open, and putting it above everything with a real
    /// deadline would make the list lie about urgency.
    pub fn commitments(&self, state: Option<&str>, limit: usize) -> Result<Vec<CommitmentRow>> {
        let sql = format!(
            "SELECT {} {}
             WHERE (?1 IS NULL OR c.state = ?1)
             ORDER BY (c.due_utc_ns IS NULL) ASC, c.due_utc_ns ASC, g.t_start_ns ASC
             LIMIT ?2",
            Self::COMMITMENT_COLUMNS,
            Self::COMMITMENT_JOINS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![state, limit as i64], Self::commitment_row_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn commitment(&self, id: i64) -> Result<Option<CommitmentRow>> {
        let sql = format!(
            "SELECT {} {} WHERE c.id = ?1",
            Self::COMMITMENT_COLUMNS,
            Self::COMMITMENT_JOINS
        );
        Ok(self
            .conn
            .query_row(&sql, params![id], Self::commitment_row_from)
            .optional()?)
    }

    /// Move a commitment through its state machine. Returns the row as it now
    /// is, or `None` when there is no such commitment.
    pub fn set_commitment_state(
        &self,
        id: i64,
        state: &str,
        at_utc_ns: i64,
    ) -> Result<Option<CommitmentRow>> {
        let n = self.conn.execute(
            "UPDATE commitments SET state = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, state, at_utc_ns],
        )?;
        if n == 0 {
            return Ok(None);
        }
        self.commitment(id)
    }

    /// Every topic label in use, most recently heard first.
    pub fn topics(&self, per_topic: usize) -> Result<Vec<TopicRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.topic,
                    COUNT(DISTINCT t.id)                                   AS threads,
                    COUNT(g.id)                                            AS segments,
                    MAX(g.t_end_ns)                                        AS last_ns
             FROM threads t
             JOIN segments g ON g.thread_id = t.id AND g.deleted_at IS NULL
             WHERE t.topic IS NOT NULL AND TRIM(t.topic) <> ''
             GROUP BY t.topic
             ORDER BY last_ns DESC, threads DESC",
        )?;
        let heads: Vec<(String, i64, i64, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?;

        let mut out = Vec::with_capacity(heads.len());
        for (topic, threads, segments, last_ns) in heads {
            let thread_ids: Vec<i64> = self
                .conn
                .prepare(
                    "SELECT t.id FROM threads t
                     WHERE t.topic = ?1
                       AND EXISTS (SELECT 1 FROM segments g
                                   WHERE g.thread_id = t.id AND g.deleted_at IS NULL)
                     ORDER BY t.ended_ns DESC LIMIT ?2",
                )?
                .query_map(params![topic, per_topic as i64], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            out.push(TopicRow {
                topic,
                threads,
                segments,
                last_ns,
                thread_ids,
            });
        }
        Ok(out)
    }

    pub fn set_thread_topic(
        &self,
        thread_id: i64,
        topic: Option<&str>,
        model_id: &str,
        at_utc_ns: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE threads SET topic = ?2, topic_model_id = ?3, enriched_at = ?4 WHERE id = ?1",
            params![thread_id, topic, model_id, at_utc_ns],
        )?;
        Ok(())
    }

    /// Mark a conversation as walked by the enrichment pass, whether or not it
    /// produced anything. A pass that found nothing must still not be re-run
    /// for ever.
    pub fn mark_thread_enriched(&self, thread_id: i64, at_utc_ns: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE threads SET enriched_at = ?2 WHERE id = ?1",
            params![thread_id, at_utc_ns],
        )?;
        Ok(())
    }

    /// The enrichment worker's queue: conversations nobody has looked at yet,
    /// newest first, that have enough transcript to be worth a model at all.
    ///
    /// Newest first because the answer to "what did I just promise" is worth
    /// more than the answer to "what did I promise in July", and because a
    /// worker that starts at the beginning of history never reaches today.
    pub fn unenriched_threads(&self, min_segments: i64, limit: usize) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id FROM threads t
             WHERE t.enriched_at IS NULL
               AND (SELECT COUNT(*) FROM segments g
                    WHERE g.thread_id = t.id AND g.deleted_at IS NULL
                      AND g.text IS NOT NULL AND TRIM(g.text) <> '') >= ?1
             ORDER BY t.ended_ns DESC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![min_segments, limit as i64], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// One conversation as the model sees it: the turns that have words, in
    /// order, with the speaker and the capture time each one needs.
    pub fn thread_lines(&self, thread_id: i64) -> Result<Vec<ThreadLine>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, speaker_id, t_start_ns, text FROM segments
             WHERE thread_id = ?1 AND deleted_at IS NULL
               AND text IS NOT NULL AND TRIM(text) <> ''
             ORDER BY t_start_ns ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![thread_id], |r| {
                Ok(ThreadLine {
                    segment_id: r.get(0)?,
                    speaker_id: r.get(1)?,
                    t_start_ns: r.get(2)?,
                    text: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// How much of the graph exists, in one query per number.
    ///
    /// `min_thread_segments` is `[graph].min_thread_segments` — the worker's
    /// own floor, passed in rather than assumed, because the split between
    /// "waiting" and "too short to read" is only meaningful at the number the
    /// worker is actually running under. Handing this the default while the
    /// daemon runs at something else would produce exactly the class of
    /// report 0.12.5 exists to stop: a table describing a machine nobody is
    /// running.
    pub fn graph_counts(&self, min_thread_segments: i64) -> Result<GraphCounts> {
        let one = |sql: &str| -> Result<i64> { Ok(self.conn.query_row(sql, [], |r| r.get(0))?) };
        // Every count is scoped to live segments, for the same reason the reads
        // are: a hidden row must not be counted in a summary the user reads.
        let live = "JOIN segments g ON g.id = c.segment_id AND g.deleted_at IS NULL";
        let by_state = |state: &str| -> Result<i64> {
            Ok(self.conn.query_row(
                &format!("SELECT COUNT(*) FROM commitments c {live} WHERE c.state = ?1"),
                params![state],
                |r| r.get(0),
            )?)
        };
        let by_source = |source: &str| -> Result<i64> {
            Ok(self.conn.query_row(
                &format!("SELECT COUNT(*) FROM commitments c {live} WHERE c.source = ?1"),
                params![source],
                |r| r.get(0),
            )?)
        };
        let candidates = by_state(commitment_state::CANDIDATE)?;
        let confirmed = by_state(commitment_state::CONFIRMED)?;

        // Unread conversations with anything to read at all, and the subset
        // the worker will take. `waiting` is written to be the same predicate
        // as `unenriched_threads`, because the number on the card and the
        // number the worker acts on have to be one number: 0.12.4 had two,
        // and the card spent an evening saying "919 waiting" next to "nothing
        // left to read".
        let with_words = "EXISTS (SELECT 1 FROM segments g
                          WHERE g.thread_id = t.id AND g.deleted_at IS NULL
                            AND g.text IS NOT NULL AND TRIM(g.text) <> '')";
        let pending = one(&format!(
            "SELECT COUNT(*) FROM threads t WHERE t.enriched_at IS NULL AND {with_words}"
        ))?;
        let waiting: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM threads t
             WHERE t.enriched_at IS NULL
               AND (SELECT COUNT(*) FROM segments g
                    WHERE g.thread_id = t.id AND g.deleted_at IS NULL
                      AND g.text IS NOT NULL AND TRIM(g.text) <> '') >= ?1",
            params![min_thread_segments],
            |r| r.get(0),
        )?;

        Ok(GraphCounts {
            time_refs: one("SELECT COUNT(*) FROM time_refs t
                 JOIN segments g ON g.id = t.segment_id AND g.deleted_at IS NULL")?,
            commitments: one(&format!("SELECT COUNT(*) FROM commitments c {live}"))?,
            // "Open" is what the view leads with: still owed, in either sense.
            open: candidates + confirmed,
            candidates,
            confirmed,
            done: by_state(commitment_state::DONE)?,
            dismissed: by_state(commitment_state::DISMISSED)?,
            from_rules: by_source(commitment_source::RULES)?,
            from_llm: by_source(commitment_source::LLM)?,
            topics: one("SELECT COUNT(DISTINCT topic) FROM threads
                 WHERE topic IS NOT NULL AND TRIM(topic) <> ''")?,
            threads: one("SELECT COUNT(*) FROM threads")?,
            threads_enriched: one("SELECT COUNT(*) FROM threads WHERE enriched_at IS NOT NULL")?,
            threads_pending: pending,
            threads_waiting: waiting,
            // Subtracted rather than counted, so the identity
            // `waiting + too_short == pending` holds by construction and
            // cannot drift the way two independently-written WHERE clauses
            // eventually do. It is the arithmetic that makes the card's three
            // numbers add up.
            threads_too_short: (pending - waiting).max(0),
        })
    }

    /// Forget every derived row: what the graph says, but not what was said.
    ///
    /// The escape hatch behind turning Tier 3 off — and the proof that the
    /// graph is an index and never the source of truth. The transcript is
    /// untouched and every one of these rows can be derived again.
    pub fn clear_derived(&self) -> Result<(usize, usize, usize)> {
        let tx = self.conn.unchecked_transaction()?;
        let commitments = tx.execute("DELETE FROM commitments", [])?;
        let time_refs = tx.execute("DELETE FROM time_refs", [])?;
        let topics = tx.execute(
            "UPDATE threads SET topic = NULL, topic_model_id = NULL, enriched_at = NULL",
            [],
        )?;
        tx.commit()?;
        Ok((commitments, time_refs, topics))
    }

    // ---- manual labelling ------------------------------------------------

    /// The fields an undo would have to put back.
    /// What a correction is about to replace. Soft-deleted rows are not
    /// correctable: they are gone from every view, and a caller that could
    /// still reassign one would be editing something the user threw away
    /// (finding #10). `Err` here is what the socket turns into `not_found`.
    pub fn segment_state(&self, segment_id: i64) -> Result<(Option<i64>, Option<String>)> {
        let row = self
            .conn
            .query_row(
                "SELECT speaker_id, text FROM segments WHERE id = ?1 AND deleted_at IS NULL",
                params![segment_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        row.ok_or_else(|| anyhow::anyhow!("no segment with id {segment_id}"))
    }

    /// Hand-assign a speaker. The match score is cleared: it described the
    /// automatic guess, and keeping it would misreport a human decision as a
    /// confident model one.
    pub fn reassign_segment(&self, segment_id: i64, speaker_id: Option<i64>) -> Result<()> {
        let speaker_id = match speaker_id {
            Some(id) => Some(self.resolve_speaker(id)?),
            None => None,
        };
        let n = self.conn.execute(
            "UPDATE segments SET speaker_id = ?2, match_score = NULL, label_via = ?3
             WHERE id = ?1 AND deleted_at IS NULL",
            params![
                segment_id,
                speaker_id,
                speaker_id.map(|_| label_via::MANUAL)
            ],
        )?;
        if n == 0 {
            bail!("no segment with id {segment_id}");
        }
        Ok(())
    }

    /// Hand-correct a transcript. The FTS index follows through the update
    /// trigger, so a corrected segment is immediately searchable by its new
    /// words and no longer by its old ones.
    pub fn correct_segment_text(&self, segment_id: i64, text: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE segments SET text = ?2 WHERE id = ?1 AND deleted_at IS NULL",
            params![segment_id, text],
        )?;
        if n == 0 {
            bail!("no segment with id {segment_id}");
        }
        Ok(())
    }

    pub fn speaker_name(&self, speaker_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT display_name FROM speakers WHERE id = ?1",
                params![speaker_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    // ---- audit trail -----------------------------------------------------

    /// Record an operation. `target_ids` and `prior_state` are JSON, produced
    /// by the caller: the store stays free of a serialisation opinion, and the
    /// undo path (a later step) reads back exactly what was written.
    pub fn log_operation(
        &self,
        op: &str,
        target_ids: &str,
        prior_state: &str,
        at_utc_ns: i64,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO operations (op, target_ids, prior_state, at_utc_ns)
             VALUES (?1, ?2, ?3, ?4)",
            params![op, target_ids, prior_state, at_utc_ns],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Most recent first — the order an undo stack wants.
    pub fn operations(&self, limit: usize) -> Result<Vec<OperationRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, op, target_ids, prior_state, at_utc_ns
             FROM operations ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok(OperationRow {
                    id: r.get(0)?,
                    op: r.get(1)?,
                    target_ids: r.get(2)?,
                    prior_state: r.get(3)?,
                    at_utc_ns: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- roster ----------------------------------------------------------

    /// Record a join, unless that person is already recorded as present in
    /// this instance since that instant — a daemon restart re-reads the log
    /// from the top and must not duplicate the people it already knows about.
    pub fn roster_join(
        &self,
        world_id: Option<&str>,
        instance: Option<&str>,
        display_name: &str,
        joined_at_utc_ns: i64,
    ) -> Result<i64> {
        let existing: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM session_roster
                 WHERE display_name = ?1 AND left_at_utc_ns IS NULL
                   AND joined_at_utc_ns = ?2
                   AND world_id IS ?3 AND instance IS ?4",
                params![display_name, joined_at_utc_ns, world_id, instance],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = existing {
            return Ok(id);
        }
        self.conn.execute(
            "INSERT INTO session_roster
                 (world_id, instance, display_name, joined_at_utc_ns, left_at_utc_ns)
             VALUES (?1, ?2, ?3, ?4, NULL)",
            params![world_id, instance, display_name, joined_at_utc_ns],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Close the newest open row for that name. Returns whether one was found:
    /// a leave for somebody we never saw join is noise, not an error.
    pub fn roster_leave(&self, display_name: &str, left_at_utc_ns: i64) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE session_roster SET left_at_utc_ns = ?2
             WHERE id = (SELECT id FROM session_roster
                         WHERE display_name = ?1 AND left_at_utc_ns IS NULL
                         ORDER BY joined_at_utc_ns DESC, id DESC LIMIT 1)",
            params![display_name, left_at_utc_ns],
        )?;
        Ok(n > 0)
    }

    /// A world change ends everyone's presence in the old instance.
    pub fn roster_close_all(&self, left_at_utc_ns: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE session_roster SET left_at_utc_ns = ?1 WHERE left_at_utc_ns IS NULL",
            params![left_at_utc_ns],
        )?)
    }

    /// Make the table say what `present` says, and report what moved.
    ///
    /// Two moments need this, and both are moments where the table and the log
    /// have been allowed to drift (audit finding #9):
    ///
    /// * **Start-up.** The tailer replays the log into memory to learn who is
    ///   in the instance. Writing only the present half leaves everyone who
    ///   left while the daemon was down with `left_at_utc_ns` NULL forever —
    ///   and `person.edges` reads an open row as *still here* through
    ///   `COALESCE(left_at, now())`, so a single missed leave grows a shared
    ///   time that never stops growing.
    /// * **Resuming from pause.** While paused nothing is written down, but the
    ///   log keeps being consumed into memory, so the difference between the
    ///   table and the world is exactly this diff.
    ///
    /// A join stamp observed during a pause is written here, at resume, with
    /// the time it really happened rather than the time the write became
    /// allowed. That is deliberate: the alternative is a roster that says
    /// somebody arrived when the user un-paused.
    pub fn roster_reconcile(
        &self,
        world_id: Option<&str>,
        instance: Option<&str>,
        present: &[(String, i64)],
        at_utc_ns: i64,
    ) -> Result<RosterReconcile> {
        let tx = self.conn.unchecked_transaction()?;
        let mut out = RosterReconcile::default();
        for row in self.roster_present()? {
            let still_here = present
                .iter()
                .any(|(name, joined)| name == &row.display_name && *joined == row.joined_at_utc_ns);
            if !still_here {
                // `at_utc_ns`, not the join stamp: the only honest thing we can
                // say is that they were gone by the time we looked.
                tx.execute(
                    "UPDATE session_roster SET left_at_utc_ns = ?2 WHERE id = ?1",
                    params![row.id, at_utc_ns],
                )?;
                out.closed += 1;
            }
        }
        tx.commit()?;
        let open_now = self.roster_present()?;
        for (name, joined) in present {
            let kept = open_now
                .iter()
                .any(|r| &r.display_name == name && r.joined_at_utc_ns == *joined);
            if kept {
                continue;
            }
            self.roster_join(world_id, instance, name, *joined)?;
            out.opened += 1;
        }
        Ok(out)
    }

    /// Who is in the instance right now.
    pub fn roster_present(&self) -> Result<Vec<RosterRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, world_id, instance, display_name, joined_at_utc_ns, left_at_utc_ns
             FROM session_roster WHERE left_at_utc_ns IS NULL
             ORDER BY joined_at_utc_ns ASC, id ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(RosterRow {
                    id: r.get(0)?,
                    world_id: r.get(1)?,
                    instance: r.get(2)?,
                    display_name: r.get(3)?,
                    joined_at_utc_ns: r.get(4)?,
                    left_at_utc_ns: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Everyone recorded in a window, present or past.
    pub fn roster_between(&self, from: i64, to: i64) -> Result<Vec<RosterRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, world_id, instance, display_name, joined_at_utc_ns, left_at_utc_ns
             FROM session_roster
             WHERE joined_at_utc_ns < ?2 AND (left_at_utc_ns IS NULL OR left_at_utc_ns > ?1)
             ORDER BY joined_at_utc_ns ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![from, to], |r| {
                Ok(RosterRow {
                    id: r.get(0)?,
                    world_id: r.get(1)?,
                    instance: r.get(2)?,
                    display_name: r.get(3)?,
                    joined_at_utc_ns: r.get(4)?,
                    left_at_utc_ns: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- proximity inheritance (0.6.1) -----------------------------------

    /// The columns proximity inheritance reasons about, for one segment.
    fn neighbour_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<NeighbourSegment> {
        Ok(NeighbourSegment {
            id: r.get(0)?,
            t_start_ns: r.get(1)?,
            t_end_ns: r.get(2)?,
            speaker_id: r.get(3)?,
            match_score: r.get::<_, Option<f64>>(4)?.map(|v| v as f32),
            label_via: r.get(5)?,
        })
    }

    pub fn neighbour_segment(&self, segment_id: i64) -> Result<Option<NeighbourSegment>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, t_start_ns, t_end_ns, speaker_id, match_score, label_via
                 FROM segments WHERE id = ?1 AND deleted_at IS NULL",
                params![segment_id],
                Self::neighbour_from,
            )
            .optional()?)
    }

    /// The live segments immediately before `segment_id` **in the same
    /// session**, nearest first.
    ///
    /// Same session is not a detail: two sources are two microphones on two
    /// different conversations, and "the turn before this one" only means
    /// something inside one of them.
    pub fn segments_before(&self, segment_id: i64, n: usize) -> Result<Vec<NeighbourSegment>> {
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.t_start_ns, g.t_end_ns, g.speaker_id, g.match_score, g.label_via
             FROM segments g
             JOIN segments anchor ON anchor.id = ?1
             WHERE g.session_id = anchor.session_id
               AND g.deleted_at IS NULL
               AND (g.t_start_ns, g.id) < (anchor.t_start_ns, anchor.id)
             ORDER BY g.t_start_ns DESC, g.id DESC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![segment_id, n as i64], Self::neighbour_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    // ---- pruning one-off voices (0.6.1) ----------------------------------

    /// Voices that are almost certainly not people: at most one segment and
    /// under `max_speech_ns` of speech in total.
    ///
    /// Named voices are excluded outright — a name is a person saying "this one
    /// matters", and a sweep must never argue with that — and so is any voice
    /// that is the target of a merge, because a merge target carries somebody
    /// else's history even when its own counts look thin.
    pub fn prune_candidates(
        &self,
        max_segments: i64,
        max_speech_ns: i64,
    ) -> Result<Vec<SpeakerSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.display_name, COALESCE(s.auto_label, s.display_name),
                    s.named_at, s.created_at,
                    COUNT(g.id) AS segments,
                    COALESCE(SUM(g.t_end_ns - g.t_start_ns), 0) AS speech,
                    s.languages,
                    s.colour, s.icon
             FROM speakers s
             LEFT JOIN segments g
                 ON g.speaker_id = s.id AND g.deleted_at IS NULL
             WHERE s.merged_into IS NULL
               AND s.named_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM speakers t WHERE t.merged_into = s.id)
             GROUP BY s.id
             HAVING segments <= ?1 AND speech < ?2
             ORDER BY speech ASC, s.id ASC",
        )?;
        let rows = stmt
            .query_map(params![max_segments, max_speech_ns], |r| {
                Ok(SpeakerSummary {
                    id: r.get(0)?,
                    display_name: r.get(1)?,
                    auto_label: r.get(2)?,
                    named_at: r.get(3)?,
                    created_at: r.get(4)?,
                    segments: r.get(5)?,
                    speech_ns: r.get(6)?,
                    languages: crate::lang::parse_languages(
                        r.get::<_, Option<String>>(7)?.as_deref(),
                    ),
                    colour: r.get(8)?,
                    icon: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every speaker tombstoned onto this one — the ids a merge folded into it.
    ///
    /// A caller that wants to remove the row itself has to know about these:
    /// `speakers.merged_into` is a real foreign key, so deleting a merge target
    /// out from under its tombstones is not merely untidy, it is refused by
    /// SQLite. The count is what the refusal message is built from.
    pub fn merge_tombstones(&self, speaker_id: i64) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM speakers WHERE merged_into = ?1 ORDER BY id")?;
        let rows = stmt
            .query_map(params![speaker_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Remove one voice's conversations, and — unless the voiceprint is kept —
    /// the voice itself, in one transaction.
    ///
    /// This is DESIGN §8's choice made real. Both halves soft-delete the
    /// voice's live segments, so the undo window still applies and the
    /// retention sweeper still finalises them exactly as `delete.run` leaves
    /// them; what differs is whether the identity survives:
    ///
    /// * `keep_voiceprint` — the speaker row, its prototypes, its goldens and
    ///   its embeddings all stay. The words are gone and the voice keeps
    ///   matching future audio, which is what "keep the bank entry (still
    ///   labeled going forward)" means.
    /// * otherwise — the labels are cleared so the foreign key can go, and the
    ///   prototypes, the embeddings behind them, the goldens and the speaker
    ///   row are removed for real. That voice has to re-enrol from scratch.
    ///
    /// The golden files are returned rather than unlinked: the row goes first,
    /// because a file with no row is residue the reconciliation sweep
    /// understands and a row with no file is a lie.
    ///
    /// A voice with **no live segments at all** is a normal input, not an
    /// error. It is in fact the case this exists for: once a voice's rows have
    /// gone, a delete scoped by segment matches nothing, and without this the
    /// ghost keeps its voiceprint and goes on matching new audio for ever.
    pub fn delete_speaker(
        &self,
        speaker_id: i64,
        keep_voiceprint: bool,
        at_utc_ns: i64,
    ) -> Result<SpeakerDeleteReport> {
        if self.resolve_speaker(speaker_id)? != speaker_id {
            bail!("speaker {speaker_id} is a tombstone, not a voice");
        }
        if !keep_voiceprint {
            let tombstones = self.merge_tombstones(speaker_id)?;
            if !tombstones.is_empty() {
                bail!(
                    "speaker {speaker_id} is a merge target: {} other voice(s) were merged into it",
                    tombstones.len()
                );
            }
        }
        let segments: Vec<i64> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM segments WHERE speaker_id = ?1 ORDER BY id")?;
            stmt.query_map(params![speaker_id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let live: Vec<i64> = {
            let mut stmt = self.conn.prepare(
                "SELECT id FROM segments WHERE speaker_id = ?1 AND deleted_at IS NULL ORDER BY id",
            )?;
            stmt.query_map(params![speaker_id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let goldens: Vec<String> = if keep_voiceprint {
            Vec::new()
        } else {
            let mut stmt = self
                .conn
                .prepare("SELECT audio_path FROM golden_samples WHERE speaker_id = ?1")?;
            stmt.query_map(params![speaker_id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE segments SET deleted_at = ?2
             WHERE speaker_id = ?1 AND deleted_at IS NULL",
            params![speaker_id, at_utc_ns],
        )?;
        // Schema v7, and GRAPH.md's charter guard in full: "purging a person
        // purges their nodes, edges, topics-participation, commitments". A
        // promise this voice made, and a promise anybody made *to* them, both
        // stop existing — not merely stop being shown. The `to` half matters:
        // "you owe Kira the shader link" must not survive deleting Kira.
        let commitments = tx.execute(
            "DELETE FROM commitments
             WHERE who_speaker_id = ?1 OR to_speaker_id = ?1
                OR segment_id IN (SELECT id FROM segments WHERE speaker_id = ?1)",
            params![speaker_id],
        )?;
        let time_refs = tx.execute(
            "DELETE FROM time_refs WHERE segment_id IN
                 (SELECT id FROM segments WHERE speaker_id = ?1)",
            params![speaker_id],
        )?;
        let mut prototypes = 0;
        let mut embeddings = 0;
        if !keep_voiceprint {
            // Order matters: the embeddings are found *through* the segments,
            // so they go before the labels that identify them are cleared.
            embeddings = tx.execute(
                "DELETE FROM embeddings WHERE segment_id IN
                     (SELECT id FROM segments WHERE speaker_id = ?1)",
                params![speaker_id],
            )?;
            tx.execute(
                "UPDATE segments SET speaker_id = NULL, match_score = NULL, label_via = NULL
                 WHERE speaker_id = ?1",
                params![speaker_id],
            )?;
            prototypes = tx.execute(
                "DELETE FROM speaker_prototypes WHERE speaker_id = ?1",
                params![speaker_id],
            )?;
            tx.execute(
                "DELETE FROM golden_samples WHERE speaker_id = ?1",
                params![speaker_id],
            )?;
            tx.execute("DELETE FROM speakers WHERE id = ?1", params![speaker_id])?;
        }
        // A thread is an index into the transcript and nothing else, so one
        // with nothing live left in it is not an empty conversation — it is not
        // a conversation. Same rule the hard purge applies (`purge_segments`).
        let threads = tx.execute(
            "DELETE FROM threads WHERE NOT EXISTS (
                 SELECT 1 FROM segments g
                 WHERE g.thread_id = threads.id AND g.deleted_at IS NULL
             )",
            [],
        )?;
        tx.commit()?;

        Ok(SpeakerDeleteReport {
            speaker_id,
            segments,
            soft_deleted: live,
            prototypes,
            embeddings,
            goldens,
            threads,
            commitments,
            time_refs,
            removed_speaker: !keep_voiceprint,
        })
    }

    /// The sweep's per-voice cascade: `delete_speaker` with the voiceprint
    /// going too, because a voice that is not a person has no bank entry worth
    /// keeping.
    pub fn prune_speaker(&self, speaker_id: i64, at_utc_ns: i64) -> Result<SpeakerDeleteReport> {
        self.delete_speaker(speaker_id, false, at_utc_ns)
    }

    /// Live segments that are past the audio window and carry **no memory
    /// value at all**: no transcript and no speaker. See the sweeper.
    pub fn empty_unlabelled_older_than(&self, before_utc_ns: i64) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, audio_path FROM segments
             WHERE deleted_at IS NULL
               AND speaker_id IS NULL
               AND (text IS NULL OR TRIM(text) = '')
               AND t_start_ns < ?1
             ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![before_utc_ns], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Raw column read, used by tests and by the CLI's single-row lookups.
    pub fn segment_fields(&self, segment_id: i64) -> Result<HashMap<String, Option<String>>> {
        let mut out = HashMap::new();
        self.conn.query_row(
            "SELECT text, asr_model_id, overlap_frac, speaker_id, match_score, lang, lang_via,
                    label_via, deleted_at
             FROM segments WHERE id = ?1",
            params![segment_id],
            |r| {
                out.insert("text".into(), r.get::<_, Option<String>>(0)?);
                out.insert("asr_model_id".into(), r.get::<_, Option<String>>(1)?);
                out.insert(
                    "overlap_frac".into(),
                    r.get::<_, Option<f64>>(2)?.map(|v| v.to_string()),
                );
                out.insert(
                    "speaker_id".into(),
                    r.get::<_, Option<i64>>(3)?.map(|v| v.to_string()),
                );
                out.insert(
                    "match_score".into(),
                    r.get::<_, Option<f64>>(4)?.map(|v| v.to_string()),
                );
                out.insert("lang".into(), r.get::<_, Option<String>>(5)?);
                out.insert("lang_via".into(), r.get::<_, Option<String>>(6)?);
                out.insert("label_via".into(), r.get::<_, Option<String>>(7)?);
                // Whether the row is soft-deleted, which is the one fact about
                // a segment that decides whether any read path can see it.
                out.insert(
                    "deleted_at".into(),
                    r.get::<_, Option<i64>>(8)?.map(|v| v.to_string()),
                );
                Ok(())
            },
        )?;
        Ok(out)
    }
}

/// Total time two sets of half-open intervals are both running.
///
/// Both are sorted by start, so one pass with two cursors settles it. Roster
/// rows are few per person and this is called once per edge, so the honest
/// linear thing is also the fast thing.
fn overlap_ns(a: &[(i64, i64)], b: &[(i64, i64)]) -> i64 {
    let (mut i, mut j, mut total) = (0usize, 0usize, 0i64);
    while i < a.len() && j < b.len() {
        let lo = a[i].0.max(b[j].0);
        let hi = a[i].1.min(b[j].1);
        total += (hi - lo).max(0);
        if a[i].1 < b[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    total
}

// ===========================================================================
// 0.8.0 — notes to self, briefs and accuracy (PROTOCOL "the accuracy round")
//
// Everything below this line was added by the 0.8.0 product round and is kept
// together on purpose: it is one table, its queries, and the three read-only
// compositions the new methods need. Nothing above it changed except the
// schema version, the migration call and the purge cascade.
// ===========================================================================

/// `notes.state`. A note is a thing you asked to be reminded of, so the only
/// transitions are the two ways of being finished with it.
pub mod note_state {
    pub const OPEN: &str = "open";
    pub const DONE: &str = "done";
    pub const DISMISSED: &str = "dismissed";
    pub const ALL: [&str; 3] = [OPEN, DONE, DISMISSED];

    /// The canonical spelling of a state named on the wire, or `None`.
    pub fn parse(s: &str) -> Option<&'static str> {
        ALL.into_iter().find(|k| *k == s)
    }
}

/// One note to self, with the moment it was spoken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteRow {
    pub id: i64,
    pub segment_id: i64,
    /// The turn's words with the wake phrase taken off the front.
    pub text: String,
    pub created_utc_ns: i64,
    pub state: String,
    /// The segment's start: when you actually said it, which is what a client
    /// shows and what the transcript can be scrolled to.
    pub t_start_ns: i64,
    // ---- 0.9.0, the assistant (schema v11) --------------------------------
    /// When this note asked to be brought back, resolved by [`crate::timeref`]
    /// against the moment it was said. `None` for a note with no time
    /// reference in it, which is most of them.
    pub due_ns: Option<i64>,
    /// When the scheduler actually announced it. `None` means it has not yet —
    /// and a note fires exactly once, which is what this column is for.
    pub fired_at_ns: Option<i64>,
    // ---- end 0.9.0 --------------------------------------------------------
}

/// One conversation label a person took part in, most recent first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonTopic {
    pub thread_id: i64,
    pub topic: String,
    pub last_ns: i64,
}

impl Store {
    /// The `notes` half of schema v10, written so it is a no-op on a v10 database.
    fn apply_v10_notes(&self) -> Result<()> {
        self.conn.execute_batch(
            // A note is an ANNOTATION referencing a segment, exactly like a
            // time reference or a commitment: the turn stays in the transcript
            // and this row points at it. `segment_id` is UNIQUE because one
            // turn is one note — a re-decode or a correction updates the words
            // in place rather than filing a second copy of the same sentence.
            "CREATE TABLE IF NOT EXISTS notes (
                 id             INTEGER PRIMARY KEY,
                 segment_id     INTEGER NOT NULL UNIQUE REFERENCES segments(id),
                 text           TEXT    NOT NULL,
                 created_utc_ns INTEGER NOT NULL,
                 -- open | done | dismissed. Nothing but a person's click moves
                 -- a note off `open`.
                 state          TEXT    NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_notes_state ON notes(state, created_utc_ns);
             CREATE INDEX IF NOT EXISTS idx_notes_segment ON notes(segment_id);",
        )?;
        Ok(())
    }

    const NOTE_COLUMNS: &'static str =
        "n.id, n.segment_id, n.text, n.created_utc_ns, n.state, g.t_start_ns,
         n.due_ns, n.fired_at_ns
         FROM notes n
         JOIN segments g ON g.id = n.segment_id AND g.deleted_at IS NULL";

    fn note_row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<NoteRow> {
        Ok(NoteRow {
            id: r.get(0)?,
            segment_id: r.get(1)?,
            text: r.get(2)?,
            created_utc_ns: r.get(3)?,
            state: r.get(4)?,
            t_start_ns: r.get(5)?,
            // 0.9.0 (v11).
            due_ns: r.get(6)?,
            fired_at_ns: r.get(7)?,
        })
    }

    /// File a note, or update the one this turn already has.
    ///
    /// Returns `None` when the words have not changed, so a re-decode that
    /// produced the same sentence does not re-announce a note the user has
    /// already seen — and, crucially, does not resurrect one they dismissed.
    ///
    /// `due_ns` (0.9.0) travels with the words because it is *derived* from
    /// them: a re-decode that turns "erinner mich morgen" into "erinner mich
    /// heute" has moved the reminder, and a due date left over from the
    /// sentence before is a reminder for something nobody said. It is written
    /// on the same UPDATE and on nothing else — a snooze
    /// ([`Self::snooze_note`]) is a person's decision and is not undone here.
    pub fn upsert_note(
        &self,
        segment_id: i64,
        text: &str,
        due_ns: Option<i64>,
        at_utc_ns: i64,
    ) -> Result<Option<NoteRow>> {
        let existing: Option<(i64, String)> = self
            .conn
            .query_row(
                "SELECT id, text FROM notes WHERE segment_id = ?1",
                params![segment_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let id = match existing {
            Some((_, was)) if was == text => return Ok(None),
            Some((id, _)) => {
                // The words moved, so the date they implied moves with them —
                // and the row goes back on the scheduler's list, because a
                // reminder that fired for a sentence that has since been
                // re-read has not fired for this one.
                self.conn.execute(
                    "UPDATE notes SET text = ?2, due_ns = ?3, fired_at_ns = NULL
                     WHERE id = ?1",
                    params![id, text, due_ns],
                )?;
                id
            }
            None => {
                self.conn.execute(
                    "INSERT INTO notes (segment_id, text, created_utc_ns, state, due_ns)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![segment_id, text, at_utc_ns, note_state::OPEN, due_ns],
                )?;
                self.conn.last_insert_rowid()
            }
        };
        self.note(id)
    }

    pub fn note(&self, id: i64) -> Result<Option<NoteRow>> {
        let sql = format!("SELECT {} WHERE n.id = ?1", Self::NOTE_COLUMNS);
        Ok(self
            .conn
            .query_row(&sql, params![id], Self::note_row_from)
            .optional()?)
    }

    /// Notes, newest first. `state` narrows; `None` is all of them.
    pub fn notes(&self, state: Option<&str>, limit: usize) -> Result<Vec<NoteRow>> {
        let sql = format!(
            "SELECT {} WHERE (?1 IS NULL OR n.state = ?1)
             ORDER BY g.t_start_ns DESC, n.id DESC LIMIT ?2",
            Self::NOTE_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt
            .query_map(params![state, limit as i64], Self::note_row_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Move a note through its state machine. `None` when there is no such
    /// note.
    pub fn set_note_state(&self, id: i64, state: &str) -> Result<Option<NoteRow>> {
        let n = self.conn.execute(
            "UPDATE notes SET state = ?2 WHERE id = ?1",
            params![id, state],
        )?;
        if n == 0 {
            return Ok(None);
        }
        self.note(id)
    }

    /// Notes whose text names somebody. A substring match, case-insensitive
    /// over ASCII, because a note is a sentence a person wrote about a person
    /// and the useful question is "did I write anything about Aspen".
    pub fn notes_mentioning(&self, name: &str, limit: usize) -> Result<Vec<NoteRow>> {
        if name.trim().is_empty() {
            return Ok(Vec::new());
        }
        // The pattern is escaped so a name containing `%` or `_` is a name.
        let pattern = format!(
            "%{}%",
            name.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let sql = format!(
            "SELECT {} WHERE n.text LIKE ?1 ESCAPE '\\'
             ORDER BY g.t_start_ns DESC, n.id DESC LIMIT ?2",
            Self::NOTE_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt
            .query_map(params![pattern, limit as i64], Self::note_row_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every OPEN commitment (`candidate` or `confirmed`) this person is on
    /// either side of. The caller splits it by direction — the two halves of a
    /// brief are one query, because a promise is one row whichever way it
    /// points.
    pub fn open_commitments_for(
        &self,
        speaker_id: i64,
        limit: usize,
    ) -> Result<Vec<CommitmentRow>> {
        let sql = format!(
            "SELECT {} {}
             WHERE c.state IN (?2, ?3)
               AND (c.who_speaker_id = ?1 OR c.to_speaker_id = ?1)
             ORDER BY (c.due_utc_ns IS NULL) ASC, c.due_utc_ns ASC, g.t_start_ns ASC
             LIMIT ?4",
            Self::COMMITMENT_COLUMNS,
            Self::COMMITMENT_JOINS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt
            .query_map(
                params![
                    speaker_id,
                    commitment_state::CANDIDATE,
                    commitment_state::CONFIRMED,
                    limit as i64
                ],
                Self::commitment_row_from,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// What this person's recent conversations were about: `threads.topic` for
    /// the threads they spoke in, most recent first, each label once.
    ///
    /// Threads with no label are simply absent — the Tier 3 pass that writes
    /// them is off by default, and an empty list is the honest report of that.
    pub fn person_recent_topics(&self, speaker_id: i64, limit: usize) -> Result<Vec<PersonTopic>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.topic, MAX(g.t_end_ns) AS last_ns
             FROM segments g
             JOIN speaker_resolved sp ON sp.id = g.speaker_id
             JOIN threads t ON t.id = g.thread_id
             WHERE sp.canonical_id = ?1 AND g.deleted_at IS NULL
               AND t.topic IS NOT NULL AND TRIM(t.topic) <> ''
             GROUP BY t.id
             ORDER BY last_ns DESC, t.id DESC",
        )?;
        let rows = stmt
            .query_map(params![speaker_id], |r| {
                Ok(PersonTopic {
                    thread_id: r.get(0)?,
                    topic: r.get(1)?,
                    last_ns: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // One row per LABEL, not per thread: three conversations about shaders
        // is one thing this person talks about, not three.
        let mut seen = BTreeSet::new();
        Ok(rows
            .into_iter()
            .filter(|t| seen.insert(t.topic.clone()))
            .take(limit)
            .collect())
    }

    /// The operations log, narrowed to one `op`, oldest first — which is the
    /// order `accuracy.summary` has to read corrections in: a second correction
    /// of the same turn replaced the first one's *result*.
    pub fn operations_of(&self, op: &str, limit: usize) -> Result<Vec<OperationRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, op, target_ids, prior_state, at_utc_ns
             FROM operations WHERE op = ?1
             ORDER BY at_utc_ns ASC, id ASC LIMIT ?2",
        )?;
        Ok(stmt
            .query_map(params![op, limit as i64], |r| {
                Ok(OperationRow {
                    id: r.get(0)?,
                    op: r.get(1)?,
                    target_ids: r.get(2)?,
                    prior_state: r.get(3)?,
                    at_utc_ns: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- 0.9.0: ground truth from Discord --------------------------------
    //
    // Everything in this block is additive: two tables, five columns and the
    // accessors over them. Nothing above the banner reads any of it, and
    // deleting the block would leave a working 0.8.2 store behind.

    fn apply_v11(&self) -> Result<()> {
        self.conn.execute_batch(
            // A speaking row is an OBSERVATION, not an annotation: it hangs
            // off no segment and outlives every one it overlaps. It has to,
            // because it arrives before the segment it will be compared with
            // — the plugin reports the ring the instant it lights up, and the
            // turn is not written down until VAD has heard it end.
            //
            // `t_end_ns` is nullable, which is the honest shape: between a
            // start and its stop the row is genuinely open. `truth_close_open`
            // shuts the ones a crash left behind.
            "CREATE TABLE IF NOT EXISTS truth_speaking (
                 id         INTEGER PRIMARY KEY,
                 user_id    TEXT    NOT NULL,
                 name       TEXT    NOT NULL,
                 channel_id TEXT,
                 t_start_ns INTEGER NOT NULL,
                 t_end_ns   INTEGER
             );
             CREATE INDEX IF NOT EXISTS idx_truth_speaking_span
                 ON truth_speaking(t_start_ns, t_end_ns);
             CREATE INDEX IF NOT EXISTS idx_truth_speaking_open
                 ON truth_speaking(user_id, t_end_ns);

             -- A Discord account. `speaker_id` is the link to a voice and is
             -- NULL until something establishes it; `name` is a NICKNAME and
             -- is never written to `speakers.display_name` by the daemon —
             -- Discord names are per-guild, people change them for jokes, and
             -- a voice's name is a decision a person makes.
             CREATE TABLE IF NOT EXISTS discord_users (
                 user_id       TEXT PRIMARY KEY,
                 name          TEXT NOT NULL,
                 speaker_id    INTEGER REFERENCES speakers(id),
                 via           TEXT,
                 linked_at_ns  INTEGER,
                 first_seen_ns INTEGER NOT NULL,
                 last_seen_ns  INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_discord_users_speaker
                 ON discord_users(speaker_id);",
        )?;
        // The verdict lives on the segment because that is what it is about,
        // and because every question worth asking of it ("how often was the
        // ladder right on clean turns") is a query over segments.
        self.add_column_if_missing("segments", "truth_user_id", "TEXT")?;
        self.add_column_if_missing("segments", "truth_verdict", "TEXT")?;
        self.add_column_if_missing("segments", "truth_coverage", "REAL")?;
        self.add_column_if_missing("segments", "truth_enrol_ns", "INTEGER")?;
        // Which prototypes Discord's word put in the bank (`store::truth_via`).
        // NULL on every row that predates this and on every ordinary
        // enrolment, which is what it should mean: nothing to say.
        self.add_column_if_missing("speaker_prototypes", "via", "TEXT")?;
        self.conn.execute_batch(
            // The labelling queue is "Discord segments with no verdict", and
            // the enrolment queue is "single segments with no enrol stamp".
            "CREATE INDEX IF NOT EXISTS idx_segments_truth
                 ON segments(truth_verdict, t_start_ns);
             CREATE INDEX IF NOT EXISTS idx_segments_truth_enrol
                 ON segments(truth_enrol_ns, t_start_ns);",
        )?;
        Ok(())
    }

    /// Schema v17: which bridge a speaking span came from.
    ///
    /// Two nullable columns and one index. No backfill — see the banner at the
    /// top of this file: NULL means "the only bridge there was", which is a
    /// fact about the row and not a placeholder for one.
    fn apply_v17(&self) -> Result<()> {
        self.add_column_if_missing("truth_speaking", "account_id", "TEXT")?;
        self.add_column_if_missing("truth_speaking", "client_kind", "TEXT")?;
        // The open-span lookup is now per bridge, and the roll-up
        // `truth_bridges` reads is a group over these two columns.
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_truth_speaking_bridge
                 ON truth_speaking(account_id, t_start_ns);",
        )?;
        Ok(())
    }

    // ---- 0.12.4 (schema v19): word-level ground truth ---------------------

    /// The `text_truth` table, and the one backfill that fills it from history.
    ///
    /// `UNIQUE(segment_id, created_ns)` is what makes this idempotent, and it
    /// is the natural key rather than a convenience: `created_ns` is the
    /// instant of the `segments.correct` operation, so one correction is one
    /// row and re-running the backfill writes nothing. A turn corrected twice
    /// keeps both rows — the second correction is evidence the first transcript
    /// was wrong too, exactly as `accuracy::corrections` has always counted it.
    fn apply_v19(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS text_truth (
                 id             INTEGER PRIMARY KEY,
                 segment_id     INTEGER NOT NULL REFERENCES segments(id),
                 -- What the person typed. The reference transcript.
                 truth_text     TEXT    NOT NULL,
                 -- What each pass that ever read this clip made of it. NULL
                 -- means that pass never ran on this row, which is not the
                 -- same as it having read nothing.
                 live_text      TEXT,
                 context_text   TEXT,
                 night_text     TEXT,
                 -- Always NULL today: the cross-check decoder stores its
                 -- VERDICT and not its words (`crate::quality`). The column
                 -- exists so the day that changes is a one-line write rather
                 -- than a migration. See `crate::text_truth`.
                 canary_text    TEXT,
                 -- The verdict standing over the words being replaced.
                 asr_confidence TEXT,
                 -- The three facets a measurement is cut by, as they were at
                 -- the moment of the correction.
                 speaker_id     INTEGER REFERENCES speakers(id),
                 source_kind    TEXT    NOT NULL,
                 duration_ns    INTEGER NOT NULL,
                 created_ns     INTEGER NOT NULL,
                 UNIQUE(segment_id, created_ns)
             );
             CREATE INDEX IF NOT EXISTS idx_text_truth_created
                 ON text_truth(created_ns);
             CREATE INDEX IF NOT EXISTS idx_text_truth_cell
                 ON text_truth(source_kind, speaker_id, created_ns);",
        )?;
        crate::text_truth::backfill(self)?;
        Ok(())
    }

    /// One correction, with everything a measurement needs beside it.
    pub fn insert_text_truth(&self, row: &TextTruth) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO text_truth
                 (segment_id, truth_text, live_text, context_text, night_text,
                  canary_text, asr_confidence, speaker_id, source_kind,
                  duration_ns, created_ns)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                row.segment_id,
                row.truth_text,
                row.live_text,
                row.context_text,
                row.night_text,
                row.canary_text,
                row.asr_confidence,
                row.speaker_id,
                row.source_kind,
                row.duration_ns,
                row.created_ns,
            ],
        )?;
        Ok(n > 0)
    }

    /// Every truth row, oldest first — the order a chronological held-out split
    /// has to read them in.
    pub fn text_truth_rows(&self, limit: usize) -> Result<Vec<TextTruth>> {
        let mut stmt = self.conn.prepare(
            "SELECT segment_id, truth_text, live_text, context_text, night_text,
                    canary_text, asr_confidence, speaker_id, source_kind,
                    duration_ns, created_ns
             FROM text_truth ORDER BY created_ns ASC, id ASC LIMIT ?1",
        )?;
        Ok(stmt
            .query_map(params![limit as i64], |r| {
                Ok(TextTruth {
                    segment_id: r.get(0)?,
                    truth_text: r.get(1)?,
                    live_text: r.get(2)?,
                    context_text: r.get(3)?,
                    night_text: r.get(4)?,
                    canary_text: r.get(5)?,
                    asr_confidence: r.get(6)?,
                    speaker_id: r.get(7)?,
                    source_kind: r.get(8)?,
                    duration_ns: r.get(9)?,
                    created_ns: r.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Schema v18: how a turn sounded.
    ///
    /// Three nullable columns and one index. No backfill — see the banner at
    /// the top of this file. The index is the pass's own queue
    /// (`mood_at_ns IS NULL`, then by capture time), so a walk over a
    /// twenty-thousand-row archive is a range scan rather than a table scan
    /// every minute for the life of the daemon. The queue is drained **newest
    /// first** ([`Store::segments_for_mood`]); the index serves either
    /// direction, because a B-tree read backwards is still a range scan.
    fn apply_v18(&self) -> Result<()> {
        self.add_column_if_missing("segments", "mood", "TEXT")?;
        self.add_column_if_missing("segments", "events", "TEXT")?;
        self.add_column_if_missing("segments", "mood_at_ns", "INTEGER")?;
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_segments_mood_queue
                 ON segments(mood_at_ns, t_start_ns);",
        )?;
        Ok(())
    }

    // ---- the mood pass (0.12.4, `crate::mood`) ---------------------------

    /// One row nobody has listened to for a mood yet, with everything the pass
    /// needs and nothing else.
    ///
    /// Deliberately not a [`SweepCandidate`]: that struct carries the speaker's
    /// declared languages because the language route cannot run without them,
    /// and a mood is a fact about a sound rather than about a language — the
    /// pass asks nothing about who is speaking or what they said. What it does
    /// need that the sweep does not is the *live text*, which is what
    /// `spike/mood_bench.py` scored the laughter tag against — see FINDINGS
    /// §42.5, where "the decoder had no words for this clip" turned out to be
    /// the only proxy on this corpus with enough positives to separate
    /// anything.
    ///
    /// **Newest first**, unlike the sweep's queue and unlike every other
    /// backlog here. What was just said is what somebody opens the transcript
    /// to look at tonight; the tail of a two-year archive can wait, and at
    /// RTF 0.08 it will not wait long. The walk stays resumable for exactly
    /// the reason it did when it ran the other way — the cursor is
    /// `mood_at_ns IS NULL` and not an offset, so a row stamped by one batch
    /// is gone from the next one whichever end the pass started at, and a
    /// daemon killed mid-archive resumes where it stopped rather than at the
    /// beginning. Rows captured *while* the pass is running are newer than
    /// anything it has read and are therefore taken first, which is the
    /// behaviour the ordering is for.
    pub fn segments_for_mood(
        &self,
        min_duration_s: f32,
        limit: usize,
    ) -> Result<Vec<MoodCandidate>> {
        let min_ns = (min_duration_s.max(0.0) as f64 * 1e9) as i64;
        Ok(self
            .conn
            .prepare(
                "SELECT g.id, g.t_end_ns - g.t_start_ns, g.audio_path, g.text
                 FROM segments g
                 WHERE g.deleted_at IS NULL
                   AND g.mood_at_ns IS NULL
                   AND g.audio_path <> ''
                   AND g.t_end_ns - g.t_start_ns >= ?1
                 ORDER BY g.t_start_ns DESC
                 LIMIT ?2",
            )?
            .query_map(params![min_ns, limit.max(1) as i64], |r| {
                Ok(MoodCandidate {
                    id: r.get(0)?,
                    duration_s: r.get::<_, i64>(1)? as f32 / 1e9,
                    audio_path: r.get(2)?,
                    text: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn text_truth_count(&self) -> Result<i64> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM text_truth", [], |r| r.get(0))?)
    }

    /// The facets one segment falls under: whose voice, which kind of source,
    /// how long, what the cross-check said, and what the night shift read.
    ///
    /// Read **now** rather than from the log, exactly as `accuracy::summary`
    /// buckets its corrections: a turn reassigned since is a turn that belongs
    /// to the voice it belongs to today.
    pub fn segment_facets(&self, segment_id: i64) -> Result<Option<SegmentFacets>> {
        Ok(self
            .conn
            .query_row(
                "SELECT g.speaker_id, src.kind, g.t_end_ns - g.t_start_ns,
                        g.asr_confidence, g.night_text, g.text, g.text_via
                 FROM segments g
                 JOIN sessions ss ON ss.id = g.session_id
                 JOIN sources src ON src.id = ss.source_id
                 WHERE g.id = ?1",
                params![segment_id],
                |r| {
                    Ok(SegmentFacets {
                        speaker_id: r.get(0)?,
                        source_kind: r.get(1)?,
                        duration_ns: r.get(2)?,
                        asr_confidence: r.get(3)?,
                        night_text: r.get(4)?,
                        text: r.get(5)?,
                        text_via: r.get(6)?,
                    })
                },
            )
            .optional()?)
    }

    /// Every machine rewrite this row has been through, oldest first: when it
    /// happened, and the `(text, text_via)` the `segments.redecode` operation
    /// kept of the state it replaced.
    ///
    /// Same `target_ids` match as `segments_routed_by_lid`, and for the same
    /// reason: the column is written as `[<id>]` by every writer of it.
    pub fn redecode_priors(&self, segment_id: i64) -> Result<Vec<RedecodePrior>> {
        let mut stmt = self.conn.prepare(
            "SELECT at_utc_ns, prior_state FROM operations
             WHERE op = 'segments.redecode' AND target_ids = '[' || ?1 || ']'
             ORDER BY at_utc_ns ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![segment_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .iter()
            .map(|(at, raw)| {
                let v: Option<serde_json::Value> = serde_json::from_str(raw).ok();
                let field = |key: &str| {
                    v.as_ref()
                        .and_then(|v| v.get(key))
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                };
                RedecodePrior {
                    at_utc_ns: *at,
                    text: field("text"),
                    text_via: field("text_via"),
                }
            })
            .collect())
    }

    /// What the pass heard, and that it has been here.
    ///
    /// Called on **every** row the pass reaches, including the two where it
    /// heard nothing: a model that abstained (`mood = None`) and a clip
    /// retention has taken (both `None`). That is the whole of why
    /// `mood_at_ns` exists as a separate column — see the v18 banner.
    ///
    /// `events` is stored as the sorted, comma-joined closed set rather than as
    /// a JSON array, for the reason `speakers.languages` is: it is a handful of
    /// short fixed words, every reader of it wants a `LIKE '%laughter%'` and no
    /// reader ever wants half of one.
    pub fn set_segment_mood(
        &self,
        segment_id: i64,
        mood: Option<&str>,
        events: Option<&str>,
        at_utc_ns: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE segments SET mood = ?2, events = ?3, mood_at_ns = ?4
             WHERE id = ?1 AND deleted_at IS NULL",
            params![segment_id, mood, events, at_utc_ns],
        )?;
        Ok(())
    }

    /// How much archive the mood pass still owes at a given floor, and how much
    /// it has already been over: `(owed, read)`.
    ///
    /// The first half is [`Self::segments_for_mood`]'s filter term for term,
    /// which is the property `lang_sweep_counts` had to learn the hard way: a
    /// status line that counts rows the walk would decline is a status line
    /// that prints the same backlog forever.
    pub fn mood_counts(&self, min_duration_s: f32) -> Result<(i64, i64)> {
        let min_ns = (min_duration_s.max(0.0) as f64 * 1e9) as i64;
        Ok(self
            .conn
            .query_row(
                "SELECT
                   SUM(CASE WHEN g.mood_at_ns IS NULL THEN 1 ELSE 0 END),
                   SUM(CASE WHEN g.mood_at_ns IS NOT NULL THEN 1 ELSE 0 END)
                 FROM segments g
                 WHERE g.deleted_at IS NULL
                   AND g.audio_path <> ''
                   AND g.t_end_ns - g.t_start_ns >= ?1",
                params![min_ns],
                |r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?)),
            )
            .map(|(a, b)| (a.unwrap_or(0), b.unwrap_or(0)))?)
    }

    /// How one voice has sounded, over every row the pass has read of theirs.
    ///
    /// Counts, not a verdict, and `read` is on the wire beside them because a
    /// count with no denominator is a claim: "3 laughs" means nothing until you
    /// know whether the pass has been over thirty of their turns or three
    /// thousand. `person.get` renders the ratio and refuses to render anything
    /// at all under [`crate::mood::MIN_ROWS_FOR_A_SUMMARY`].
    ///
    /// Resolved through `speaker_resolved` like every other per-person total,
    /// so a merged-away voice's turns count toward the voice that survived.
    pub fn person_mood(&self, speaker_id: i64) -> Result<MoodTotals> {
        Ok(self.conn.query_row(
            "SELECT
               SUM(CASE WHEN g.mood_at_ns IS NOT NULL THEN 1 ELSE 0 END),
               SUM(CASE WHEN g.mood = 'happy' THEN 1 ELSE 0 END),
               SUM(CASE WHEN g.mood = 'sad' THEN 1 ELSE 0 END),
               SUM(CASE WHEN g.mood = 'angry' THEN 1 ELSE 0 END),
               SUM(CASE WHEN g.mood = 'neutral' THEN 1 ELSE 0 END),
               SUM(CASE WHEN g.events LIKE '%laughter%' THEN 1 ELSE 0 END),
               SUM(CASE WHEN g.events LIKE '%music%' THEN 1 ELSE 0 END),
               MAX(CASE WHEN g.mood_at_ns IS NOT NULL THEN g.t_start_ns END)
             FROM segments g
             JOIN speaker_resolved sp ON sp.id = g.speaker_id
             WHERE sp.canonical_id = ?1 AND g.deleted_at IS NULL",
            params![speaker_id],
            |r| {
                Ok(MoodTotals {
                    read: r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    happy: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    sad: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    angry: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    neutral: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    laughter: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    music: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    last_ns: r.get(7)?,
                })
            },
        )?)
    }

    /// The same totals over one conversation, for the "how it felt" line.
    pub fn thread_mood(&self, thread_id: i64) -> Result<MoodTotals> {
        Ok(self.conn.query_row(
            "SELECT
               SUM(CASE WHEN g.mood_at_ns IS NOT NULL THEN 1 ELSE 0 END),
               SUM(CASE WHEN g.mood = 'happy' THEN 1 ELSE 0 END),
               SUM(CASE WHEN g.mood = 'sad' THEN 1 ELSE 0 END),
               SUM(CASE WHEN g.mood = 'angry' THEN 1 ELSE 0 END),
               SUM(CASE WHEN g.mood = 'neutral' THEN 1 ELSE 0 END),
               SUM(CASE WHEN g.events LIKE '%laughter%' THEN 1 ELSE 0 END),
               SUM(CASE WHEN g.events LIKE '%music%' THEN 1 ELSE 0 END),
               MAX(CASE WHEN g.mood_at_ns IS NOT NULL THEN g.t_start_ns END)
             FROM segments g
             WHERE g.thread_id = ?1 AND g.deleted_at IS NULL",
            params![thread_id],
            |r| {
                Ok(MoodTotals {
                    read: r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                    happy: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    sad: r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                    angry: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    neutral: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    laughter: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                    music: r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    last_ns: r.get(7)?,
                })
            },
        )?)
    }

    /// Open a speaking row, closing anything this user already had open.
    ///
    /// Two starts with no stop between them is a dropped batch, not two
    /// overlapping utterances by one person — so the earlier row is closed at
    /// the later one's start rather than left to run.
    ///
    /// **Per bridge since v17.** "This user already had one open" is a question
    /// about one client's view of them: with two bridges in two calls the same
    /// account can genuinely be talking in both, and closing Vesktop's span
    /// because the official client saw a ring light up would cut a real
    /// utterance short. `IS` rather than `=` so NULL matches NULL, which keeps
    /// an old plugin's spans a single stream of their own.
    pub fn truth_speaking_start(
        &self,
        user_id: &str,
        name: &str,
        channel_id: Option<&str>,
        t_ns: i64,
        client: &crate::bridge::ClientRef,
    ) -> Result<i64> {
        let account = client.account_id.as_deref();
        let kind = client.kind.map(crate::bridge::ClientKind::as_str);
        self.conn.execute(
            "UPDATE truth_speaking SET t_end_ns = MAX(t_start_ns, ?2)
             WHERE user_id = ?1 AND t_end_ns IS NULL AND account_id IS ?3",
            params![user_id, t_ns, account],
        )?;
        self.conn.execute(
            "INSERT INTO truth_speaking
                 (user_id, name, channel_id, t_start_ns, t_end_ns, account_id, client_kind)
             VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
            params![user_id, name, channel_id, t_ns, account, kind],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Close this user's open speaking row. A stop with no start is dropped:
    /// there is no span to invent, and inventing one would put speech on the
    /// timeline that nobody reported.
    ///
    /// Scoped to the bridge that sent the stop, for [`Self::truth_speaking_start`]'s
    /// reason: one client's `stop` is not evidence about the other's ring.
    pub fn truth_speaking_stop(
        &self,
        user_id: &str,
        t_ns: i64,
        client: &crate::bridge::ClientRef,
    ) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE truth_speaking SET t_end_ns = MAX(t_start_ns, ?2)
             WHERE id = (SELECT id FROM truth_speaking
                         WHERE user_id = ?1 AND t_end_ns IS NULL AND account_id IS ?3
                         ORDER BY t_start_ns DESC LIMIT 1)",
            params![user_id, t_ns, client.account_id.as_deref()],
        )?;
        Ok(n > 0)
    }

    /// Close every row that has been open longer than `timeout_ns`, at the
    /// timeout rather than at now: the plugin was not running, so the last
    /// thing we actually know is that they were talking when we lost them.
    /// Returns how many were closed.
    pub fn truth_close_open(&self, now_ns: i64, timeout_ns: i64) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE truth_speaking SET t_end_ns = t_start_ns + ?2
             WHERE t_end_ns IS NULL AND t_start_ns + ?2 <= ?1",
            params![now_ns, timeout_ns],
        )?)
    }

    /// Record that we have seen this Discord account, and what it is calling
    /// itself. Never touches `speaker_id` — a sighting is not a link.
    pub fn upsert_discord_user(&self, user_id: &str, name: &str, at_ns: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO discord_users (user_id, name, first_seen_ns, last_seen_ns)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(user_id) DO UPDATE SET
                 name          = excluded.name,
                 last_seen_ns  = MAX(discord_users.last_seen_ns, excluded.last_seen_ns),
                 first_seen_ns = MIN(discord_users.first_seen_ns, excluded.first_seen_ns)",
            params![user_id, name, at_ns],
        )?;
        Ok(())
    }

    const DISCORD_USER_COLUMNS: &'static str =
        "d.user_id, d.name, d.speaker_id, d.via, d.linked_at_ns, d.first_seen_ns, d.last_seen_ns,
         (SELECT s.display_name FROM speakers s WHERE s.id = d.speaker_id),
         -- The highlight (v15), so the truth-link list picks a person out the
         -- same way every other list of names does.
         (SELECT s.colour FROM speakers s WHERE s.id = d.speaker_id),
         (SELECT s.icon FROM speakers s WHERE s.id = d.speaker_id)
         FROM discord_users d";

    fn discord_user_row_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<DiscordUserRow> {
        Ok(DiscordUserRow {
            user_id: r.get(0)?,
            name: r.get(1)?,
            speaker_id: r.get(2)?,
            via: r.get(3)?,
            linked_at_ns: r.get(4)?,
            first_seen_ns: r.get(5)?,
            last_seen_ns: r.get(6)?,
            speaker_name: r.get(7)?,
            speaker_colour: r.get(8)?,
            speaker_icon: r.get(9)?,
        })
    }

    pub fn discord_users(&self) -> Result<Vec<DiscordUserRow>> {
        let sql = format!(
            "SELECT {} ORDER BY d.last_seen_ns DESC, d.user_id ASC",
            Self::DISCORD_USER_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt
            .query_map([], Self::discord_user_row_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn discord_user(&self, user_id: &str) -> Result<Option<DiscordUserRow>> {
        let sql = format!("SELECT {} WHERE d.user_id = ?1", Self::DISCORD_USER_COLUMNS);
        Ok(self
            .conn
            .query_row(&sql, params![user_id], Self::discord_user_row_from)
            .optional()?)
    }

    /// Point a Discord account at a voice, or (with `speaker_id` `None`) stop
    /// pointing it anywhere. Returns the row as it now stands, or `None` when
    /// there is no such user.
    pub fn set_discord_link(
        &self,
        user_id: &str,
        speaker_id: Option<i64>,
        via: Option<&str>,
        at_ns: i64,
    ) -> Result<Option<DiscordUserRow>> {
        let n = self.conn.execute(
            "UPDATE discord_users
                SET speaker_id = ?2, via = ?3, linked_at_ns = ?4
              WHERE user_id = ?1",
            params![
                user_id,
                speaker_id,
                speaker_id.and(via),
                speaker_id.map(|_| at_ns)
            ],
        )?;
        if n == 0 {
            return Ok(None);
        }
        self.discord_user(user_id)
    }

    /// Every speaking span that touches `[from_ns, to_ns)`, oldest first. An
    /// open span is reported running to `to_ns` — it is still going as far as
    /// anybody knows, and clipping it there is what keeps coverage ≤ 1.
    pub fn truth_spans_between(&self, from_ns: i64, to_ns: i64) -> Result<Vec<TruthSpan>> {
        self.truth_spans_between_scoped(from_ns, to_ns, &crate::bridge::Scope::Every)
    }

    /// [`Self::truth_spans_between`], restricted to one bridge's spans
    /// (0.12.3).
    ///
    /// The three scopes are `crate::bridge::Scope`'s and the SQL is one clause
    /// each. The clause that matters is the middle one: `account_id IS NULL OR
    /// account_id = ?` — an unscoped span belongs to every scope, because it
    /// was written when there was one bridge and it is evidence about whatever
    /// that bridge could hear. Without that half, upgrading the daemon would
    /// silently un-judge the whole archive.
    pub fn truth_spans_between_scoped(
        &self,
        from_ns: i64,
        to_ns: i64,
        scope: &crate::bridge::Scope,
    ) -> Result<Vec<TruthSpan>> {
        let clause = match scope {
            crate::bridge::Scope::Every => "",
            crate::bridge::Scope::Account(_) => " AND (account_id IS NULL OR account_id = ?3)",
            crate::bridge::Scope::Legacy => " AND account_id IS NULL",
        };
        let sql = format!(
            "SELECT user_id, name, t_start_ns, COALESCE(t_end_ns, ?2), account_id, client_kind
               FROM truth_speaking
              WHERE t_start_ns < ?2 AND COALESCE(t_end_ns, ?2) > ?1{clause}
              ORDER BY t_start_ns ASC, id ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let row = |r: &rusqlite::Row<'_>| {
            Ok(TruthSpan {
                user_id: r.get(0)?,
                name: r.get(1)?,
                t_start_ns: r.get(2)?,
                t_end_ns: r.get(3)?,
                account_id: r.get(4)?,
                client_kind: r.get(5)?,
            })
        };
        let rows = match scope {
            crate::bridge::Scope::Account(a) => stmt
                .query_map(params![from_ns, to_ns, a], row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            _ => stmt
                .query_map(params![from_ns, to_ns], row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };
        Ok(rows)
    }

    /// Every bridge that has ever written a scoped span, with what
    /// `truth.status` and [`crate::bridge::scope_for_source`] need of it
    /// (0.12.3).
    ///
    /// Read from `truth_speaking` and not from a live registry, deliberately:
    /// the verdict pass runs against segments recorded hours ago and has to
    /// know which bridges existed *then*, not which ones happen to be
    /// connected now. `recent_ns` is the window the per-minute rate is measured
    /// over.
    pub fn truth_bridges(
        &self,
        now_ns: i64,
        recent_ns: i64,
    ) -> Result<Vec<crate::bridge::BridgeSeen>> {
        let mut stmt = self.conn.prepare(
            "SELECT account_id, client_kind, MAX(t_start_ns), COUNT(*),
                    SUM(CASE WHEN t_start_ns >= ?1 THEN 1 ELSE 0 END)
               FROM truth_speaking
              WHERE account_id IS NOT NULL
              GROUP BY account_id, client_kind
              ORDER BY account_id ASC, client_kind ASC",
        )?;
        Ok(stmt
            .query_map(params![now_ns - recent_ns], |r| {
                Ok(crate::bridge::BridgeSeen {
                    account_id: r.get(0)?,
                    kind: r
                        .get::<_, Option<String>>(1)?
                        .as_deref()
                        .and_then(crate::bridge::ClientKind::parse),
                    last_span_ns: r.get(2)?,
                    spans: r.get(3)?,
                    spans_recent: r.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The mixed Discord sources this install has ever recorded, for the status
    /// line that says which bridge each one is scoped to.
    pub fn discord_source_keys(&self, patterns: &[String]) -> Result<Vec<String>> {
        if patterns.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT match_key FROM sources sc
              WHERE ({}) AND match_key NOT LIKE 'discord:%'
              ORDER BY match_key ASC",
            Self::discord_source_clause(patterns.len(), 1)
        );
        let binds: Vec<String> = patterns.iter().map(|p| p.to_lowercase()).collect();
        let refs: Vec<&dyn rusqlite::ToSql> =
            binds.iter().map(|b| b as &dyn rusqlite::ToSql).collect();
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt
            .query_map(refs.as_slice(), |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How many speaking rows there are, and how many are still open. For
    /// `truth.status`, which is the first thing anybody looks at when the
    /// plugin does not seem to be arriving.
    pub fn truth_span_counts(&self) -> Result<(i64, i64)> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(t_end_ns IS NULL), 0) FROM truth_speaking",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }

    /// The most recent speaking row's start, or `None` when there are none.
    pub fn truth_last_span_ns(&self) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row("SELECT MAX(t_start_ns) FROM truth_speaking", [], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .optional()?
            .flatten())
    }

    /// SQL that is true for a session whose source looks like Discord.
    /// `?N` is bound to a single lower-cased pattern; the caller ORs one copy
    /// per configured pattern together, because SQLite has no array bind and a
    /// hand-built `IN` list of user strings is how injections happen.
    fn discord_source_clause(patterns: usize, first_param: usize) -> String {
        if patterns == 0 {
            return "0".to_string();
        }
        (0..patterns)
            .map(|i| {
                let p = first_param + i;
                format!(
                    "(INSTR(LOWER(sc.match_key), ?{p}) > 0 \
                      OR INSTR(LOWER(sc.display_name), ?{p}) > 0)"
                )
            })
            .collect::<Vec<_>>()
            .join(" OR ")
    }

    /// Discord segments waiting for a verdict, newest first.
    ///
    /// The queue is "no verdict yet", plus `unknown` rows that truth has since
    /// caught up with — a segment labelled `unknown` because the plugin was
    /// not running has to be re-examined if the plugin later backfills that
    /// moment, and a segment that is still uncovered is left alone forever
    /// rather than being re-read every twenty seconds.
    pub fn segments_for_truth(
        &self,
        patterns: &[String],
        limit: usize,
    ) -> Result<Vec<TruthCandidate>> {
        if patterns.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT g.id, g.t_start_ns, g.t_end_ns, g.speaker_id, g.overlap_frac, sc.kind,
                    sc.match_key
               FROM segments g
               JOIN sessions ss ON ss.id = g.session_id
               JOIN sources  sc ON sc.id = ss.source_id
              WHERE g.deleted_at IS NULL
                AND ({})
                AND (g.truth_verdict IS NULL
                     OR (g.truth_verdict = ?1
                         AND EXISTS (SELECT 1 FROM truth_speaking t
                                      WHERE t.t_start_ns < g.t_end_ns
                                        AND COALESCE(t.t_end_ns, g.t_end_ns) > g.t_start_ns)))
              ORDER BY g.t_start_ns DESC
              LIMIT ?2",
            Self::discord_source_clause(patterns.len(), 3)
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut binds: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(truth_verdict::UNKNOWN), Box::new(limit as i64)];
        for p in patterns {
            binds.push(Box::new(p.to_lowercase()));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
        Ok(stmt
            .query_map(refs.as_slice(), |r| {
                Ok(TruthCandidate {
                    id: r.get(0)?,
                    t_start_ns: r.get(1)?,
                    t_end_ns: r.get(2)?,
                    speaker_id: r.get(3)?,
                    overlap_frac: r.get(4)?,
                    kind: r.get(5)?,
                    source: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Stamp a segment with what Discord said about it. Never touches
    /// `speaker_id`: the verdict is the yardstick, not the answer.
    pub fn set_segment_truth(
        &self,
        segment_id: i64,
        user_id: Option<&str>,
        verdict: &str,
        coverage: Option<f64>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE segments
                SET truth_user_id = ?2, truth_verdict = ?3, truth_coverage = ?4
              WHERE id = ?1",
            params![segment_id, user_id, verdict, coverage],
        )?;
        Ok(())
    }

    /// What was stamped on one segment. `None` when there is no such segment;
    /// a live segment with no verdict yet answers with an all-`None` row,
    /// which is a different fact and reads as one.
    pub fn segment_truth(&self, segment_id: i64) -> Result<Option<SegmentTruth>> {
        Ok(self
            .conn
            .query_row(
                "SELECT truth_verdict, truth_user_id, truth_coverage FROM segments WHERE id = ?1",
                params![segment_id],
                |r| {
                    Ok(SegmentTruth {
                        verdict: r.get(0)?,
                        user_id: r.get(1)?,
                        coverage: r.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// Clean single-speaker turns belonging to a LINKED user that no
    /// enrolment pass has looked at yet, newest first.
    pub fn segments_for_truth_enrol(
        &self,
        min_duration_s: f64,
        min_coverage: f64,
        limit: usize,
    ) -> Result<Vec<TruthEnrolCandidate>> {
        let min_ns = (min_duration_s * 1e9) as i64;
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.truth_user_id, d.speaker_id, g.t_start_ns, g.t_end_ns,
                    g.overlap_frac, LENGTH(TRIM(COALESCE(g.text, '')))
               FROM segments g
               JOIN discord_users d ON d.user_id = g.truth_user_id
              WHERE g.deleted_at IS NULL
                AND g.truth_verdict = ?1
                AND g.truth_enrol_ns IS NULL
                AND d.speaker_id IS NOT NULL
                AND g.truth_coverage >= ?2
                AND (g.t_end_ns - g.t_start_ns) >= ?3
              ORDER BY g.t_start_ns DESC
              LIMIT ?4",
        )?;
        Ok(stmt
            .query_map(
                params![truth_verdict::SINGLE, min_coverage, min_ns, limit as i64],
                |r| {
                    Ok(TruthEnrolCandidate {
                        id: r.get(0)?,
                        user_id: r.get(1)?,
                        speaker_id: r.get(2)?,
                        t_start_ns: r.get(3)?,
                        t_end_ns: r.get(4)?,
                        overlap_frac: r.get(5)?,
                        text_len: r.get(6)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---- 0.12.0: retro-labelling from ground truth -------------------------

    /// How many turns would enrol if `[truth] enrol` were on, and never will
    /// while it is off.
    ///
    /// The same `WHERE` as [`Self::segments_for_truth_enrol`] with the `LIMIT`
    /// and the column list taken off. It exists because §29 found the enrolment
    /// pass had never fired on an install with 137 turns queued for it, and the
    /// reason was neither a bug nor a bar the data could not reach: the feature
    /// was simply off, and nothing anybody could run said so. A count in
    /// `truth.summary` is the cheapest possible cure — the operator sees the
    /// queue and the switch in the same report.
    pub fn truth_enrol_waiting(&self, min_duration_s: f64, min_coverage: f64) -> Result<i64> {
        let min_ns = (min_duration_s * 1e9) as i64;
        Ok(self.conn.query_row(
            "SELECT COUNT(*)
               FROM segments g
               JOIN discord_users d ON d.user_id = g.truth_user_id
              WHERE g.deleted_at IS NULL
                AND g.truth_verdict = ?1
                AND g.truth_enrol_ns IS NULL
                AND d.speaker_id IS NOT NULL
                AND g.truth_coverage >= ?2
                AND (g.t_end_ns - g.t_start_ns) >= ?3",
            params![truth_verdict::SINGLE, min_coverage, min_ns],
            |r| r.get(0),
        )?)
    }

    /// Turns Discord can name and the voicebank could not: `single` verdict, a
    /// linked user, and no speaker at all.
    ///
    /// No duration bar, deliberately, and it is the one place in this file that
    /// does not have one. Every other truth pass filters short turns because it
    /// is *measuring* the voicebank, and a sub-second grunt the embedder refused
    /// would measure the floor rather than the model
    /// (`truth::MIN_SCORE_DURATION_S`). This pass measures nothing. It copies a
    /// name Discord already wrote onto a row that has none, and Discord's word
    /// about a one-second turn is exactly as good as its word about a ten-second
    /// one — the ring was drawn from the same flux event. Refusing the short
    /// ones would leave the shortest turns, which are the hardest to label by
    /// any other route, permanently anonymous for no reason anybody could state.
    ///
    /// Ordered oldest-first so a `--limit` run takes the backlog in the order it
    /// happened rather than a random slice of it.
    ///
    /// `you` is the pinned "You" voice and is **excluded**, which is not a
    /// nicety — it is the difference between this pass helping and quietly
    /// corrupting the archive. 0.10.1 already established the fact
    /// (`truth::summary`, FINDINGS §17): a Discord client never plays your own
    /// microphone back to you, so yours is the one voice a turn captured from
    /// that client's output *cannot* contain. A `single` verdict naming your own
    /// account therefore says "you were talking over this", not "this is you",
    /// and the scoring path drops those rows for exactly that reason. Writing
    /// them as labels would put your name on 17 turns of somebody else's voice
    /// on the install §29 measured — permanently, in the thing you later read
    /// back as memory. Passing `None` disables the exclusion and is meant for
    /// tests; the daemon and the CLI both pass `store.you_speaker_id()`.
    pub fn segments_for_truth_label(
        &self,
        you: Option<i64>,
        limit: usize,
    ) -> Result<Vec<TruthLabelCandidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.truth_user_id, d.name, d.speaker_id, g.t_start_ns, g.t_end_ns,
                    g.truth_coverage
               FROM segments g
               JOIN discord_users d ON d.user_id = g.truth_user_id
              WHERE g.deleted_at IS NULL
                AND g.truth_verdict = ?1
                AND g.speaker_id IS NULL
                AND d.speaker_id IS NOT NULL
                AND (?3 IS NULL OR d.speaker_id != ?3)
              ORDER BY g.t_start_ns ASC, g.id ASC
              LIMIT ?2",
        )?;
        Ok(stmt
            .query_map(params![truth_verdict::SINGLE, limit as i64, you], |r| {
                Ok(TruthLabelCandidate {
                    id: r.get(0)?,
                    user_id: r.get(1)?,
                    user_name: r.get(2)?,
                    speaker_id: r.get(3)?,
                    t_start_ns: r.get(4)?,
                    t_end_ns: r.get(5)?,
                    coverage: r.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Put Discord's name on a row the voicebank left blank.
    ///
    /// `AND speaker_id IS NULL` is repeated here even though the query that
    /// produced the candidate already required it: the gather and the commit
    /// happen under two different acquisitions of the store lock, and in between
    /// them the live ladder may well have labelled the row itself. Losing the
    /// race is the correct outcome — the live label was made with the audio in
    /// hand — so the guard is in the `WHERE` and the return value says whether
    /// the write landed.
    pub fn label_segment_from_truth(&self, segment_id: i64, speaker_id: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE segments SET speaker_id = ?2, match_score = NULL, label_via = ?3
              WHERE id = ?1 AND speaker_id IS NULL AND deleted_at IS NULL",
            params![segment_id, speaker_id, label_via::TRUTH],
        )? > 0)
    }

    // ---- end 0.12.0 --------------------------------------------------------

    /// Mark an enrolment candidate as considered, whether or not it enrolled.
    /// Without this the pass would re-embed the same refused turn forever.
    pub fn mark_truth_enrol_considered(&self, segment_id: i64, at_ns: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE segments SET truth_enrol_ns = ?2 WHERE id = ?1",
            params![segment_id, at_ns],
        )?;
        Ok(())
    }

    /// Say a prototype came from Discord's word. Called straight after
    /// `add_prototype` returned its id, so the provenance and the row are
    /// written in the same breath.
    pub fn set_prototype_via(&self, prototype_id: i64, via: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE speaker_prototypes SET via = ?2 WHERE id = ?1",
            params![prototype_id, via],
        )?;
        Ok(())
    }

    /// How this Discord user's clean turns were labelled by the voicebank:
    /// `(speaker_id, count)`, commonest first, plus the count of clean turns
    /// the ladder declined to label at all.
    ///
    /// `min_duration_s` excludes the turns too short to be evidence of
    /// anything — the same floor the identity score uses, for the same reason.
    pub fn truth_label_histogram(
        &self,
        user_id: &str,
        min_duration_s: f64,
    ) -> Result<(Vec<(i64, i64)>, i64)> {
        let min_ns = (min_duration_s * 1e9) as i64;
        let mut stmt = self.conn.prepare(
            "SELECT g.speaker_id, COUNT(*) FROM segments g
              WHERE g.deleted_at IS NULL
                AND g.truth_verdict = ?1
                AND g.truth_user_id = ?2
                AND (g.t_end_ns - g.t_start_ns) >= ?3
              GROUP BY g.speaker_id
              ORDER BY COUNT(*) DESC, g.speaker_id ASC",
        )?;
        let rows: Vec<(Option<i64>, i64)> = stmt
            .query_map(params![truth_verdict::SINGLE, user_id, min_ns], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut labelled = Vec::new();
        let mut unlabelled = 0i64;
        for (speaker, n) in rows {
            match speaker {
                Some(id) => labelled.push((id, n)),
                None => unlabelled += n,
            }
        }
        Ok((labelled, unlabelled))
    }

    /// Stamp how much of a segment had two Discord users talking at once
    /// (v13, `crate::truth::simultaneous_frac`).
    ///
    /// Separate from `set_segment_truth` on purpose: the verdict is written
    /// for every candidate, this only when speaking spans were actually there
    /// to measure, and NULL has to keep meaning "nobody looked".
    pub fn set_segment_truth_overlap(&self, segment_id: i64, frac: f64) -> Result<()> {
        self.conn.execute(
            "UPDATE segments SET truth_overlap_frac = ?2 WHERE id = ?1",
            params![segment_id, frac],
        )?;
        Ok(())
    }

    /// What was stamped, or `None` where nothing has measured it.
    pub fn segment_truth_overlap(&self, segment_id: i64) -> Result<Option<f64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT truth_overlap_frac FROM segments WHERE id = ?1",
                params![segment_id],
                |r| r.get::<_, Option<f64>>(0),
            )
            .optional()?
            .flatten())
    }

    /// How many segments carry each verdict.
    pub fn truth_verdict_counts(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT truth_verdict, COUNT(*) FROM segments
              WHERE deleted_at IS NULL AND truth_verdict IS NOT NULL
              GROUP BY truth_verdict",
        )?;
        Ok(stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every `single` segment long enough to score, with the voicebank's
    /// answer beside the truth user's linked one. The identity ladder's whole
    /// report card comes out of this one query.
    pub fn truth_identity_rows(&self, min_duration_s: f64) -> Result<Vec<TruthScoreRow>> {
        let min_ns = (min_duration_s * 1e9) as i64;
        let mut stmt = self.conn.prepare(
            "SELECT g.truth_user_id, d.speaker_id, g.speaker_id
               FROM segments g
               JOIN discord_users d ON d.user_id = g.truth_user_id
              WHERE g.deleted_at IS NULL
                AND g.truth_verdict = ?1
                AND d.speaker_id IS NOT NULL
                AND (g.t_end_ns - g.t_start_ns) >= ?2",
        )?;
        Ok(stmt
            .query_map(params![truth_verdict::SINGLE, min_ns], |r| {
                Ok(TruthScoreRow {
                    user_id: r.get(0)?,
                    truth_speaker_id: r.get(1)?,
                    heard_speaker_id: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// `(verdict, flagged)` for every segment with a verdict, where flagged
    /// means the overlap gate refused it. Scored in Rust rather than in SQL
    /// so the threshold comes from the daemon's live operating point instead
    /// of being baked into a query.
    pub fn truth_overlap_rows(&self) -> Result<Vec<(String, Option<f32>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT truth_verdict, overlap_frac FROM segments
              WHERE deleted_at IS NULL AND truth_verdict IN (?1, ?2)",
        )?;
        Ok(stmt
            .query_map(
                params![truth_verdict::SINGLE, truth_verdict::OVERLAP],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The Discord accounts that are the user themselves (0.12.1).
    ///
    /// "The user" is the pinned "You" voice, and an account is theirs when it
    /// is linked to it — by hand or by the auto-linker, it makes no difference
    /// here. A list rather than one id because a person may have two accounts
    /// and an alt is exactly as inaudible in their own client's output as the
    /// main one; empty when nothing is linked, which is the state
    /// [`crate::truth::Audible`] treats as "nothing is known to be missing".
    pub fn own_discord_user_ids(&self) -> Result<Vec<String>> {
        match self.you_speaker_id()? {
            None => Ok(Vec::new()),
            Some(you) => self.discord_user_ids_for_speaker(you),
        }
    }

    /// Every verdict that could change if a user stops counting as present,
    /// oldest first (0.12.1, `truth::rejudge`).
    ///
    /// `single`, `overlap` and `partial` and nothing else, and that is a
    /// closed argument rather than an optimisation: the own-account rule only
    /// ever *removes* presence, and removing presence cannot turn `nobody`
    /// into anything or make an `unknown` known. Whatever else is re-judged
    /// later, this query is the complete set of rows this rule can touch, so
    /// the pass is bounded by the number of verdicts that assert somebody was
    /// talking — 3,781 on the install §34 measured, not the 9,770 with a
    /// verdict of any kind.
    pub fn segments_for_rejudge(&self, limit: usize) -> Result<Vec<RejudgeCandidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.t_start_ns, g.t_end_ns, sc.kind, g.truth_verdict,
                    g.truth_user_id, g.truth_coverage, g.truth_overlap_frac, sc.match_key
               FROM segments g
               JOIN sessions ss ON ss.id = g.session_id
               JOIN sources  sc ON sc.id = ss.source_id
              WHERE g.deleted_at IS NULL
                AND g.truth_verdict IN (?1, ?2, ?3)
              ORDER BY g.t_start_ns ASC, g.id ASC
              LIMIT ?4",
        )?;
        Ok(stmt
            .query_map(
                params![
                    truth_verdict::SINGLE,
                    truth_verdict::OVERLAP,
                    truth_verdict::PARTIAL,
                    limit as i64
                ],
                |r| {
                    Ok(RejudgeCandidate {
                        id: r.get(0)?,
                        t_start_ns: r.get(1)?,
                        t_end_ns: r.get(2)?,
                        kind: r.get(3)?,
                        verdict: r.get(4)?,
                        user_id: r.get(5)?,
                        coverage: r.get(6)?,
                        overlap_frac: r.get(7)?,
                        source: r.get(8)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Discord segments nothing has been able to say anything about: no
    /// verdict at all. Counted so `unknown` in the report is a real number
    /// rather than the absence of one.
    pub fn truth_unverdicted_count(&self, patterns: &[String]) -> Result<i64> {
        if patterns.is_empty() {
            return Ok(0);
        }
        let sql = format!(
            "SELECT COUNT(*) FROM segments g
               JOIN sessions ss ON ss.id = g.session_id
               JOIN sources  sc ON sc.id = ss.source_id
              WHERE g.deleted_at IS NULL AND g.truth_verdict IS NULL AND ({})",
            Self::discord_source_clause(patterns.len(), 1)
        );
        let binds: Vec<String> = patterns.iter().map(|p| p.to_lowercase()).collect();
        let refs: Vec<&dyn rusqlite::ToSql> =
            binds.iter().map(|b| b as &dyn rusqlite::ToSql).collect();
        Ok(self.conn.query_row(&sql, refs.as_slice(), |r| r.get(0))?)
    }

    // ---- end 0.9.0 -------------------------------------------------------
}

/// A Discord account we have heard from (v11).
#[derive(Debug, Clone)]
pub struct DiscordUserRow {
    pub user_id: String,
    /// The nickname the plugin last reported. Never written to a speaker.
    pub name: String,
    pub speaker_id: Option<i64>,
    /// `store::truth_via`, or `None` when unlinked.
    pub via: Option<String>,
    pub linked_at_ns: Option<i64>,
    pub first_seen_ns: i64,
    pub last_seen_ns: i64,
    /// The linked voice's name, for a client that wants to show both.
    pub speaker_name: Option<String>,
    /// And the linked voice's highlight (v15).
    pub speaker_colour: Option<String>,
    pub speaker_icon: Option<String>,
}

/// One stretch of one Discord user talking (v11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TruthSpan {
    pub user_id: String,
    pub name: String,
    pub t_start_ns: i64,
    /// An open span is reported clipped to the window it was asked for.
    pub t_end_ns: i64,
    /// Which bridge reported it (v17). `None` is a plugin that predates the
    /// field: the only bridge there was.
    pub account_id: Option<String>,
    pub client_kind: Option<String>,
}

/// A segment waiting for a verdict.
#[derive(Debug, Clone)]
pub struct TruthCandidate {
    pub id: i64,
    pub t_start_ns: i64,
    pub t_end_ns: i64,
    pub speaker_id: Option<i64>,
    pub overlap_frac: Option<f32>,
    /// `sources.kind` for the session this turn belongs to (0.12.1). The
    /// verdict needs it: which accounts the recording can physically contain
    /// is a fact about the *stream*, not about the call
    /// ([`crate::truth::Audible`]).
    pub kind: String,
    /// `sources.match_key` (0.12.3) — which Discord client's tap this is, and
    /// therefore which bridge's speaking spans are evidence about it
    /// ([`crate::bridge::scope_for_source`]).
    pub source: String,
}

/// A verdict already on disk, with everything a re-judge needs to redo it
/// (0.12.1). `coverage` and `overlap_frac` are what was stored, kept so the
/// pass can say what it could re-derive when the spans behind them are gone.
#[derive(Debug, Clone)]
pub struct RejudgeCandidate {
    pub id: i64,
    pub t_start_ns: i64,
    pub t_end_ns: i64,
    pub kind: String,
    pub verdict: String,
    pub user_id: Option<String>,
    pub coverage: Option<f64>,
    pub overlap_frac: Option<f64>,
    /// `sources.match_key` (0.12.3), for the scope. On the archive this is
    /// almost always answered `Scope::Every` — every span on disk predates the
    /// field — which is why re-judging an old install moves exactly the rows
    /// 0.12.1 said it would.
    pub source: String,
}

/// A clean turn that might be worth enrolling.
#[derive(Debug, Clone)]
pub struct TruthEnrolCandidate {
    pub id: i64,
    pub user_id: String,
    pub speaker_id: i64,
    pub t_start_ns: i64,
    pub t_end_ns: i64,
    pub overlap_frac: Option<f32>,
    /// Characters of transcript, so the pass can pick a plausible word count
    /// without loading the text it does not otherwise need.
    pub text_len: i64,
}

/// The truth stamp on one segment (v11).
#[derive(Debug, Clone, PartialEq)]
pub struct SegmentTruth {
    /// `store::truth_verdict`, or `None` when nothing has looked yet.
    pub verdict: Option<String>,
    /// The Discord account the verdict is about, when it is about one.
    pub user_id: Option<String>,
    /// That account's share of the segment.
    pub coverage: Option<f64>,
}

/// One scored `single` segment: what Discord said, and what we heard.
#[derive(Debug, Clone)]
pub struct TruthScoreRow {
    pub user_id: String,
    pub truth_speaker_id: i64,
    pub heard_speaker_id: Option<i64>,
}

// ===========================================================================
// 0.9.0 — the assistant (schema v11). One migration, and the queries the three
// features need: reminders that fire, one digest per conversation per day, and
// a translation on a turn.
//
// Its own impl block on purpose: three parallel worktrees are editing this
// file, and a block with a name is a block a merge can see the shape of.
// ===========================================================================

/// `digests.lang` is a plain tag, but the *day* is a string and the format is
/// load-bearing — it is the primary key's other half and what `digest.list`
/// filters on. Local calendar days, `YYYY-MM-DD`, because "yesterday" is a
/// thing that happens in a timezone and not in UTC.
pub const DAY_FORMAT: &str = "%Y-%m-%d";

/// One conversation, summarised. `people_json` and `open_json` are JSON arrays
/// on the row for the same reason `speakers.languages` is: they are read whole,
/// written whole, and never joined against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestRow {
    pub thread_id: i64,
    pub day: String,
    pub lang: String,
    /// The paragraph as it was rendered when the row was written — prose with
    /// people's names in it. Kept on the row so anything reading the database
    /// directly sees what the user saw.
    pub summary: String,
    pub people_json: String,
    pub open_json: String,
    /// 0.11.6: what the model actually wrote, before letters became names.
    /// `None` on every row written before 0.11.6, which is what makes those
    /// rows recognisable without a version column of their own.
    pub summary_raw: Option<String>,
    /// 0.11.6: the roster the letters stood for, `[{"id": 3, "label": "Aspen"}]`
    /// in letter order — `A` is index 0. The label is the one that was current
    /// when the digest was written; the id is what a re-render follows, so a
    /// rename moves the paragraph too.
    pub roster_json: Option<String>,
    /// 0.11.6: how `summary` was arrived at — `"names"` (the model was given
    /// the labels and wrote them), `"letters"` (it wrote letters and the
    /// daemon substituted), or `"legacy"` for a row from before any of this,
    /// which is rendered at read time.
    pub rendered: Option<String>,
    pub model_id: String,
    pub created_ns: i64,
}

/// A conversation the digest worker may look at: ended, long enough, and with
/// no digest yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestCandidate {
    pub thread_id: i64,
    pub started_ns: i64,
    pub ended_ns: i64,
    pub turns: i64,
}

/// A committed turn the translation pass may look at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslateCandidate {
    pub id: i64,
    pub text: String,
    pub lang: String,
}

impl Store {
    /// The assistant round (0.9.0). Four columns, one table, no backfill.
    ///
    /// `notes.due_ns` and `notes.fired_at_ns` are the reminder scheduler's
    /// whole state, and they are deliberately two columns rather than one
    /// nullable "next fire": a note that has fired keeps its due date, because
    /// the date is what the user said and the firing is what the daemon did.
    ///
    /// `segments.translation` / `translation_via` sit on the segment rather
    /// than in a table, like `threads.topic`: one string per turn, dead the
    /// moment the turn is, and a table would be a second thing that can
    /// disagree with `segments`.
    fn apply_v11_assist(&self) -> Result<()> {
        self.add_column_if_missing("notes", "due_ns", "INTEGER")?;
        self.add_column_if_missing("notes", "fired_at_ns", "INTEGER")?;
        self.add_column_if_missing("segments", "translation", "TEXT")?;
        self.add_column_if_missing("segments", "translation_via", "TEXT")?;
        self.conn.execute_batch(
            // The scheduler's query is "open notes with a due date that has
            // passed and have not fired", every thirty seconds, forever. This
            // is the covering index for exactly that.
            "CREATE INDEX IF NOT EXISTS idx_notes_due ON notes(due_ns, fired_at_ns, state);

             -- One digest per conversation, ever: `thread_id` is the key, and
             -- `day` is on the row rather than in the key because a
             -- conversation happens on one day and re-summarising it on the
             -- next would be a second paragraph about the same evening.
             CREATE TABLE IF NOT EXISTS digests (
                 thread_id   INTEGER PRIMARY KEY REFERENCES threads(id) ON DELETE CASCADE,
                 day         TEXT    NOT NULL,
                 lang        TEXT    NOT NULL,
                 summary     TEXT    NOT NULL,
                 people_json TEXT    NOT NULL,
                 open_json   TEXT    NOT NULL,
                 model_id    TEXT    NOT NULL,
                 created_ns  INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_digests_day ON digests(day, created_ns);

             -- The translation pass's queue: turns with words, a language, and
             -- no verdict yet. `translation_via IS NULL` is the queue and
             -- `translation IS NULL` is not — a turn the pass looked at and
             -- declined to translate is marked (see `mark_translation_declined`)
             -- so the worker does not walk it again every ten seconds.
             CREATE INDEX IF NOT EXISTS idx_segments_translation
                 ON segments(translation_via, t_start_ns);",
        )?;
        Ok(())
    }

    // ---- reminders ---------------------------------------------------------

    /// Notes that have come due: open, dated, not yet fired, `due_ns <= now`.
    ///
    /// Ordered oldest first so a daemon that was off overnight announces a
    /// backlog in the order the user asked for it, and bounded because a
    /// scheduler that can publish two hundred events in one tick is a
    /// scheduler that can hang up every client's outbox.
    pub fn notes_due(&self, now_utc_ns: i64, limit: usize) -> Result<Vec<NoteRow>> {
        let sql = format!(
            "SELECT {} WHERE n.state = ?1 AND n.due_ns IS NOT NULL
                        AND n.fired_at_ns IS NULL AND n.due_ns <= ?2
             ORDER BY n.due_ns ASC, n.id ASC LIMIT ?3",
            Self::NOTE_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        Ok(stmt
            .query_map(
                params![note_state::OPEN, now_utc_ns, limit as i64],
                Self::note_row_from,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Mark a note as announced. Returns whether this call was the one that did
    /// it — the guard that makes "fires once and never twice" true even if two
    /// ticks overlap, because the UPDATE only matches a row that is still
    /// unfired.
    pub fn mark_note_fired(&self, id: i64, at_utc_ns: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE notes SET fired_at_ns = ?2 WHERE id = ?1 AND fired_at_ns IS NULL",
            params![id, at_utc_ns],
        )? > 0)
    }

    /// Push a note's due date `minutes` into the future from `from_utc_ns`, and
    /// put it back on the scheduler's list.
    ///
    /// A snooze on a note with no due date **gives** it one, which is the only
    /// way a person can ask to be reminded of something they said without a
    /// time in it. Returns the row, or `None` when there is no such note.
    pub fn snooze_note(&self, id: i64, minutes: i64, from_utc_ns: i64) -> Result<Option<NoteRow>> {
        let due = from_utc_ns.saturating_add(minutes.max(1).saturating_mul(60_000_000_000));
        let n = self.conn.execute(
            "UPDATE notes SET due_ns = ?2, fired_at_ns = NULL, state = ?3 WHERE id = ?1",
            params![id, due, note_state::OPEN],
        )?;
        if n == 0 {
            return Ok(None);
        }
        self.note(id)
    }

    // ---- digests -----------------------------------------------------------

    /// Conversations the digest worker may consider: ended before
    /// `settled_before_ns`, at least `min_turns` turns with words in them, and
    /// no digest row.
    ///
    /// "Ended" is `threads.ended_ns`, which the threading rule advances with
    /// every turn — so a conversation still being spoken is simply not in this
    /// list yet, and a conversation that resumes after a digest was written
    /// keeps the digest it has. That is the honest failure of one digest per
    /// thread and it is the one worth having: the alternative is a paragraph
    /// that changes under a reader.
    pub fn threads_for_digest(
        &self,
        settled_before_ns: i64,
        min_turns: i64,
        limit: usize,
    ) -> Result<Vec<DigestCandidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.id, t.started_ns, t.ended_ns, COUNT(g.id) AS turns
             FROM threads t
             JOIN segments g ON g.thread_id = t.id
                            AND g.deleted_at IS NULL
                            AND g.text IS NOT NULL AND TRIM(g.text) <> ''
             LEFT JOIN digests d ON d.thread_id = t.id
             WHERE d.thread_id IS NULL AND t.ended_ns <= ?1
             GROUP BY t.id
             HAVING turns >= ?2
             ORDER BY t.ended_ns DESC
             LIMIT ?3",
        )?;
        Ok(stmt
            .query_map(params![settled_before_ns, min_turns, limit as i64], |r| {
                Ok(DigestCandidate {
                    thread_id: r.get(0)?,
                    started_ns: r.get(1)?,
                    ended_ns: r.get(2)?,
                    turns: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Write one digest. Replaces rather than fails: a re-run after a model
    /// change should produce the model's current answer, not an error about a
    /// row somebody already wrote.
    pub fn upsert_digest(&self, d: &DigestRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO digests
                 (thread_id, day, lang, summary, people_json, open_json, model_id,
                  created_ns, summary_raw, roster_json, rendered)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
             ON CONFLICT(thread_id) DO UPDATE SET
                 day = excluded.day, lang = excluded.lang, summary = excluded.summary,
                 people_json = excluded.people_json, open_json = excluded.open_json,
                 model_id = excluded.model_id, created_ns = excluded.created_ns,
                 summary_raw = excluded.summary_raw, roster_json = excluded.roster_json,
                 rendered = excluded.rendered",
            params![
                d.thread_id,
                d.day,
                d.lang,
                d.summary,
                d.people_json,
                d.open_json,
                d.model_id,
                d.created_ns,
                d.summary_raw,
                d.roster_json,
                d.rendered
            ],
        )?;
        Ok(())
    }

    /// 0.11.6: a digest says people's names, so the row has to carry what the
    /// model wrote and who the letters were.
    ///
    /// Three nullable columns and no backfill. A row from before this change
    /// has `summary_raw IS NULL`, and that is exactly the fact
    /// [`crate::digest::digest_json`] keys the legacy read-time rendering off —
    /// there is nothing to guess and no model call to make.
    fn apply_digest_names(&self) -> Result<()> {
        self.add_column_if_missing("digests", "summary_raw", "TEXT")?;
        self.add_column_if_missing("digests", "roster_json", "TEXT")?;
        self.add_column_if_missing("digests", "rendered", "TEXT")?;
        Ok(())
    }

    /// Record that a conversation was read and found not worth summarising.
    ///
    /// A refusal is a result and has to be written down, or the worker asks the
    /// model about the same eight "ja"s every ten seconds for the rest of the
    /// evening. An empty summary is the marker; `digest_rows` filters them out,
    /// so a refusal is invisible to a client and permanent to the worker.
    pub fn mark_thread_not_worth_summarising(
        &self,
        thread_id: i64,
        model_id: &str,
        at_utc_ns: i64,
    ) -> Result<()> {
        self.upsert_digest(&DigestRow {
            thread_id,
            day: String::new(),
            lang: String::new(),
            summary: String::new(),
            people_json: "[]".into(),
            open_json: "[]".into(),
            summary_raw: None,
            roster_json: None,
            rendered: None,
            model_id: model_id.to_string(),
            created_ns: at_utc_ns,
        })
    }

    /// Digests with something in them, newest conversation first. `day` narrows
    /// to one local calendar day.
    pub fn digest_rows(&self, day: Option<&str>, limit: usize) -> Result<Vec<DigestRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.thread_id, d.day, d.lang, d.summary, d.people_json, d.open_json,
                    d.model_id, d.created_ns, d.summary_raw, d.roster_json, d.rendered
             FROM digests d
             JOIN threads t ON t.id = d.thread_id
             WHERE TRIM(d.summary) <> '' AND (?1 IS NULL OR d.day = ?1)
             ORDER BY t.ended_ns DESC, d.thread_id DESC
             LIMIT ?2",
        )?;
        Ok(stmt
            .query_map(params![day, limit as i64], |r| {
                Ok(DigestRow {
                    thread_id: r.get(0)?,
                    day: r.get(1)?,
                    lang: r.get(2)?,
                    summary: r.get(3)?,
                    people_json: r.get(4)?,
                    open_json: r.get(5)?,
                    model_id: r.get(6)?,
                    created_ns: r.get(7)?,
                    summary_raw: r.get(8)?,
                    roster_json: r.get(9)?,
                    rendered: r.get(10)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// How many conversations have been read and how many are still waiting —
    /// `(with a summary, refused, pending)`. Read by `status`.
    pub fn digest_counts(&self, settled_before_ns: i64, min_turns: i64) -> Result<(i64, i64, i64)> {
        let written: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM digests WHERE TRIM(summary) <> ''",
            [],
            |r| r.get(0),
        )?;
        let refused: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM digests WHERE TRIM(summary) = ''",
            [],
            |r| r.get(0),
        )?;
        let pending = self
            .threads_for_digest(settled_before_ns, min_turns, usize::MAX >> 32)?
            .len() as i64;
        Ok((written, refused, pending))
    }

    // ---- translation -------------------------------------------------------

    /// Turns waiting for a translation into `to`: stamped with some other
    /// language, not spoken by one of `mine`, with words, and not yet looked at.
    ///
    /// `mine` is the languages the user's own voice speaks — a turn in a
    /// language you already read is not a turn you need translated, and asking
    /// the model anyway would be spending a model call to produce a copy.
    pub fn segments_for_translation(
        &self,
        to: &str,
        mine: &[String],
        min_words: usize,
        limit: usize,
    ) -> Result<Vec<TranslateCandidate>> {
        // `mine` is a small set of two-letter tags; an IN list built here is
        // bounded by the number of languages a person can declare.
        let skip: Vec<String> = std::iter::once(to.to_string())
            .chain(mine.iter().cloned())
            .collect();
        let holes = (0..skip.len())
            .map(|i| format!("?{}", i + 3))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT g.id, g.text, g.lang FROM segments g
             WHERE g.deleted_at IS NULL AND g.translation_via IS NULL
               AND g.lang IS NOT NULL AND g.lang NOT IN ({holes})
               AND g.text IS NOT NULL AND TRIM(g.text) <> ''
             ORDER BY g.t_start_ns DESC LIMIT ?1"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut args: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(limit as i64), Box::new(min_words as i64)];
        for s in &skip {
            args.push(Box::new(s.clone()));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt
            .query_map(refs.as_slice(), |r| {
                Ok(TranslateCandidate {
                    id: r.get(0)?,
                    text: r.get(1)?,
                    lang: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // The word floor is applied here rather than in SQL: SQLite cannot
        // count words, and a LIKE-based approximation would be a second,
        // different definition of "three words" from the one the pass uses.
        Ok(rows
            .into_iter()
            .filter(|c| crate::asr::normalise_words(&c.text).len() >= min_words)
            .collect())
    }

    /// Turns nothing has read a language out of, newest first (0.10.2).
    ///
    /// The companion to [`Self::segments_for_translation`], which can only see
    /// rows that already carry a `de`/`en` stamp. A French turn carries none —
    /// the classifier has no vocabulary for it — so it is invisible to that
    /// query and to every earlier version of this feature. These are the rows
    /// `crate::lang::guess_other` is offered, outside the lock.
    ///
    /// `limit` is a scan window rather than a batch size: most of what comes
    /// back is genuinely unreadable (a mumble, two words, a name) and will be
    /// rejected by the guesser rather than translated. The caller passes
    /// something in the low thousands and takes its batch out of the far end.
    pub fn segments_without_language(
        &self,
        min_words: usize,
        limit: usize,
    ) -> Result<Vec<TranslateCandidate>> {
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.text FROM segments g
             WHERE g.deleted_at IS NULL AND g.translation_via IS NULL
               AND g.lang IS NULL
               AND g.text IS NOT NULL AND TRIM(g.text) <> ''
             ORDER BY g.t_start_ns DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok(TranslateCandidate {
                    id: r.get(0)?,
                    text: r.get(1)?,
                    lang: String::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter(|c| crate::asr::normalise_words(&c.text).len() >= min_words)
            .collect())
    }

    /// Store a translation of one turn.
    pub fn set_segment_translation(
        &self,
        segment_id: i64,
        translation: &str,
        model_id: &str,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE segments SET translation = ?2, translation_via = ?3
             WHERE id = ?1 AND deleted_at IS NULL",
            params![segment_id, translation, model_id],
        )?;
        Ok(())
    }

    /// Record that the pass looked at a turn and decided against translating
    /// it — the model echoed the input, or answered in the wrong language.
    ///
    /// `translation_via` is set and `translation` left NULL, which is what
    /// keeps the queue finite: NULL/NULL means "nothing has looked", and this
    /// row has been looked at.
    pub fn mark_translation_declined(&self, segment_id: i64, model_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE segments SET translation = NULL, translation_via = ?2
             WHERE id = ?1 AND deleted_at IS NULL",
            params![segment_id, model_id],
        )?;
        Ok(())
    }

    /// A turn whose words changed has a translation of words nobody said any
    /// more. Clearing both columns puts it back at the end of the queue.
    pub fn clear_segment_translation(&self, segment_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE segments SET translation = NULL, translation_via = NULL WHERE id = ?1",
            params![segment_id],
        )?;
        Ok(())
    }

    /// `(translated, declined)` — the two halves of what the pass has done.
    pub fn translation_counts(&self) -> Result<(i64, i64)> {
        Ok(self.conn.query_row(
            "SELECT COUNT(translation),
                    SUM(CASE WHEN translation_via IS NOT NULL AND translation IS NULL
                             THEN 1 ELSE 0 END)
             FROM segments WHERE deleted_at IS NULL",
            [],
            |r| Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0))),
        )?)
    }

    /// The languages the user's own voice is declared to speak, for the
    /// translation pass's "a turn you can already read" rule. Empty when there
    /// is no pinned voice or it has no declaration — which is the common state,
    /// and means only `translate_to` itself is skipped.
    pub fn your_languages(&self) -> Result<Vec<String>> {
        let Some(you) = self.you_speaker_id()? else {
            return Ok(Vec::new());
        };
        Ok(self.speaker_languages(you)?.unwrap_or_default())
    }
}

// ---- end 0.9.0 ------------------------------------------------------------

// ---- 0.10.1: the cross-check's verdicts, counted -------------------------

/// How many rows carry each cross-check verdict, per source and per voice —
/// the unbiased half of `accuracy.summary`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfidenceCount {
    pub source: String,
    pub speaker_id: Option<i64>,
    /// `"solid"`, `"shaky"`, or `None` for checked-with-no-verdict.
    pub confidence: Option<String>,
    pub n: i64,
}

impl Store {
    /// Verdict counts over every live row that has been checked, grouped the
    /// same way `segment_row` resolves a row: the source's match key and the
    /// voice after merges.
    pub fn confidence_counts(&self) -> Result<Vec<ConfidenceCount>> {
        let mut stmt = self.conn.prepare(
            "SELECT sc.match_key, sp.canonical_id, g.asr_confidence, COUNT(*)
             FROM segments g
             JOIN sessions ss ON ss.id = g.session_id
             JOIN sources sc ON sc.id = ss.source_id
             LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
             WHERE g.deleted_at IS NULL AND g.confidence_at_ns IS NOT NULL
             GROUP BY sc.match_key, sp.canonical_id, g.asr_confidence",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ConfidenceCount {
                    source: r.get(0)?,
                    speaker_id: r.get(1)?,
                    confidence: r.get(2)?,
                    n: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

// ---- end 0.10.1 -----------------------------------------------------------
// ---- 0.11.0: source-aware identity ----------------------------------------
//
// Everything below the banner is additive: one index, four read queries and
// one narrow UPDATE. Nothing above it reads any of this, and deleting the block
// would leave a working 0.10.0 store behind.
//
// **Why a query and not a counter table.** A maintained `speaker_sources`
// counter would be a fifth thing every relabelling path has to remember —
// `merge_speakers`, `split`, `set_segment_speaker`, `set_segment_speaker_via`,
// proximity inheritance, `speakers.prune`, delete-by-speaker, the truth pass's
// enrolment and the retention sweeper all move or remove rows that would have
// to be counted — and a counter that drifts is worse than no counter, because
// the prior would then refuse voices on the strength of a number nobody can
// see is wrong. The query is a grouped join over `segments.speaker_id`, which
// is already indexed, and this install's whole `segments` table is under ten
// thousand rows after two months of daily capture. The covering index below
// makes the per-speaker case index-only up to the session join.

/// One row of a voice's source history: where it has been heard, how often,
/// and when last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeakerSource {
    pub source_id: i64,
    /// The stable identifier — an application's PE/executable name, or the
    /// literal `mic` / `room`. This is what the prior keys on and what a client
    /// should render as the chip's identity.
    pub match_key: String,
    pub display_name: String,
    /// `app`, `mic` or `room` (`crate::store::source_kind`).
    pub kind: String,
    pub segments: i64,
    /// When this voice was last heard on this source.
    pub last_ns: i64,
}

/// What the prior needs to know about one voice's standing on one source.
/// Deliberately not [`SpeakerSource`]: this is two counts, for every voice at
/// once, and it is asked per segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceStanding {
    pub speaker_id: i64,
    pub on_source: i64,
    pub total: i64,
}

/// One labelled turn, in the shape the audit's chronological replay needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct LabelledSegment {
    pub id: i64,
    pub speaker_id: i64,
    pub source_id: i64,
    pub match_score: Option<f64>,
    pub label_via: Option<String>,
    pub t_start_ns: i64,
}

/// The source a segment was captured from, with the span the presence rules
/// look around.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentSource {
    pub segment_id: i64,
    pub source_id: i64,
    pub match_key: String,
    pub display_name: String,
    pub kind: String,
    pub t_start_ns: i64,
    pub t_end_ns: i64,
}

impl Store {
    /// The covering index the source matrix is read through. Idempotent and
    /// additive, so it needs no schema version of its own — it is an index,
    /// and an index is not a shape.
    pub(crate) fn apply_source_prior_index(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_segments_speaker_session
                 ON segments(speaker_id, session_id);",
        )?;
        Ok(())
    }

    /// Where one voice has been heard, most turns first.
    ///
    /// Live rows only: a soft-deleted turn has left every other read path and
    /// must not keep a chip alive on the Speakers list either.
    pub fn speaker_sources(&self, speaker_id: i64) -> Result<Vec<SpeakerSource>> {
        let mut stmt = self.conn.prepare(
            "SELECT sc.id, sc.match_key, sc.display_name, sc.kind,
                    COUNT(g.id), MAX(g.t_end_ns)
             FROM segments g
             JOIN sessions ss ON ss.id = g.session_id
             JOIN sources  sc ON sc.id = ss.source_id
             WHERE g.speaker_id = ?1 AND g.deleted_at IS NULL
             GROUP BY sc.id
             ORDER BY 5 DESC, sc.id ASC",
        )?;
        Ok(stmt
            .query_map(params![speaker_id], Self::speaker_source_from)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The whole voice × source matrix in one query, keyed by voice.
    ///
    /// One query rather than one per voice: `speakers.list` renders every row
    /// at once and the audit prints the matrix, so the N+1 shape would be the
    /// only expensive thing on either path.
    pub fn speaker_source_matrix(&self) -> Result<HashMap<i64, Vec<SpeakerSource>>> {
        let mut stmt = self.conn.prepare(
            "SELECT g.speaker_id, sc.id, sc.match_key, sc.display_name, sc.kind,
                    COUNT(g.id), MAX(g.t_end_ns)
             FROM segments g
             JOIN sessions ss ON ss.id = g.session_id
             JOIN sources  sc ON sc.id = ss.source_id
             WHERE g.speaker_id IS NOT NULL AND g.deleted_at IS NULL
             GROUP BY g.speaker_id, sc.id
             ORDER BY g.speaker_id ASC, 6 DESC, sc.id ASC",
        )?;
        let mut out: HashMap<i64, Vec<SpeakerSource>> = HashMap::new();
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    SpeakerSource {
                        source_id: r.get(1)?,
                        match_key: r.get(2)?,
                        display_name: r.get(3)?,
                        kind: r.get(4)?,
                        segments: r.get(5)?,
                        last_ns: r.get(6)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (speaker_id, row) in rows {
            out.entry(speaker_id).or_default().push(row);
        }
        Ok(out)
    }

    fn speaker_source_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<SpeakerSource> {
        Ok(SpeakerSource {
            source_id: r.get(0)?,
            match_key: r.get(1)?,
            display_name: r.get(2)?,
            kind: r.get(3)?,
            segments: r.get(4)?,
            last_ns: r.get(5)?,
        })
    }

    /// Every voice's standing against one source: turns there, turns anywhere.
    ///
    /// Asked once per segment analysed, which is why it is a single grouped
    /// scan rather than a lookup per candidate — a voicebank of thirty voices
    /// would otherwise be thirty round trips inside the store mutex.
    pub fn source_standings(&self, source_id: i64) -> Result<Vec<SourceStanding>> {
        let mut stmt = self.conn.prepare(
            "SELECT g.speaker_id,
                    SUM(CASE WHEN ss.source_id = ?1 THEN 1 ELSE 0 END),
                    COUNT(g.id)
             FROM segments g
             JOIN sessions ss ON ss.id = g.session_id
             WHERE g.speaker_id IS NOT NULL AND g.deleted_at IS NULL
             GROUP BY g.speaker_id",
        )?;
        Ok(stmt
            .query_map(params![source_id], |r| {
                Ok(SourceStanding {
                    speaker_id: r.get(0)?,
                    on_source: r.get(1)?,
                    total: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Which source a segment came from, and the span the presence rules look
    /// either side of.
    pub fn segment_source(&self, segment_id: i64) -> Result<Option<SegmentSource>> {
        Ok(self
            .conn
            .query_row(
                "SELECT sc.id, sc.match_key, sc.display_name, sc.kind,
                        g.t_start_ns, g.t_end_ns
                 FROM segments g
                 JOIN sessions ss ON ss.id = g.session_id
                 JOIN sources  sc ON sc.id = ss.source_id
                 WHERE g.id = ?1",
                params![segment_id],
                |r| {
                    Ok(SegmentSource {
                        segment_id,
                        source_id: r.get(0)?,
                        match_key: r.get(1)?,
                        display_name: r.get(2)?,
                        kind: r.get(3)?,
                        t_start_ns: r.get(4)?,
                        t_end_ns: r.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    /// The Discord accounts a voice is linked to. Empty for a voice nothing has
    /// linked, which is the state the hard presence rule refuses to act on.
    pub fn discord_user_ids_for_speaker(&self, speaker_id: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.user_id FROM discord_users d
             JOIN speaker_resolved sp ON sp.id = d.speaker_id
             WHERE sp.canonical_id = ?1",
        )?;
        Ok(stmt
            .query_map(params![speaker_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Which Discord accounts spoke at all in `[from_ns, to_ns)`.
    ///
    /// The *set*, not the spans: the hard rule asks one yes/no question per
    /// account and a list of ids answers it without materialising every ring
    /// Discord drew in ten minutes.
    pub fn discord_users_speaking_between(
        &self,
        from_ns: i64,
        to_ns: i64,
    ) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT user_id FROM truth_speaking
             WHERE t_start_ns < ?2 AND COALESCE(t_end_ns, ?2) > ?1",
        )?;
        Ok(stmt
            .query_map(params![from_ns, to_ns], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?)
    }

    /// Which display names the VRChat roster had present in `[from_ns, to_ns)`.
    ///
    /// An open row (nobody has left) counts as present through the window's
    /// end, which is the same reading `roster_intervals` takes.
    pub fn roster_names_between(&self, from_ns: i64, to_ns: i64) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT display_name FROM session_roster
             WHERE joined_at_utc_ns < ?2 AND COALESCE(left_at_utc_ns, ?2) > ?1",
        )?;
        Ok(stmt
            .query_map(params![from_ns, to_ns], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?)
    }

    /// The name a voice would be looked up in the roster by, or `None` when it
    /// has never been named. Resolved through the tombstone view, like
    /// `roster_intervals`, so a merged-away id answers for the surviving voice.
    pub fn named_speaker_name(&self, speaker_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT s.display_name FROM speakers s
                 JOIN speaker_resolved sp ON sp.canonical_id = s.id
                 WHERE sp.id = ?1 AND s.named_at IS NOT NULL",
                params![speaker_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// One labelled turn, as the audit's chronological replay reads it.
    pub fn labelled_segments_in_order(&self) -> Result<Vec<LabelledSegment>> {
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.speaker_id, ss.source_id, g.match_score, g.label_via, g.t_start_ns
             FROM segments g
             JOIN sessions ss ON ss.id = g.session_id
             WHERE g.speaker_id IS NOT NULL AND g.deleted_at IS NULL
             ORDER BY g.t_start_ns ASC, g.id ASC",
        )?;
        Ok(stmt
            .query_map([], |r| {
                Ok(LabelledSegment {
                    id: r.get(0)?,
                    speaker_id: r.get(1)?,
                    source_id: r.get(2)?,
                    match_score: r.get(3)?,
                    label_via: r.get(4)?,
                    t_start_ns: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every capture source, by id — the audit's lookup table.
    pub fn sources_by_id(&self) -> Result<HashMap<i64, (String, String, String)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, match_key, display_name, kind FROM sources")?;
        Ok(stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    (
                        r.get(1)?,
                        r.get(2)?,
                        r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    ),
                ))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    /// Take one label back to *unassigned*: the only write `identity repair`
    /// is allowed to make.
    ///
    /// It never re-points a row at another voice. The audit's finding is "this
    /// label is not supported by where the audio came from", which is an
    /// argument against the label it has and not for any other one — and a
    /// sweep that guesses again in bulk is how a bad label becomes a hundred.
    /// The embedding stays, so the row remains evidence for a later reassign.
    pub fn unassign_segment_speaker(&self, segment_id: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE segments SET speaker_id = NULL, match_score = NULL, label_via = NULL
             WHERE id = ?1 AND deleted_at IS NULL",
            params![segment_id],
        )? > 0)
    }
}

// ---- end 0.11.0 -----------------------------------------------------------

// ---- 0.11.0: learned identity ----------------------------------------------
//
// Where the learned operating point lives, and why here rather than in the
// config file.
//
// A threshold fitted from *this* install's ground truth is derived data with
// provenance — which voice, how many turns, when, by what. A config file is
// the user's opinion and nothing may quietly rewrite it; the database is
// already where every other derived-and-refittable thing lives (prototypes,
// the source matrix, the language votes). Putting it here also means
// `speakers.threshold` travels with the voice through a merge, a rename and a
// backup, and that `recalld identity calibrate --reset` is one UPDATE rather
// than an edit to a file somebody may have hand-annotated.
//
// Everything is nullable and absence means the global. A store where nothing
// has ever been calibrated is byte-for-byte a 0.10.2 store.

/// One voice's learned operating point, with everything needed to explain it.
#[derive(Debug, Clone, PartialEq)]
pub struct LearnedThreshold {
    pub speaker_id: i64,
    pub threshold: f32,
    pub margin: f32,
    /// `store::truth_via` — `learned` for a fit, or whatever a future hand
    /// override calls itself.
    pub via: String,
    /// Truth rows the fit saw for this voice. The number a client should show
    /// as "calibrated on N turns".
    pub n: i64,
    pub at_ns: i64,
}

// How many times this thread has loaded the whole calibration corpus.
//
// Test instrument, and it earns its keep: the thing worth asserting about the
// nightly fit's rate limit is not what it returns but that it stops the
// **expensive** half from running, and that is invisible from the outside.
// Thread-local rather than a global atomic so two tests fitting at once cannot
// read each other's counts.
#[cfg(test)]
thread_local! {
    pub static CALIBRATION_ROW_LOADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One truth-labelled turn with its embedding, in the shape a calibration fit
/// needs it. The same rows the bench reads, through the same door.
#[derive(Debug, Clone)]
pub struct CalibrationRow {
    pub segment_id: i64,
    pub t_start_ns: i64,
    pub overlap_frac: f32,
    pub duration_s: f32,
    pub words: usize,
    /// The voice Discord's ground truth says this is.
    pub truth_speaker_id: i64,
    pub embedding: Embedding,
}

/// A run of voices minted from one source in a short window (0.12.2).
///
/// The signature of the failure FINDINGS §46 documents: a per-voice label bar
/// fitted above a voice's typical own-turn score turns that voice's turns into
/// mints, each mint seeds a phantom from a fresh recording of the same person,
/// and a same-evening recording outscores an older bank — so the next turn
/// matches the phantom or mints another. Twenty in thirty-three minutes on
/// 2026-09-04.
#[derive(Debug, Clone, PartialEq)]
pub struct MintBurst {
    /// The capture source every mint in the run came from.
    pub source_key: String,
    pub first_ns: i64,
    pub last_ns: i64,
    /// `(speaker id, name, seed segment)`, in the order they were minted.
    pub minted: Vec<(i64, String, Option<i64>)>,
    /// What Discord says the seed turns were: `(voice, name, how many)`,
    /// commonest first. A burst whose seeds all name one existing voice is the
    /// cascade; one with no verdicts at all is a room filling up with strangers
    /// and is reported without the accusation.
    pub seeds_say: Vec<(i64, String, usize)>,
    pub seeds_with_a_verdict: usize,
    /// Where the run's voices ended up, for the ones somebody has already
    /// merged away: `(voice, name, how many)`, commonest first. A burst whose
    /// members were all folded into one voice by hand is the same finding
    /// arriving from the other end, and it is the only evidence a burst leaves
    /// once the merge has taken its rows away.
    pub merged_into: Vec<(i64, String, usize)>,
}

/// A voice that is somebody the bank already knew, split off under a number
/// (0.12.2). What `identity repair --phantoms` merges away.
#[derive(Debug, Clone, PartialEq)]
pub struct PhantomVoice {
    pub speaker_id: i64,
    pub name: String,
    /// Prototypes it still holds, and how many of those ground truth condemns.
    pub prototypes: usize,
    pub condemned: usize,
    /// Its live rows, and how many carry a Discord `single` verdict.
    pub rows: usize,
    pub covered: usize,
    /// `(voice, name, rows)` for every voice its covered rows name, commonest
    /// first.
    pub says: Vec<(i64, String, usize)>,
    /// The voice it would be merged into, when one holds a strict majority.
    pub target: Option<(i64, String)>,
    /// Why it is not a candidate, when it is not.
    pub refused: Option<String>,
}

/// One row's speaker as it was before a bulk correction touched it (0.12.2).
///
/// Written into `operations` before the write, which is the whole of what
/// "reversible" means here: putting a phantom's turns back is replaying this
/// list.
#[derive(Debug, Clone, PartialEq)]
pub struct PriorLabel {
    pub segment_id: i64,
    pub speaker_id: Option<i64>,
    pub label_via: Option<String>,
    pub match_score: Option<f64>,
}

/// A prototype ground truth says is a recording of somebody else (0.12.0).
#[derive(Debug, Clone, PartialEq)]
pub struct CondemnedPrototype {
    pub prototype_id: i64,
    /// The voice it is filed under.
    pub owner: i64,
    pub owner_name: String,
    /// The voice Discord's verdict says was actually talking.
    pub truth_speaker: i64,
    pub truth_name: String,
    pub source_segment_id: i64,
    pub coverage: f64,
    pub created_at: i64,
}

impl Store {
    /// Schema v15: a voice can be highlighted.
    ///
    /// Two nullable columns and nothing else. `colour` holds a palette *token*
    /// (`crate::palette::PALETTE`) rather than a hex string, so that the three
    /// renderers — light ground, dark ground, and the headset overlay's own
    /// rasteriser — can each paint it at the saturation and lightness their
    /// surface was measured at. A stored `#111111` would be a highlight that
    /// makes a name harder to read, which is the opposite of the feature.
    ///
    /// The column is deliberately NOT constrained to the palette in SQL. A
    /// build that drops a token would otherwise fail to *open* a database that
    /// still had it, turning a cosmetic setting into a startup error; instead
    /// an unknown token reads back as "no highlight" and the row is left alone
    /// until somebody sets it again. Validation lives on the write path, where
    /// it can say something useful to the person typing.
    fn apply_v15(&self) -> Result<()> {
        self.add_column_if_missing("speakers", "colour", "TEXT")?;
        self.add_column_if_missing("speakers", "icon", "TEXT")?;
        // `speaker_resolved` is how every segment gets its speaker's name, and
        // it now has to carry the highlight for the same reason: a merged-away
        // voice must answer with the SURVIVING voice's colour, not with the one
        // it wore before somebody decided the two were the same person.
        //
        // Dropped and rebuilt rather than created-if-missing, because the view
        // already exists on every database this migration will ever run
        // against — `apply_v2` made it — and `CREATE VIEW IF NOT EXISTS` would
        // therefore do nothing at all, leaving a three-column view under a
        // five-column query. A view holds no rows, so dropping one costs
        // nothing and loses nothing.
        self.conn.execute_batch(
            "DROP VIEW IF EXISTS speaker_resolved;
             CREATE VIEW speaker_resolved AS
                 SELECT s.id                                AS id,
                        COALESCE(t.id, s.id)                AS canonical_id,
                        COALESCE(t.display_name, s.display_name) AS display_name,
                        COALESCE(t.colour, s.colour)        AS colour,
                        COALESCE(t.icon, s.icon)            AS icon
                 FROM speakers s
                 LEFT JOIN speakers t ON t.id = s.merged_into;",
        )?;
        Ok(())
    }

    /// Schema v16: when the archive language sweep was last at a row.
    ///
    /// One nullable column, and it exists because 0.11.9 tried to answer two
    /// questions with one: `lang_via = 'sweep'` was written both as a real
    /// provenance (with `lang` set to `de`/`en`) and as a bare "asked, nothing
    /// to say" mark with `lang` still NULL — and the rows the *text* pre-filter
    /// declined got neither, deliberately, so that a declaration added tomorrow
    /// could still reach them.
    ///
    /// On the live install that made the sweep unfinishable. 1,729 of 1,782
    /// rows are declined by the pre-filter for free, and with nothing written
    /// about them they were owed again on every run: `recalld lang sweep
    /// --apply` printed the same "1730 still owed" every time and the nightly
    /// pass re-walked all of them every night. The bookkeeping question ("has
    /// this pass been here") and the provenance question ("how did this row get
    /// its language") are not the same question, and this column is the first
    /// one — the same shape, and the same name, as `night_at_ns`,
    /// `confidence_at_ns` and `redecode_at_ns`, which are the three other
    /// passes that had to answer it.
    ///
    /// Nothing but `crate::sweep` reads it, which is the property that lets it
    /// mark a row without blocking any future decision about that row: the
    /// live path never looks, and the mark is cleared the moment either input
    /// to the pre-filter moves (see [`Self::clear_segment_sweep`] and
    /// [`Self::clear_speaker_sweep`]).
    ///
    /// **The backfill is a rename.** Every row carrying the bare mark — the
    /// `lang_via = 'sweep'` with a NULL `lang` that 0.11.9 wrote — is exactly a
    /// row this column is now for, so it gets a timestamp and gives the
    /// `lang_via` back. Zero is used rather than "now": the honest answer to
    /// *when* is "before this migration", and a real clock reading would claim
    /// the sweep ran at upgrade time. Rows with the stamp shape (`lang` set)
    /// keep their `lang_via` untouched — that one is a provenance and always
    /// was.
    fn apply_v16(&self) -> Result<()> {
        self.add_column_if_missing("segments", "sweep_at_ns", "INTEGER")?;
        self.conn.execute(
            "UPDATE segments SET sweep_at_ns = 0, lang_via = NULL
             WHERE lang_via = ?1 AND lang IS NULL",
            params![lang_via::SWEEP],
        )?;
        self.conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_segments_sweep
                 ON segments(sweep_at_ns, lang, t_start_ns);",
        )?;
        Ok(())
    }

    /// Set or clear one voice's highlight.
    ///
    /// `None` clears; `Some` sets. The caller decides which of those an absent
    /// parameter means — `speakers.set` leaves an omitted key alone and clears
    /// an explicit null, and that distinction cannot be made once both have
    /// collapsed into an `Option` here.
    ///
    /// Writes `id` directly rather than through `speaker_resolved`, and the
    /// caller is expected to have refused a tombstone first: this is the
    /// mirror-image of the `set_speaker_languages` trap (a read that resolves
    /// through the tombstone and a write that does not), and the guard lives at
    /// the same place for the same reason.
    pub fn set_speaker_style(
        &self,
        speaker_id: i64,
        colour: Option<&str>,
        icon: Option<&str>,
    ) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE speakers SET colour = ?2, icon = ?3 WHERE id = ?1",
            params![speaker_id, colour, icon],
        )?;
        if n == 0 {
            bail!("no speaker with id {speaker_id}");
        }
        Ok(())
    }

    /// One voice's highlight, resolved through the tombstone view.
    pub fn speaker_style(
        &self,
        speaker_id: i64,
    ) -> Result<Option<(Option<String>, Option<String>)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT sp.colour, sp.icon FROM speaker_resolved sp WHERE sp.id = ?1",
                params![speaker_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    /// Every highlighted voice, by id, resolved through the tombstone view.
    ///
    /// One query for the whole table rather than one per name, because the
    /// surfaces that need this — a thread's participants, a person's edges, a
    /// digest's roster — render a dozen names at once and an N+1 here would be
    /// the only expensive thing on the page. Same shape and same reasoning as
    /// `speaker_source_matrix`.
    ///
    /// Only highlighted voices are in the map. "Not in the map" is the answer
    /// for everybody else, which is also what an older database says for
    /// everybody.
    pub fn speaker_styles(&self) -> Result<SpeakerStyles> {
        let mut stmt = self.conn.prepare(
            "SELECT sp.id, sp.colour, sp.icon FROM speaker_resolved sp
             WHERE sp.colour IS NOT NULL OR sp.icon IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    (
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ),
                ))
            })?
            .collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()?;
        Ok(rows)
    }

    /// The learned-identity shape. Idempotent, additive, no backfill.
    pub(crate) fn apply_learned_identity(&self) -> Result<()> {
        self.add_column_if_missing("speakers", "label_threshold", "REAL")?;
        self.add_column_if_missing("speakers", "label_margin", "REAL")?;
        self.add_column_if_missing("speakers", "threshold_via", "TEXT")?;
        self.add_column_if_missing("speakers", "threshold_n", "INTEGER")?;
        self.add_column_if_missing("speakers", "threshold_at", "INTEGER")?;
        // One row, enforced by the primary key rather than by convention: a
        // second projection is not a variant, it is a bug that would make
        // "which space is live" a question with two answers.
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS identity_projection (
                 id             INTEGER PRIMARY KEY CHECK (id = 1),
                 embed_model_id TEXT    NOT NULL,
                 dim            INTEGER NOT NULL,
                 matrix         BLOB    NOT NULL,
                 n_rows         INTEGER NOT NULL,
                 n_classes      INTEGER NOT NULL,
                 shrinkage      REAL    NOT NULL,
                 power          REAL    NOT NULL,
                 centred        INTEGER NOT NULL,
                 version        INTEGER NOT NULL,
                 fitted_at_ns   INTEGER NOT NULL
             );",
        )?;
        Ok(())
    }

    /// Every learned threshold, resolved through the tombstone view so a
    /// merged-away voice answers for the one that survived it.
    pub fn learned_thresholds(&self) -> Result<Vec<LearnedThreshold>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, label_threshold, label_margin, threshold_via,
                    COALESCE(threshold_n, 0), COALESCE(threshold_at, 0)
             FROM speakers
             WHERE merged_into IS NULL AND label_threshold IS NOT NULL
             ORDER BY id ASC",
        )?;
        Ok(stmt
            .query_map([], |r| {
                Ok(LearnedThreshold {
                    speaker_id: r.get(0)?,
                    threshold: r.get::<_, f64>(1)? as f32,
                    margin: r.get::<_, Option<f64>>(2)?.unwrap_or(0.0) as f32,
                    via: r
                        .get::<_, Option<String>>(3)?
                        .unwrap_or_else(|| truth_via::LEARNED.to_string()),
                    n: r.get(4)?,
                    at_ns: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The table the ladder asks, with `global` standing in for every voice
    /// nothing has been learned about.
    pub fn threshold_table(&self, global: (f32, f32)) -> Result<crate::calib::Thresholds> {
        let mut t = crate::calib::Thresholds::global(global.0, global.1);
        for row in self.learned_thresholds()? {
            t.insert(row.speaker_id, row.threshold, row.margin);
        }
        Ok(t)
    }

    /// Write one voice's learned point.
    pub fn set_learned_threshold(
        &self,
        speaker_id: i64,
        threshold: f32,
        margin: f32,
        via: &str,
        n: i64,
        at_ns: i64,
    ) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE speakers
                SET label_threshold = ?2, label_margin = ?3,
                    threshold_via = ?4, threshold_n = ?5, threshold_at = ?6
              WHERE id = ?1",
            params![speaker_id, threshold as f64, margin as f64, via, n, at_ns],
        )? > 0)
    }

    /// Back to the globals. `None` clears every voice; a list clears those.
    ///
    /// The count is voices that actually *had* a learned value — an UPDATE
    /// that nulls a column already NULL touches a row without changing
    /// anything, and reporting that as "cleared 33 voices" on an install where
    /// nothing was ever learned is a number that lies.
    pub fn clear_learned_thresholds(&self, only: Option<&[i64]>) -> Result<usize> {
        const SQL: &str = "UPDATE speakers
             SET label_threshold = NULL, label_margin = NULL,
                 threshold_via = NULL, threshold_n = NULL, threshold_at = NULL
             WHERE label_threshold IS NOT NULL";
        Ok(match only {
            None => self.conn.execute(SQL, [])?,
            Some(ids) => {
                let mut n = 0;
                let mut stmt = self.conn.prepare(&format!("{SQL} AND id = ?1"))?;
                for id in ids {
                    n += stmt.execute(params![id])?;
                }
                n
            }
        })
    }

    /// The installed projection, if there is one, and its provenance.
    pub fn installed_projection(&self) -> Result<Option<(crate::calib::Projection, i64, i64)>> {
        /// `(model, dim, matrix, rows, classes, shrinkage, power, centred,
        /// version, fitted at)` — one row of `identity_projection`, named so
        /// the tuple does not have to be read twice.
        type Row = (String, i64, Vec<u8>, i64, i64, f64, f64, i64, i64, i64);
        let row: Option<Row> = self
            .conn
            .query_row(
                "SELECT embed_model_id, dim, matrix, n_rows, n_classes,
                        shrinkage, power, centred, version, fitted_at_ns
                 FROM identity_projection WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                    ))
                },
            )
            .optional()?;
        let Some((model, dim, blob, n_rows, n_classes, shrinkage, power, centred, ver, at)) = row
        else {
            return Ok(None);
        };
        let mut p = crate::calib::Projection::from_blob(model, dim as usize, &blob)?;
        p.n_rows = n_rows as usize;
        p.n_classes = n_classes as usize;
        p.whitening = crate::calib::Whitening {
            shrinkage,
            power,
            centre: centred != 0,
        };
        Ok(Some((p, ver, at)))
    }

    /// Install a projection, replacing whatever was there.
    ///
    /// The *decision* to replace belongs to `identity_learn`, which measures
    /// the incumbent against the candidate first; this is the write it makes
    /// once it has.
    pub fn install_projection(
        &self,
        p: &crate::calib::Projection,
        version: i64,
        at_ns: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO identity_projection
                 (id, embed_model_id, dim, matrix, n_rows, n_classes,
                  shrinkage, power, centred, version, fitted_at_ns)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                 embed_model_id = excluded.embed_model_id,
                 dim = excluded.dim, matrix = excluded.matrix,
                 n_rows = excluded.n_rows, n_classes = excluded.n_classes,
                 shrinkage = excluded.shrinkage, power = excluded.power,
                 centred = excluded.centred, version = excluded.version,
                 fitted_at_ns = excluded.fitted_at_ns",
            params![
                p.model_id,
                p.dim as i64,
                p.to_blob(),
                p.n_rows as i64,
                p.n_classes as i64,
                p.whitening.shrinkage,
                p.whitening.power,
                i64::from(p.whitening.centre),
                version,
                at_ns
            ],
        )?;
        Ok(())
    }

    pub fn clear_projection(&self) -> Result<bool> {
        Ok(self
            .conn
            .execute("DELETE FROM identity_projection WHERE id = 1", [])?
            > 0)
    }

    // ---- 0.12.0: the learned aggregate ------------------------------------

    /// How a voice's several prototypes become the one score the ladder
    /// compares, as this install has learned it.
    ///
    /// A `settings` row and not a column, because it is one value for the
    /// whole install rather than a property of any voice. Absent — or
    /// unreadable, which a hand-edited or future-version value could be —
    /// means [`Aggregate::Max`](crate::calib::Aggregate::Max): the rule every
    /// version before 0.12.0 used. A store that has learned nothing must
    /// behave exactly as it did before this existed, and a bad value must cost
    /// the learned rule rather than the label.
    pub fn learned_aggregate(&self) -> Result<crate::calib::Aggregate> {
        Ok(self
            .setting(AGGREGATE_KEY)?
            .as_deref()
            .and_then(crate::calib::Aggregate::parse)
            .unwrap_or_default())
    }

    pub fn set_learned_aggregate(&self, a: crate::calib::Aggregate) -> Result<()> {
        self.set_setting(AGGREGATE_KEY, &a.as_str())
    }

    /// Back to the shipped rule. `true` when there was something to take back.
    pub fn clear_learned_aggregate(&self) -> Result<bool> {
        Ok(self.conn.execute(
            "DELETE FROM settings WHERE key = ?1",
            params![AGGREGATE_KEY],
        )? > 0)
    }

    // ---- 0.12.0: prototypes ground truth condemns -------------------------

    /// Every prototype whose **own source segment** Discord says was somebody
    /// else talking.
    ///
    /// This is a consistency check, not a fit: there is no parameter in it and
    /// nothing is learned. A prototype enrolled from a turn that carries a
    /// `single` verdict — one linked account covering at least
    /// [`truth_verdict::SINGLE_MIN`] of the audio — naming a *different* voice
    /// than the prototype's owner is a recording of that other person filed
    /// under this one, and it will go on winning turns forever, because a
    /// wrong prototype is permanent in a way a wrong label is not.
    ///
    /// Three exclusions, each of them load-bearing:
    ///
    /// * **`partial` and `overlap` verdicts are not evidence.** A `partial`
    ///   verdict is one account under the coverage bar, which is as easily our
    ///   segmentation being generous as it is the wrong person; an `overlap`
    ///   turn has two mouths open and the embedder is captured by one of them
    ///   ([`crate::identity`]), so the prototype may perfectly well be its
    ///   owner. Measured: removing the twenty overlap-sourced prototypes on
    ///   this install *raises* the wrong-label count (§32 step 2).
    /// * **The user's own account is not evidence.** A `single` verdict naming
    ///   the user's own Discord account is not ground truth about audio
    ///   captured from the user's own Discord client — a client does not play
    ///   your microphone back to you (0.10.1, §17). The same rule that keeps
    ///   those rows out of every headline keeps them from condemning a
    ///   prototype, and on this install that rule alone spares twenty-seven.
    /// * **Golden prototypes are never condemned.** Hand-enrolled audio is the
    ///   user's own word about who this is, and it outranks a speaking ring.
    pub fn condemned_prototypes(&self, embed_model_id: &str) -> Result<Vec<CondemnedPrototype>> {
        let you = self.you_speaker_id()?;
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.speaker_id, own.display_name, d.speaker_id, said.display_name,
                    g.id, g.truth_coverage, p.created_at
               FROM speaker_prototypes p
               JOIN speakers own  ON own.id = p.speaker_id
               JOIN segments g    ON g.id = p.source_segment_id
               JOIN discord_users d ON d.user_id = g.truth_user_id
               JOIN speakers said ON said.id = d.speaker_id
              WHERE p.embed_model_id = ?1
                AND p.is_golden = 0
                AND own.merged_into IS NULL
                AND said.merged_into IS NULL
                AND g.deleted_at IS NULL
                AND g.truth_verdict = ?2
                AND COALESCE(g.truth_coverage, 0.0) >= ?3
                AND d.speaker_id <> p.speaker_id
              ORDER BY p.id",
        )?;
        let rows = stmt
            .query_map(
                params![
                    embed_model_id,
                    truth_verdict::SINGLE,
                    truth_verdict::SINGLE_MIN
                ],
                |r| {
                    Ok(CondemnedPrototype {
                        prototype_id: r.get(0)?,
                        owner: r.get(1)?,
                        owner_name: r.get(2)?,
                        truth_speaker: r.get(3)?,
                        truth_name: r.get(4)?,
                        source_segment_id: r.get(5)?,
                        coverage: r.get::<_, Option<f64>>(6)?.unwrap_or(0.0),
                        created_at: r.get(7)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter(|c| Some(c.truth_speaker) != you)
            .collect())
    }

    /// Runs of `k` or more voices minted from one source inside `window_ns`
    /// of each other (0.12.2).
    ///
    /// A mint is not itself a fault — a stranger joining the call is supposed
    /// to get a row in the voicebank. What is a fault is a *run* of them whose
    /// seed turns Discord says were all the same person already in the bank,
    /// which is the shape of the FINDINGS §46 cascade and the reason this is a
    /// report rather than an alarm on a single mint.
    ///
    /// The seed turn is the earliest live row filed under the voice, which
    /// survives `identity repair --prototypes` deleting the prototype it made.
    /// Voices later merged away are included and their run still reported: a
    /// burst that was cleaned up by hand is exactly the history an operator
    /// wants to see before it happens again.
    pub fn mint_bursts(&self, k: usize, window_ns: i64) -> Result<Vec<MintBurst>> {
        if k == 0 {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            r"SELECT s.id, s.display_name, s.created_at,
                    g.id, COALESCE(so.match_key, '(no source)'),
                    d.speaker_id, said.display_name,
                    s.merged_into, tgt.display_name
               FROM speakers s
               LEFT JOIN segments g ON g.id = (
                    SELECT MIN(x.id) FROM segments x
                     WHERE x.speaker_id = s.id AND x.deleted_at IS NULL)
               LEFT JOIN sessions ss ON ss.id = g.session_id
               LEFT JOIN sources so  ON so.id = ss.source_id
               LEFT JOIN discord_users d ON d.user_id = g.truth_user_id
                    AND g.truth_verdict = ?1
               LEFT JOIN speakers said ON said.id = d.speaker_id
               LEFT JOIN speakers tgt ON tgt.id = s.merged_into
              WHERE s.display_name = s.auto_label
                AND s.auto_label LIKE 'Speaker\_%' ESCAPE '\'
              ORDER BY s.created_at ASC, s.id ASC",
        )?;
        struct Row {
            id: i64,
            name: String,
            at: i64,
            seed: Option<i64>,
            source: String,
            says: Option<(i64, String)>,
            went_to: Option<(i64, String)>,
        }
        let rows = stmt
            .query_map(params![truth_verdict::SINGLE], |r| {
                let said: Option<i64> = r.get(5)?;
                let name: Option<String> = r.get(6)?;
                let into: Option<i64> = r.get(7)?;
                let into_name: Option<String> = r.get(8)?;
                Ok(Row {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    at: r.get(2)?,
                    seed: r.get(3)?,
                    source: r.get(4)?,
                    says: said.zip(name),
                    went_to: into.zip(into_name),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        // A burst is a maximal chain: each mint within `window_ns` of the one
        // before it, from the same source. Chained rather than a fixed window,
        // because a cascade paces itself by how often the person talks.
        let mut out: Vec<MintBurst> = Vec::new();
        let mut run: Vec<&Row> = Vec::new();
        let flush = |run: &Vec<&Row>, out: &mut Vec<MintBurst>| {
            if run.len() < k {
                return;
            }
            let mut tally: Vec<(i64, String, usize)> = Vec::new();
            let mut went: Vec<(i64, String, usize)> = Vec::new();
            let mut covered = 0usize;
            let count = |into: &mut Vec<(i64, String, usize)>, id: i64, name: &str| match into
                .iter_mut()
                .find(|(v, ..)| *v == id)
            {
                Some((.., n)) => *n += 1,
                None => into.push((id, name.to_string(), 1)),
            };
            for r in run {
                if let Some((id, name)) = &r.says {
                    covered += 1;
                    count(&mut tally, *id, name);
                }
                if let Some((id, name)) = &r.went_to {
                    count(&mut went, *id, name);
                }
            }
            let commonest_first = |v: &mut Vec<(i64, String, usize)>| {
                v.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)))
            };
            commonest_first(&mut tally);
            commonest_first(&mut went);
            out.push(MintBurst {
                source_key: run[0].source.clone(),
                first_ns: run[0].at,
                last_ns: run[run.len() - 1].at,
                minted: run.iter().map(|r| (r.id, r.name.clone(), r.seed)).collect(),
                seeds_say: tally,
                seeds_with_a_verdict: covered,
                merged_into: went,
            });
        };
        for r in &rows {
            let joins = run
                .last()
                .is_some_and(|p| p.source == r.source && r.at - p.at <= window_ns);
            if !joins {
                flush(&run, &mut out);
                run.clear();
            }
            run.push(r);
        }
        flush(&run, &mut out);
        out.sort_by(|a, b| {
            b.minted
                .len()
                .cmp(&a.minted.len())
                .then(a.first_ns.cmp(&b.first_ns))
        });
        Ok(out)
    }

    /// Voices that are not people: an unnamed `Speaker_NN` holding nothing but
    /// prototypes ground truth condemns, whose rows Discord says are one voice
    /// the bank already has (0.12.2).
    ///
    /// Three conditions, and every one of them is a consistency check with no
    /// free parameter:
    ///
    /// * **unnamed.** `display_name = auto_label` and the label is the minted
    ///   form. The moment somebody types a name over it, it is a person's voice
    ///   and this command has no opinion about it.
    /// * **no prototype stands up.** Every prototype it still holds is one
    ///   [`Self::condemned_prototypes`] names — its own source turn is a turn
    ///   Discord says was somebody else. A voice with *zero* prototypes passes
    ///   vacuously, which is the population `identity repair --prototypes`
    ///   leaves behind: that command deletes the vectors and cannot touch the
    ///   labels, so hundreds of a real person's turns stay filed under a number.
    /// * **the rows agree.** Of its rows carrying a Discord `single` verdict, a
    ///   strict majority must name one voice, and that voice is the target.
    ///   Not unanimity: a phantom holding 168 of one person's turns and 2 of
    ///   another's is still that person's turns, and the two dissenters are
    ///   relabelled to *their own* verdict rather than following the merge.
    ///   No majority is a refusal — a voice half of each is not evidence.
    pub fn phantom_voices(&self, embed_model_id: &str) -> Result<Vec<PhantomVoice>> {
        let you = self.you_speaker_id()?;
        let condemned: Vec<i64> = self
            .condemned_prototypes(embed_model_id)?
            .into_iter()
            .map(|c| c.prototype_id)
            .collect();
        let mut stmt = self.conn.prepare(
            r"SELECT s.id, s.display_name,
                    (SELECT COUNT(*) FROM speaker_prototypes p WHERE p.speaker_id = s.id),
                    (SELECT COUNT(*) FROM segments g
                      WHERE g.speaker_id = s.id AND g.deleted_at IS NULL)
               FROM speakers s
              WHERE s.merged_into IS NULL
                AND s.display_name = s.auto_label
                AND s.auto_label LIKE 'Speaker\_%' ESCAPE '\'
              ORDER BY s.id",
        )?;
        let candidates = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? as usize,
                    r.get::<_, i64>(3)? as usize,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut protos = self
            .conn
            .prepare("SELECT id FROM speaker_prototypes WHERE speaker_id = ?1")?;
        let mut verdicts = self.conn.prepare(
            "SELECT d.speaker_id, said.display_name, COUNT(*)
               FROM segments g
               JOIN discord_users d ON d.user_id = g.truth_user_id
               JOIN speakers said ON said.id = d.speaker_id
              WHERE g.speaker_id = ?1
                AND g.deleted_at IS NULL
                AND g.truth_verdict = ?2
                AND COALESCE(g.truth_coverage, 0.0) >= ?3
                AND said.merged_into IS NULL
              GROUP BY d.speaker_id
              ORDER BY COUNT(*) DESC, d.speaker_id ASC",
        )?;

        let mut out = Vec::new();
        for (id, name, n_protos, rows) in candidates {
            if Some(id) == you {
                continue;
            }
            let held: Vec<i64> = protos
                .query_map(params![id], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let n_condemned = held.iter().filter(|p| condemned.contains(p)).count();
            let says: Vec<(i64, String, usize)> = verdicts
                .query_map(
                    params![id, truth_verdict::SINGLE, truth_verdict::SINGLE_MIN],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)? as usize,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let covered: usize = says.iter().map(|(.., n)| n).sum();
            let mut refused = None;
            if n_condemned < held.len() {
                refused = Some(format!(
                    "{} of its {} prototypes stand up to ground truth",
                    held.len() - n_condemned,
                    held.len()
                ));
            } else if covered == 0 {
                refused = Some("no row of it carries a Discord verdict".into());
            }
            let target = match (&refused, says.first()) {
                (None, Some((v, n, c))) if *c * 2 > covered && Some(*v) != you && *v != id => {
                    Some((*v, n.clone()))
                }
                _ => None,
            };
            if refused.is_none() && target.is_none() {
                refused = Some("no voice holds a majority of the rows Discord could name".into());
            }
            out.push(PhantomVoice {
                speaker_id: id,
                name,
                prototypes: n_protos,
                condemned: n_condemned,
                rows,
                covered,
                says,
                target,
                refused,
            });
        }
        out.sort_by(|a, b| b.rows.cmp(&a.rows).then(a.speaker_id.cmp(&b.speaker_id)));
        Ok(out)
    }

    /// Give a phantom's turns back and tombstone it (0.12.2).
    ///
    /// Every row it holds is relabelled `label_via = "truth"`: one that carries
    /// its own Discord `single` verdict goes to **that** verdict's voice, and
    /// everything else follows the majority target. Then the empty voice is
    /// merged away, so it stops appearing in the roster, stops being a
    /// candidate in the ladder, and takes its condemned prototypes with it.
    ///
    /// Returns the rows as they were, for the caller to write into
    /// `operations`. Nothing here is guessed and nothing is deleted: putting it
    /// back is a matter of replaying that list.
    pub fn absorb_phantom(
        &self,
        phantom: i64,
        target: i64,
    ) -> Result<(Vec<PriorLabel>, MergeReport)> {
        if phantom == target {
            bail!("cannot absorb speaker {phantom} into itself");
        }
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.speaker_id, g.label_via, g.match_score, d.speaker_id
               FROM segments g
               LEFT JOIN discord_users d ON d.user_id = g.truth_user_id
                    AND g.truth_verdict = ?2
                    AND COALESCE(g.truth_coverage, 0.0) >= ?3
               LEFT JOIN speakers said ON said.id = d.speaker_id AND said.merged_into IS NULL
              WHERE g.speaker_id = ?1 AND g.deleted_at IS NULL
              ORDER BY g.id",
        )?;
        let rows = stmt
            .query_map(
                params![phantom, truth_verdict::SINGLE, truth_verdict::SINGLE_MIN],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, Option<f64>>(3)?,
                        r.get::<_, Option<i64>>(4)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        let tx = self.conn.unchecked_transaction()?;
        for (id, .., said) in &rows {
            let to = said.unwrap_or(target);
            tx.execute(
                "UPDATE segments
                    SET speaker_id = ?2, match_score = NULL, label_via = ?3, sweep_at_ns = NULL
                  WHERE id = ?1",
                params![id, to, label_via::TRUTH],
            )?;
        }
        tx.commit()?;
        let prior = rows
            .into_iter()
            .map(
                |(segment_id, speaker_id, label_via, match_score, _)| PriorLabel {
                    segment_id,
                    speaker_id,
                    label_via,
                    match_score,
                },
            )
            .collect();
        Ok((prior, self.merge_speakers(phantom, target)?))
    }

    /// Which embedding spaces the bank actually holds vectors in. Ordered by
    /// how many, so the first is the one that matters.
    pub fn embed_model_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT embed_model_id FROM speaker_prototypes
             GROUP BY embed_model_id ORDER BY COUNT(*) DESC",
        )?;
        let v = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    /// [`Self::prototypes_with_source`], minus the named prototypes — the bank
    /// as it *would* look after a repair, without writing anything.
    pub fn prototypes_with_source_excluding(
        &self,
        embed_model_id: &str,
        exclude: &[i64],
    ) -> Result<Vec<(i64, Option<i64>, Embedding)>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.speaker_id, p.source_segment_id, p.vector
             FROM speaker_prototypes p
             JOIN speakers s ON s.id = p.speaker_id
             WHERE s.merged_into IS NULL AND p.embed_model_id = ?1",
        )?;
        let rows = stmt
            .query_map(params![embed_model_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Vec<u8>>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .filter(|(id, ..)| !exclude.contains(id))
            .map(|(_, sp, src, blob)| Ok((sp, src, Embedding::from_blob(embed_model_id, &blob)?)))
            .collect()
    }

    /// Remove prototypes by id, reporting how many rows actually went. The
    /// count is the honest one: an id that was not there is not a deletion.
    pub fn delete_prototypes(&self, ids: &[i64]) -> Result<usize> {
        let mut n = 0;
        for id in ids {
            n += self
                .conn
                .execute("DELETE FROM speaker_prototypes WHERE id = ?1", params![id])?;
        }
        Ok(n)
    }

    /// Every truth-labelled turn a calibration fit can use, oldest first.
    ///
    /// The same shape as the bench's query and for the same reasons: a
    /// `single` verdict, a linked account, a stored embedding and at least
    /// `min_duration_s` of audio. Ordered by time because every split
    /// downstream is chronological, and a fit that had to sort its own input
    /// is a fit that could forget to.
    pub fn truth_calibration_rows(&self, min_duration_s: f64) -> Result<Vec<CalibrationRow>> {
        #[cfg(test)]
        CALIBRATION_ROW_LOADS.with(|c| c.set(c.get() + 1));
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.t_start_ns, COALESCE(g.overlap_frac, 0.0),
                    (g.t_end_ns - g.t_start_ns), COALESCE(g.text, ''),
                    d.speaker_id, e.vector, e.embed_model_id
             FROM segments g
             JOIN discord_users d ON d.user_id = g.truth_user_id
             JOIN speakers s ON s.id = d.speaker_id
             JOIN embeddings e ON e.id = (
                 SELECT MAX(x.id) FROM embeddings x WHERE x.segment_id = g.id)
             WHERE g.deleted_at IS NULL
               AND g.truth_verdict = ?1
               AND d.speaker_id IS NOT NULL
               AND s.merged_into IS NULL
               AND (g.t_end_ns - g.t_start_ns) >= ?2
             ORDER BY g.t_start_ns ASC, g.id ASC",
        )?;
        let rows = stmt
            .query_map(
                params![truth_verdict::SINGLE, (min_duration_s * 1e9) as i64],
                |r| {
                    let text: String = r.get(4)?;
                    Ok((
                        CalibrationRow {
                            segment_id: r.get(0)?,
                            t_start_ns: r.get(1)?,
                            overlap_frac: r.get::<_, f64>(2)? as f32,
                            duration_s: r.get::<_, i64>(3)? as f32 / 1e9,
                            words: crate::lang::word_count(&text),
                            truth_speaker_id: r.get(5)?,
                            embedding: Embedding::new("", Vec::new()),
                        },
                        r.get::<_, Vec<u8>>(6)?,
                        r.get::<_, String>(7)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(mut row, blob, model)| {
                row.embedding = Embedding::from_blob(model, &blob)?;
                Ok(row)
            })
            .collect()
    }

    /// How many rows [`Self::truth_calibration_rows`] would return, without
    /// returning any of them.
    ///
    /// The rate limit on the nightly calibration pass is "six hours, and only
    /// if the corpus grew by a fifth". The second half needs a number; it does
    /// not need every embedding in the corpus deserialised to get one. The
    /// predicate is deliberately the same one, spelled as an `EXISTS` where the
    /// loading query has a `JOIN … MAX(x.id)`: both mean "this segment has at
    /// least one embedding", and `calibration_count_matches_the_rows_it_counts`
    /// holds them together.
    pub fn truth_calibration_row_count(&self, min_duration_s: f64) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*)
             FROM segments g
             JOIN discord_users d ON d.user_id = g.truth_user_id
             JOIN speakers s ON s.id = d.speaker_id
             WHERE g.deleted_at IS NULL
               AND g.truth_verdict = ?1
               AND d.speaker_id IS NOT NULL
               AND s.merged_into IS NULL
               AND (g.t_end_ns - g.t_start_ns) >= ?2
               AND EXISTS (SELECT 1 FROM embeddings x WHERE x.segment_id = g.id)",
            params![truth_verdict::SINGLE, (min_duration_s * 1e9) as i64],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// The whole bank with the segment each prototype came from, so a replay
    /// can drop the prototypes a row produced itself. Without that a row
    /// scores 1.0 against itself and the measurement is a memory test.
    pub fn prototypes_with_source(
        &self,
        embed_model_id: &str,
    ) -> Result<Vec<(i64, Option<i64>, Embedding)>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.speaker_id, p.source_segment_id, p.vector
             FROM speaker_prototypes p
             JOIN speakers s ON s.id = p.speaker_id
             WHERE s.merged_into IS NULL AND p.embed_model_id = ?1",
        )?;
        let rows = stmt
            .query_map(params![embed_model_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(|(sp, src, blob)| Ok((sp, src, Embedding::from_blob(embed_model_id, &blob)?)))
            .collect()
    }

    /// `(really overlapped, measured overlap fraction)` for every turn with a
    /// `single` or `overlap` verdict, oldest first — the overlap gate's own
    /// ground truth, ready to be split chronologically.
    pub fn truth_overlap_rows_in_order(&self) -> Result<Vec<(i64, bool, f32)>> {
        let mut stmt = self.conn.prepare(
            "SELECT t_start_ns, truth_verdict, COALESCE(overlap_frac, 0.0)
             FROM segments
             WHERE deleted_at IS NULL AND truth_verdict IN (?1, ?2)
             ORDER BY t_start_ns ASC, id ASC",
        )?;
        Ok(stmt
            .query_map(
                params![truth_verdict::SINGLE, truth_verdict::OVERLAP],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)? == truth_verdict::OVERLAP,
                        r.get::<_, f64>(2)? as f32,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

// ---- end 0.11.0 -----------------------------------------------------------

#[cfg(test)]
mod tests {
    #[test]
    fn the_microphone_joins_the_conversation_it_is_actually_in() {
        // The user's half of a conversation lives in the mic session while
        // everyone else's lives in an app session. Threading them apart made
        // every thread a monologue (2026-09-02): no counterparties for the
        // commitment extractor, no shared threads for the person graph. The
        // mic bridges; two APP sessions at the same moment stay separate.
        let dir = std::env::temp_dir().join(format!("nxr-micbridge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir).unwrap();

        let app = store.upsert_source("VRChat.exe", "VRChat", 0).unwrap();
        let mic = store
            .upsert_source_kind("mic", "Microphone", KIND_MIC, 0)
            .unwrap();
        let other_app = store.upsert_source("Discord", "Discord", 0).unwrap();
        let s_app = store.begin_session(app, 0).unwrap();
        let s_mic = store.begin_session(mic, 0).unwrap();
        let s_other = store.begin_session(other_app, 0).unwrap();

        let sec = 1_000_000_000i64;
        let cfg = crate::config::GraphConfig::default();
        let seg = |session: i64, t: i64, speaker: Option<i64>| {
            let id = store
                .insert_segment(session, t, t + sec, "x.wav", t)
                .unwrap();
            if let Some(sp) = speaker {
                store
                    .set_segment_speaker_via(id, Some(sp), Some(0.9), None)
                    .unwrap();
            }
            crate::threads::assign(&store, &cfg, id).unwrap();
            store
                .conn
                .query_row(
                    "SELECT thread_id FROM segments WHERE id = ?1",
                    params![id],
                    |r| r.get::<_, Option<i64>>(0),
                )
                .unwrap()
                .expect("threaded")
        };

        let rowan = store.create_speaker("Rowan", 0).unwrap();
        let you = store.create_speaker("You", 0).unwrap();
        let ines = store.create_speaker("Ines", 0).unwrap();

        // Rowan speaks in the app; the user answers on the mic seconds later:
        // one conversation, across two sessions.
        let t1 = seg(s_app, 0, Some(rowan));
        let t2 = seg(s_mic, 3 * sec, Some(you));
        assert_eq!(t1, t2, "the mic joins the live conversation");

        // A different APP starting at the same moment is its own conversation.
        let t3 = seg(s_other, 5 * sec, Some(ines));
        assert_ne!(t1, t3, "two app sessions never merge by time alone");

        // And the mic keeps alternating with the first conversation.
        let t4 = seg(s_mic, 8 * sec, Some(you));
        assert_eq!(t4, t1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn emb(id: &str, v: &[f32]) -> Embedding {
        Embedding::new(id, v.to_vec())
    }

    fn a_segment(s: &Store) -> i64 {
        let src = s.upsert_source("VRChat.exe", "VRChat.exe", 1).unwrap();
        let sess = s.begin_session(src, 1_000).unwrap();
        s.insert_segment(sess, 1_000, 2_000, "segments/a.wav", 0)
            .unwrap()
    }

    // ---- Step 1 behaviour, unchanged -------------------------------------

    // ---- 0.12.0 (schema v14): which instance a session was -----------------

    #[test]
    fn two_copies_of_one_app_share_a_source_and_no_longer_share_an_identity() {
        let s = store();
        // What PipeWire gives us: one `application.process.binary`, so one
        // source row — and that stays true, because the allowlist, the
        // `[rules]` table and the GUI's source card are all keyed on it.
        let src = s.upsert_source("vesktop", "Vesktop", 1).unwrap();
        let a = s.begin_session_for(src, 10, Some("serial:8801")).unwrap();
        let b = s.begin_session_for(src, 11, Some("serial:8802")).unwrap();
        let same: i64 = s
            .conn
            .query_row(
                "SELECT COUNT(DISTINCT source_id) FROM sessions WHERE id IN (?1, ?2)",
                params![a, b],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(same, 1, "one app, one source row — unchanged");
        let keys: Vec<Option<String>> = ["a", "b"]
            .iter()
            .zip([a, b])
            .map(|(_, id)| {
                s.conn
                    .query_row(
                        "SELECT instance_key FROM sessions WHERE id = ?1",
                        params![id],
                        |r| r.get(0),
                    )
                    .unwrap()
            })
            .collect();
        assert_eq!(
            keys,
            vec![Some("serial:8801".into()), Some("serial:8802".into())],
            "…and the two copies are now distinguishable, which is the whole change"
        );
    }

    #[test]
    fn a_session_from_a_node_with_no_instance_property_is_null_not_a_guess() {
        // NULL has to stay distinguishable from "instance one". A session
        // recorded before v14, or opened by a node carrying neither
        // `object.serial` nor `application.process.id`, genuinely does not know.
        let s = store();
        let src = s.upsert_source("vesktop", "Vesktop", 1).unwrap();
        let id = s.begin_session(src, 10).unwrap();
        let key: Option<String> = s
            .conn
            .query_row(
                "SELECT instance_key FROM sessions WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(key, None);
    }

    #[test]
    fn schema_version_is_stamped() {
        let s = store();
        let v: i64 = s
            .conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn sources_are_deduplicated_by_match_key() {
        let s = store();
        let a = s.upsert_source("VRChat.exe", "VRChat.exe", 100).unwrap();
        let b = s
            .upsert_source("VRChat.exe", "VRChat.exe (renamed)", 200)
            .unwrap();
        assert_eq!(a, b);
        let rows = s.list_sources().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].first_seen, 100);
        assert_eq!(rows[0].display_name, "VRChat.exe (renamed)");
    }

    #[test]
    fn a_seen_source_defaults_to_denied() {
        let s = store();
        s.upsert_source("firefox", "Firefox", 1).unwrap();
        assert!(!s.list_sources().unwrap()[0].allowed);
    }

    #[test]
    fn upsert_does_not_clobber_an_existing_allow_flag() {
        let s = store();
        s.set_allowed("VRChat.exe", true, 1).unwrap();
        s.upsert_source("VRChat.exe", "VRChat.exe", 5).unwrap();
        assert!(s.list_sources().unwrap()[0].allowed);
    }

    #[test]
    fn set_allowed_creates_then_flips() {
        let s = store();
        s.set_allowed("Discord", true, 10).unwrap();
        assert!(s.list_sources().unwrap()[0].allowed);
        s.set_allowed("Discord", false, 999).unwrap();
        let rows = s.list_sources().unwrap();
        assert!(!rows[0].allowed);
        assert_eq!(rows[0].first_seen, 10);
    }

    #[test]
    fn sessions_and_segments_round_trip() {
        let s = store();
        let src = s.upsert_source("VRChat.exe", "VRChat.exe", 1).unwrap();
        let sess = s.begin_session(src, 1_000).unwrap();
        s.insert_segment(sess, 1_100, 2_100, "segments/a.wav", 2_200)
            .unwrap();
        s.insert_segment(sess, 3_100, 4_100, "segments/b.wav", 4_200)
            .unwrap();
        assert_eq!(s.segment_count(sess).unwrap(), 2);

        s.end_session(sess, 9_000).unwrap();
        let ended: i64 = s
            .conn
            .query_row(
                "SELECT ended_at_utc_ns FROM sessions WHERE id = ?1",
                params![sess],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ended, 9_000);
    }

    #[test]
    fn ending_a_session_twice_keeps_the_first_end_time() {
        let s = store();
        let src = s.upsert_source("x", "x", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        s.end_session(sess, 100).unwrap();
        s.end_session(sess, 200).unwrap();
        let ended: i64 = s
            .conn
            .query_row(
                "SELECT ended_at_utc_ns FROM sessions WHERE id = ?1",
                params![sess],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ended, 100);
    }

    #[test]
    fn dangling_sessions_are_closed_on_restart() {
        let s = store();
        let src = s.upsert_source("x", "x", 1).unwrap();
        let open = s.begin_session(src, 0).unwrap();
        let closed = s.begin_session(src, 0).unwrap();
        s.end_session(closed, 50).unwrap();

        assert_eq!(s.close_dangling_sessions(500).unwrap(), 1);
        let ended: i64 = s
            .conn
            .query_row(
                "SELECT ended_at_utc_ns FROM sessions WHERE id = ?1",
                params![open],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ended, 500);
    }

    #[test]
    fn segments_require_a_real_session() {
        let s = store();
        assert!(s.insert_segment(4242, 0, 1, "x.wav", 0).is_err());
    }

    #[test]
    fn allowed_sources_sort_first() {
        let s = store();
        s.upsert_source("zzz", "zzz", 1).unwrap();
        s.set_allowed("aaa", false, 1).unwrap();
        s.set_allowed("mmm", true, 1).unwrap();
        let keys: Vec<String> = s
            .list_sources()
            .unwrap()
            .into_iter()
            .map(|r| r.match_key)
            .collect();
        assert_eq!(keys, vec!["mmm", "aaa", "zzz"]);
    }

    // ---- migration -------------------------------------------------------

    /// 0.9.0 (v11). The whole migration is four columns and one table, and the
    /// thing worth asserting is that it is a **no-op on the rows that exist**:
    /// a note captured before v11 had no due date to lose, and a turn captured
    /// before it has no translation nobody computed.
    #[test]
    fn a_v10_database_gains_the_assistant_and_loses_nothing() {
        let dir = std::env::temp_dir().join(format!(
            "nx-recall-v11-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A database as 0.8.2 left it: opened by this build, then stamped back
        // to v10 with the v11 columns dropped, which is as close to a real v10
        // file as a test can get without checking a binary into the tree.
        let (note_id, seg) = {
            let s = Store::open(&dir).unwrap();
            let src = s.upsert_source("VRChat.exe", "VRChat", 0).unwrap();
            let sess = s.begin_session(src, 0).unwrap();
            let seg = s.insert_segment(sess, 100, 200, "x.wav", 0).unwrap();
            s.set_segment_analysis(
                seg,
                &SegmentAnalysis {
                    text: Some("Recall, merk dir den Shader".into()),
                    ..Default::default()
                },
            )
            .unwrap();
            let note = s.upsert_note(seg, "den Shader", None, 7).unwrap().unwrap();
            s.conn
                .execute_batch(
                    // The indexes reference the columns, so they go first —
                    // which is also what a real v10 database looks like.
                    "DROP INDEX IF EXISTS idx_notes_due;
                     DROP INDEX IF EXISTS idx_segments_translation;
                     ALTER TABLE notes DROP COLUMN due_ns;
                     ALTER TABLE notes DROP COLUMN fired_at_ns;
                     ALTER TABLE segments DROP COLUMN translation;
                     ALTER TABLE segments DROP COLUMN translation_via;
                     DROP TABLE digests;
                     UPDATE schema_version SET version = 10;",
                )
                .unwrap();
            (note.id, seg)
        };

        let s = Store::open(&dir).unwrap();
        let v: i64 = s
            .conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);

        // The note is still there, and it is not a reminder: nothing invented a
        // date for a sentence that never had one.
        let note = s.note(note_id).unwrap().expect("the note survived");
        assert_eq!(note.text, "den Shader");
        assert_eq!(note.due_ns, None);
        assert_eq!(note.fired_at_ns, None);
        assert!(s.notes_due(i64::MAX, 10).unwrap().is_empty());

        // The turn is still there, and it has no translation — which is not the
        // same as "needs none", and is why the column is NULL rather than "".
        let row = s.segment_row(seg).unwrap().expect("the turn survived");
        assert_eq!(row.text.as_deref(), Some("Recall, merk dir den Shader"));
        assert_eq!(row.translation, None);
        assert_eq!(row.translation_via, None);
        assert_eq!(s.translation_counts().unwrap(), (0, 0));

        // …and the new surface works on the migrated database.
        assert_eq!(s.digest_rows(None, 10).unwrap(), Vec::new());
        s.snooze_note(note_id, 5, 0).unwrap().unwrap();
        assert_eq!(s.notes_due(6 * 60_000_000_000, 10).unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 0.12.0 (v15). Two nullable columns is the easy half; the half that can
    /// actually break is the **view**.
    ///
    /// `speaker_resolved` has existed since v2 and every read path in this file
    /// joins through it, so v15 cannot add its two columns with
    /// `CREATE VIEW IF NOT EXISTS` — the view is already there, the create would
    /// be a no-op, and every segment query would then ask a three-column view
    /// for `sp.colour`. That is not a wrong answer, it is `no such column` on
    /// the transcript, which is the whole application. So the migration DROPs
    /// and rebuilds, and this test is the thing that notices if it ever stops.
    ///
    /// Built by taking a current database back to the shape 0.11.8 left (the
    /// two columns off, the three-column view restored, the stamp back at 13)
    /// rather than by pasting a historical schema in here — the same
    /// downgrade-simulation the v5 and v11 migration tests use, and for the
    /// same reason. It also covers the skipped stamp: this tree carries no
    /// `apply_v14`, so a 13 has to arrive at 15 in one open.
    #[test]
    fn a_pre_highlight_database_gains_the_highlight_and_its_view_is_rebuilt() {
        let dir = std::env::temp_dir().join(format!(
            "nx-recall-v15-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let (kira, seg) = {
            let s = Store::open(&dir).unwrap();
            let src = s.upsert_source("VRChat.exe", "VRChat", 0).unwrap();
            let sess = s.begin_session(src, 0).unwrap();
            let seg = s.insert_segment(sess, 100, 200, "x.wav", 0).unwrap();
            let kira = s.create_speaker("Kira", 1).unwrap();
            s.set_segment_speaker(seg, Some(kira), Some(0.9)).unwrap();
            s.set_segment_analysis(
                seg,
                &SegmentAnalysis {
                    text: Some("die Shader sind fertig".into()),
                    ..Default::default()
                },
            )
            .unwrap();

            // A database as 0.11.8 left it. The view goes first: SQLite will
            // not drop a column another object still names, and a real v13
            // database has the three-column view anyway.
            s.conn
                .execute_batch(
                    "DROP VIEW IF EXISTS speaker_resolved;
                     ALTER TABLE speakers DROP COLUMN colour;
                     ALTER TABLE speakers DROP COLUMN icon;
                     CREATE VIEW speaker_resolved AS
                         SELECT s.id                                AS id,
                                COALESCE(t.id, s.id)                AS canonical_id,
                                COALESCE(t.display_name, s.display_name) AS display_name
                         FROM speakers s
                         LEFT JOIN speakers t ON t.id = s.merged_into;
                     UPDATE schema_version SET version = 13;",
                )
                .unwrap();
            (kira, seg)
        };

        let s = Store::open(&dir).unwrap();
        let v: i64 = s
            .conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        assert_eq!(v, 19);

        // The columns are back…
        let columns = |table: &str| -> Vec<String> {
            let mut stmt = s
                .conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .unwrap();
            stmt.query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let speakers = columns("speakers");
        assert!(speakers.contains(&"colour".to_owned()), "{speakers:?}");
        assert!(speakers.contains(&"icon".to_owned()), "{speakers:?}");

        // …and so is the five-column view, which is the half a
        // `CREATE VIEW IF NOT EXISTS` would have quietly skipped.
        assert_eq!(
            columns("speaker_resolved"),
            vec!["id", "canonical_id", "display_name", "colour", "icon"]
        );

        // The voice survived the round trip and is simply not highlighted,
        // which is the only thing NULL has ever meant here — no backfill, no
        // invented colour for somebody who never picked one.
        let row = s.speaker_summary(kira).unwrap().expect("Kira survived");
        assert_eq!(row.display_name, "Kira");
        assert_eq!(row.colour, None);
        assert_eq!(row.icon, None);
        assert_eq!(s.speaker_style(kira).unwrap(), Some((None, None)));

        // And a segment reads back through the rebuilt view. This is the
        // assertion that fails with `no such column: sp.colour` if the view is
        // ever created-if-missing instead of dropped and rebuilt.
        let seg_row = s.segment_row(seg).unwrap().expect("the turn survived");
        assert_eq!(seg_row.speaker_name.as_deref(), Some("Kira"));
        assert_eq!(seg_row.speaker_colour, None);
        assert_eq!(seg_row.speaker_icon, None);

        // The new surface works on the migrated database.
        s.set_speaker_style(kira, Some("violet"), Some("\u{1f319}"))
            .unwrap();
        let seg_row = s.segment_row(seg).unwrap().unwrap();
        assert_eq!(seg_row.speaker_colour.as_deref(), Some("violet"));
        assert_eq!(seg_row.speaker_icon.as_deref(), Some("\u{1f319}"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 0.12.4 (v18). Three nullable columns, no backfill, and one property
    /// that is easy to get wrong and expensive to notice: **the queue has to
    /// empty.**
    ///
    /// `mood_at_ns` is a separate column from the two answers precisely so a
    /// row the model abstained on stops being asked about. If the pass wrote
    /// only `mood` and `events`, the three rows in four SenseVoice declines on
    /// would come back in every query for the life of the archive, and the
    /// backlog printed on the Memory card would never move — the exact bug
    /// `lang_sweep_counts` had to learn from in 0.12.1.
    #[test]
    fn a_row_the_model_had_no_opinion_about_leaves_the_mood_queue() {
        let dir = std::env::temp_dir().join(format!("nxr-v18-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Store::open(&dir).unwrap();
        let v: i64 = s
            .conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);

        let src = s.upsert_source("VRChat.exe", "VRChat.exe", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        // Three rows: one the model will answer about, one it will decline,
        // and one whose clip retention has taken.
        let heard = s
            .insert_segment(sess, 0, 3_000_000_000, "a.wav", 0)
            .unwrap();
        let quiet = s
            .insert_segment(sess, 4_000_000_000, 7_000_000_000, "b.wav", 0)
            .unwrap();
        let gone = s
            .insert_segment(sess, 8_000_000_000, 11_000_000_000, "", 0)
            .unwrap();

        // The queue skips the row with no audio, and there is nothing to skip
        // it FOR — this is the filter `mood_counts` has to match term for term.
        // Newest first: `quiet` starts at 4s and `heard` at 0s.
        let queued: Vec<i64> = s
            .segments_for_mood(1.0, 10)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(queued, vec![quiet, heard]);
        assert_eq!(s.mood_counts(1.0).unwrap(), (2, 0));

        // A short row is never queued at all, whatever the floor is doing.
        let brief = s
            .insert_segment(sess, 12_000_000_000, 12_300_000_000, "c.wav", 0)
            .unwrap();
        assert!(
            !s.segments_for_mood(1.0, 10)
                .unwrap()
                .iter()
                .any(|c| c.id == brief),
            "a 300ms back-channel reached the queue"
        );

        s.set_segment_mood(heard, Some("happy"), Some("laughter,music"), 100)
            .unwrap();
        // The abstention: both answers NULL, and the STAMP set anyway.
        s.set_segment_mood(quiet, None, None, 100).unwrap();

        assert!(
            s.segments_for_mood(1.0, 10).unwrap().is_empty(),
            "a row the model declined came back in the queue"
        );
        assert_eq!(
            s.mood_counts(1.0).unwrap(),
            (0, 2),
            "the backlog never reached zero"
        );

        // The answers read back on the ordinary segment shape, which is what
        // every surface in the app draws from.
        let row = s.segment_row(heard).unwrap().unwrap();
        assert_eq!(row.mood.as_deref(), Some("happy"));
        assert_eq!(row.events.as_deref(), Some("laughter,music"));
        let row = s.segment_row(quiet).unwrap().unwrap();
        assert_eq!(row.mood, None);
        assert_eq!(
            row.events, None,
            "an abstention is NULL, never the empty string"
        );
        // A row nothing has reached is indistinguishable from an abstention on
        // the wire, and that is correct — both render as nothing.
        let row = s.segment_row(gone).unwrap().unwrap();
        assert_eq!(row.mood, None);

        // The totals a person page reads. `read` is the denominator and counts
        // the abstention, because the pass DID listen to it — a laughter share
        // over "rows with a mood" would be a different and much flatterier
        // number.
        let t = s.thread_mood(0).unwrap();
        assert_eq!(t.read, 0, "these rows are in no thread");
        let src_totals = s
            .conn
            .query_row(
                "SELECT COUNT(*) FROM segments WHERE mood_at_ns IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(src_totals, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 0.12.5. **The latest thing said is the first thing read.**
    ///
    /// The pass used to drain oldest first, which is right for a sweep whose
    /// output nobody looks at and wrong for this one: the reason to know a
    /// turn had laughter on it is that somebody is about to open tonight's
    /// transcript. On an archive with two years in it, oldest-first means the
    /// row captured five minutes ago is stamped last — after every one of the
    /// twenty thousand before it.
    ///
    /// Two properties, and the second is the one that makes the first safe:
    /// the head of the queue is the newest unstamped row, and the walk is
    /// still **resumable** — stamping a batch removes it from the next query,
    /// so the pass converges on an empty queue from either end and a daemon
    /// killed halfway does not start again at the top.
    #[test]
    fn the_mood_pass_reads_the_newest_turns_first() {
        let dir = std::env::temp_dir().join(format!("nxr-mood-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = Store::open(&dir).unwrap();
        let src = s.upsert_source("VRChat.exe", "VRChat.exe", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();

        // Ten seconds of archive, inserted oldest-first so that insertion
        // order and capture order agree — if the query were falling back on
        // rowid it would still look right, which is why the assertion below
        // names the ids rather than counting them.
        let mut ids = Vec::new();
        for i in 0..6i64 {
            let t0 = i * 10_000_000_000;
            ids.push(
                s.insert_segment(sess, t0, t0 + 3_000_000_000, &format!("{i}.wav"), 0)
                    .unwrap(),
            );
        }

        let first: Vec<i64> = s
            .segments_for_mood(1.0, 2)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(
            first,
            vec![ids[5], ids[4]],
            "the pass started at the beginning of history instead of at tonight"
        );

        // Resumable: stamp what came back and the next batch is the next two
        // down, never the same two again.
        for id in &first {
            s.set_segment_mood(*id, None, None, 1).unwrap();
        }
        let second: Vec<i64> = s
            .segments_for_mood(1.0, 2)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(second, vec![ids[3], ids[2]]);

        // A turn captured WHILE the pass is working is newer than everything
        // it has read, so it goes to the head of the queue rather than to the
        // back of a two-year line. This is the whole point of the ordering.
        let just_now = s
            .insert_segment(sess, 900_000_000_000, 903_000_000_000, "now.wav", 0)
            .unwrap();
        assert_eq!(
            s.segments_for_mood(1.0, 1).unwrap()[0].id,
            just_now,
            "a turn said just now queued behind the archive"
        );

        // And the queue still empties, which is the property 0.12.4's test
        // exists to protect and which reordering must not cost.
        loop {
            let batch = s.segments_for_mood(1.0, 4).unwrap();
            if batch.is_empty() {
                break;
            }
            for c in batch {
                s.set_segment_mood(c.id, None, None, 1).unwrap();
            }
        }
        assert_eq!(
            s.mood_counts(1.0).unwrap().0,
            0,
            "the backlog never drained"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_sweeps_bare_mark_becomes_a_visit_and_gives_the_lang_via_back() {
        // Schema v16's backfill, which is a rename rather than a computation.
        //
        // 0.11.9 wrote `lang_via = 'sweep'` in two shapes — with a language, as
        // a provenance, and with a NULL language, as an "asked, nothing to say"
        // mark. The second shape is exactly what `sweep_at_ns` is now for, so
        // it moves; the first is a provenance and must not be touched. Getting
        // this backwards would either lose the stamps or leave the marks
        // invisible to a column nothing else reads.
        let dir = std::env::temp_dir().join(format!("nxr-v16-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (marked, stamped, untouched) = {
            let s = Store::open(&dir).unwrap();
            let src = s.upsert_source("VRChat.exe", "VRChat.exe", 1).unwrap();
            let sess = s.begin_session(src, 0).unwrap();
            let id = || {
                s.insert_segment(sess, 0, 2_000_000_000, "a.wav", 0)
                    .unwrap()
            };
            let (marked, stamped, untouched) = (id(), id(), id());
            // As 0.12.0 left them, written straight to the columns because the
            // calls that produced these two shapes no longer exist.
            s.conn
                .execute(
                    "UPDATE segments SET lang_via = 'sweep' WHERE id = ?1",
                    params![marked],
                )
                .unwrap();
            s.conn
                .execute(
                    "UPDATE segments SET lang = 'de', lang_via = 'sweep' WHERE id = ?1",
                    params![stamped],
                )
                .unwrap();
            // The index goes first: SQLite will not drop a column another
            // object still names, and a real v15 database has neither.
            s.conn
                .execute_batch(
                    "DROP INDEX IF EXISTS idx_segments_sweep;
                     ALTER TABLE segments DROP COLUMN sweep_at_ns;
                     UPDATE schema_version SET version = 15;",
                )
                .unwrap();
            (marked, stamped, untouched)
        };

        let s = Store::open(&dir).unwrap();
        let read = |id: i64| -> (Option<String>, Option<String>, Option<i64>) {
            s.conn
                .query_row(
                    "SELECT lang, lang_via, sweep_at_ns FROM segments WHERE id = ?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap()
        };
        // The bare mark is now a visit, and the `lang_via` it was borrowing is
        // back to NULL. Zero rather than "now": the honest answer to *when* is
        // "before this migration", and a clock reading would claim the sweep
        // ran at upgrade time.
        assert_eq!(read(marked), (None, None, Some(0)));
        // The stamp is a provenance and keeps both halves of itself…
        assert_eq!(
            read(stamped),
            (Some("de".into()), Some("sweep".into()), None)
        );
        // …and a row nobody had swept is untouched in all three columns.
        assert_eq!(read(untouched), (None, None, None));

        // The migrated marks really are off the work list, which is the whole
        // point of moving them.
        assert!(
            !s.segments_for_lang_sweep(1.0, 10)
                .unwrap()
                .iter()
                .any(|r| r.id == marked)
        );
        assert_eq!(s.lang_sweep_counts(1.0).unwrap(), (1, 1));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_v1_database_migrates_and_keeps_its_rows() {
        let dir = std::env::temp_dir().join(format!("nx-recall-mig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("recall.db");

        // A database exactly as Step 1 left it.
        {
            let c = Connection::open(&db).unwrap();
            c.execute_batch(
                "CREATE TABLE schema_version (version INTEGER NOT NULL);
                 INSERT INTO schema_version (version) VALUES (1);
                 CREATE TABLE sources (
                     id INTEGER PRIMARY KEY, match_key TEXT NOT NULL UNIQUE,
                     display_name TEXT NOT NULL, allowed INTEGER NOT NULL DEFAULT 0,
                     first_seen INTEGER NOT NULL);
                 CREATE TABLE sessions (
                     id INTEGER PRIMARY KEY, source_id INTEGER NOT NULL REFERENCES sources(id),
                     started_at_utc_ns INTEGER NOT NULL, ended_at_utc_ns INTEGER);
                 CREATE TABLE segments (
                     id INTEGER PRIMARY KEY, session_id INTEGER NOT NULL REFERENCES sessions(id),
                     t_start_ns INTEGER NOT NULL, t_end_ns INTEGER NOT NULL,
                     audio_path TEXT NOT NULL, created_at INTEGER NOT NULL);
                 INSERT INTO sources (id, match_key, display_name, allowed, first_seen)
                     VALUES (1, 'VRChat.exe', 'VRChat.exe', 1, 5);
                 INSERT INTO sessions (id, source_id, started_at_utc_ns) VALUES (1, 1, 10);
                 INSERT INTO segments (id, session_id, t_start_ns, t_end_ns, audio_path, created_at)
                     VALUES (1, 1, 100, 200, 'segments/old.wav', 0);",
            )
            .unwrap();
        }

        let s = Store::open(&dir).unwrap();
        let v: i64 = s
            .conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        assert_eq!(s.segment_count(1).unwrap(), 1);
        let sources = s.list_sources().unwrap();
        assert_eq!(sources.len(), 1);
        // v4 backfill, over the whole v1 -> v5 chain: everything that existed
        // before the microphone was an application.
        assert_eq!(sources[0].kind, KIND_APP);
        assert_eq!(s.session_source_kind(1).unwrap().as_deref(), Some(KIND_APP));
        // v5: the row had no speaker, so it has no label provenance either,
        // and no voice has declared a language.
        assert_eq!(s.segment_fields(1).unwrap()["label_via"], None);

        // The v2 surface is usable on the migrated database.
        s.set_segment_analysis(
            1,
            &SegmentAnalysis {
                text: Some("hello from the past".into()),
                asr_model_id: Some("m@1".into()),
                overlap_frac: Some(0.0),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s.search("past", 10).unwrap().len(), 1);

        // Re-opening an already-migrated database is a no-op, not an error.
        drop(s);
        let s = Store::open(&dir).unwrap();
        assert_eq!(s.segment_count(1).unwrap(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The v5 backfill has one real judgement in it: before v5, "labelled by
    /// the microphone" was spelled "has a speaker and a NULL score", and that
    /// spelling has to be read back correctly or every old mic row would claim
    /// to have been matched against a voicebank it never touched.
    #[test]
    fn a_v4_database_learns_where_its_old_labels_came_from() {
        let dir = std::env::temp_dir().join(format!("nx-recall-mig5-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let matched;
        let mine;
        let you;
        {
            // Build a v4-shaped database using the current code, then take the
            // v5 columns back off: the only way to test the migration without
            // pasting a whole historical schema in here.
            let s = Store::open(&dir).unwrap();
            let src = s.upsert_source("VRChat.exe", "VRChat.exe", 1).unwrap();
            let sess = s.begin_session(src, 0).unwrap();
            you = s.ensure_you_speaker(1).unwrap();
            let other = s.create_speaker("Kira", 1).unwrap();
            matched = s.insert_segment(sess, 0, 1_000, "a.wav", 0).unwrap();
            mine = s.insert_segment(sess, 2_000, 3_000, "b.wav", 0).unwrap();
            s.set_segment_speaker(matched, Some(other), Some(0.8))
                .unwrap();
            s.set_segment_speaker(mine, Some(you), None).unwrap();
            s.conn
                .execute_batch(
                    "DROP INDEX IF EXISTS idx_segments_label_via;
                     ALTER TABLE segments DROP COLUMN label_via;
                     ALTER TABLE segments DROP COLUMN lang_via;
                     ALTER TABLE speakers DROP COLUMN languages;
                     UPDATE schema_version SET version = 4;",
                )
                .unwrap();
        }

        let s = Store::open(&dir).unwrap();
        let v: i64 = s
            .conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        assert_eq!(
            s.segment_fields(matched).unwrap()["label_via"].as_deref(),
            Some(label_via::MATCH)
        );
        assert_eq!(
            s.segment_fields(mine).unwrap()["label_via"].as_deref(),
            Some(label_via::MIC),
            "a scoreless row on the pinned voice was the microphone, not a match"
        );
        assert_eq!(s.you_speaker_id().unwrap(), Some(you));
        assert_eq!(s.speaker_languages(you).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The v6 backfill has one job and it is the whole feature: a database
    /// captured before threading existed must come up threaded exactly as it
    /// would have been had every turn been threaded as it arrived.
    #[test]
    fn a_v5_database_is_threaded_on_the_way_up() {
        let dir = std::env::temp_dir().join(format!("nx-recall-mig6-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let sec = 1_000_000_000i64;
        let mut ids: Vec<i64> = Vec::new();
        {
            // A v5-shaped database: the same interleaving the pure rule is
            // tested on, written the way 0.6.1 would have written it.
            let s = Store::open(&dir).unwrap();
            let src = s.upsert_source("VRChat.exe", "VRChat.exe", 1).unwrap();
            let sess = s.begin_session(src, 0).unwrap();
            let a = s.create_speaker("A", 1).unwrap();
            let b = s.create_speaker("B", 1).unwrap();
            let c = s.create_speaker("C", 1).unwrap();
            let d = s.create_speaker("D", 1).unwrap();
            for (i, sp) in [a, b, a, b, c, d, c, d].iter().enumerate() {
                let at = i as i64 * 5 * sec;
                let id = s
                    .insert_segment(sess, at, at + 3 * sec, "x.wav", 0)
                    .unwrap();
                s.set_segment_speaker(id, Some(*sp), Some(0.8)).unwrap();
                ids.push(id);
            }
            s.conn
                .execute_batch(
                    "DROP INDEX IF EXISTS idx_segments_thread;
                     DROP INDEX IF EXISTS idx_segments_session_start;
                     DROP INDEX IF EXISTS idx_segments_speaker_thread;
                     DROP TABLE threads;
                     ALTER TABLE segments DROP COLUMN thread_id;
                     UPDATE schema_version SET version = 5;",
                )
                .unwrap();
        }

        let s = Store::open(&dir).unwrap();
        let threads: Vec<i64> = ids
            .iter()
            .map(|id| s.segment_row(*id).unwrap().unwrap().thread_id.unwrap())
            .collect();
        assert_eq!(
            threads[0], threads[3],
            "A and B's four turns are one conversation"
        );
        assert_eq!(
            threads[4], threads[7],
            "C and D's four turns are another one"
        );
        assert_ne!(
            threads[0], threads[4],
            "two pairs talking past each other are not one conversation"
        );
        let n: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);

        // Re-opening does not thread anything twice: the column exists now, so
        // the backfill does not run again.
        drop(s);
        let s = Store::open(&dir).unwrap();
        let again: i64 = s
            .conn
            .query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))
            .unwrap();
        assert_eq!(again, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_future_schema_is_refused_rather_than_mangled() {
        let dir = std::env::temp_dir().join(format!("nx-recall-future-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            let c = Connection::open(dir.join("recall.db")).unwrap();
            c.execute_batch(
                "CREATE TABLE schema_version (version INTEGER NOT NULL);
                 INSERT INTO schema_version (version) VALUES (99);",
            )
            .unwrap();
        }
        assert!(Store::open(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- FTS -------------------------------------------------------------

    #[test]
    fn the_transcript_index_is_filled_by_the_update_that_writes_the_text() {
        let s = store();
        let seg = a_segment(&s);
        // The row exists with no text; nothing to find yet.
        assert!(s.search("violin", 10).unwrap().is_empty());

        s.set_segment_analysis(
            seg,
            &SegmentAnalysis {
                text: Some("solitude and a violin".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let hits = s.search("violin", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].segment_id(), seg);
        assert!(hits[0].snippet.contains("violin"));
    }

    #[test]
    fn re_transcribing_replaces_the_old_text_in_the_index() {
        let s = store();
        let seg = a_segment(&s);
        let set = |t: &str| {
            s.set_segment_analysis(
                seg,
                &SegmentAnalysis {
                    text: Some(t.into()),
                    ..Default::default()
                },
            )
            .unwrap()
        };
        set("the first transcript");
        set("a completely different one");
        assert!(s.search("first", 10).unwrap().is_empty());
        assert_eq!(s.search("different", 10).unwrap().len(), 1);
    }

    #[test]
    fn deleting_a_segment_removes_it_from_the_index() {
        let s = store();
        let seg = a_segment(&s);
        s.set_segment_analysis(
            seg,
            &SegmentAnalysis {
                text: Some("ephemeral words".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s.search("ephemeral", 10).unwrap().len(), 1);
        s.conn
            .execute("DELETE FROM segments WHERE id = ?1", params![seg])
            .unwrap();
        assert!(s.search("ephemeral", 10).unwrap().is_empty());
    }

    #[test]
    fn search_reports_the_speaker_and_soft_deleted_rows_are_hidden() {
        let s = store();
        let seg = a_segment(&s);
        let spk = s.create_speaker("Mira", 0).unwrap();
        s.set_segment_analysis(
            seg,
            &SegmentAnalysis {
                text: Some("meet me at the fountain".into()),
                ..Default::default()
            },
        )
        .unwrap();
        s.set_segment_speaker(seg, Some(spk), Some(0.7)).unwrap();
        let hits = s.search("fountain", 10).unwrap();
        assert_eq!(hits[0].speaker(), Some("Mira"));

        s.conn
            .execute(
                "UPDATE segments SET deleted_at = 1 WHERE id = ?1",
                params![seg],
            )
            .unwrap();
        assert!(s.search("fountain", 10).unwrap().is_empty());
    }

    // ---- voicebank -------------------------------------------------------

    #[test]
    fn prototypes_are_scoped_to_one_embedding_space() {
        let s = store();
        let spk = s.create_speaker("A", 0).unwrap();
        s.add_prototype(spk, &emb("eres2net_en@1", &[1.0, 0.0]), None, false, 20, 0)
            .unwrap();
        s.add_prototype(
            spk,
            &emb("titanet_small@1", &[0.0, 1.0]),
            None,
            false,
            20,
            0,
        )
        .unwrap();

        let bank = s.prototypes("eres2net_en@1").unwrap();
        assert_eq!(bank.len(), 1);
        assert_eq!(bank[0].1.vector, vec![1.0, 0.0]);
        assert_eq!(s.prototypes("titanet_small@1").unwrap().len(), 1);
        assert!(s.prototypes("nothing@1").unwrap().is_empty());
    }

    #[test]
    fn a_speaker_stops_growing_at_the_cap_and_drops_its_nearest_prototype() {
        let s = store();
        let spk = s.create_speaker("A", 0).unwrap();
        for i in 0..3 {
            let v = emb("m@1", &[1.0, i as f32]);
            s.add_prototype(spk, &v, None, false, 3, 0).unwrap();
        }
        assert_eq!(s.prototype_count(spk).unwrap(), 3);

        // Nearly identical to [1,0]; that one should be the eviction.
        s.add_prototype(spk, &emb("m@1", &[1.0, 0.001]), None, false, 3, 0)
            .unwrap();
        assert_eq!(s.prototype_count(spk).unwrap(), 3);
        let kept: Vec<Vec<f32>> = s
            .prototypes("m@1")
            .unwrap()
            .into_iter()
            .map(|(_, e)| e.vector)
            .collect();
        assert!(!kept.contains(&vec![1.0, 0.0]));
        assert!(kept.contains(&vec![1.0, 1.0]));
        assert!(kept.contains(&vec![1.0, 2.0]));
    }

    #[test]
    fn golden_prototypes_are_not_subject_to_the_cap() {
        let s = store();
        let spk = s.create_speaker("A", 0).unwrap();
        let first = s
            .add_prototype(spk, &emb("m@1", &[1.0, 0.0]), None, true, 1, 0)
            .unwrap();
        s.add_prototype(spk, &emb("m@1", &[0.0, 1.0]), None, true, 1, 0)
            .unwrap();
        assert_eq!(s.prototype_count(spk).unwrap(), 2);
        assert!(first.is_some(), "a stored prototype answers with its rowid");

        // An auto-enrolled vector cannot displace them, so it is dropped — and
        // saying so is the point (audit finding #25). This used to answer
        // `Ok(0)`, a perfectly plausible rowid, and `commit_mic` read it as an
        // enrolment: `mic_enrolled` counted turns that added nothing to the
        // bank, for exactly the voice whose bank was already full of the best
        // audio it will ever have.
        let dropped = s
            .add_prototype(spk, &emb("m@1", &[1.0, 0.0]), None, false, 1, 0)
            .unwrap();
        assert_eq!(dropped, None, "nothing was stored, and the answer says so");
        assert_eq!(s.prototype_count(spk).unwrap(), 2);
    }

    #[test]
    fn minted_names_are_sequential() {
        let s = store();
        assert_eq!(s.mint_speaker(0).unwrap(), 1);
        assert_eq!(s.mint_speaker(0).unwrap(), 2);
        let names: Vec<String> = s
            .list_speakers()
            .unwrap()
            .into_iter()
            .map(|r| r.display_name)
            .collect();
        assert!(names.contains(&"Speaker_01".to_string()));
        assert!(names.contains(&"Speaker_02".to_string()));
    }

    #[test]
    fn naming_is_retroactive_because_the_id_is_the_identity() {
        let s = store();
        let seg = a_segment(&s);
        let spk = s.mint_speaker(0).unwrap();
        s.set_segment_speaker(seg, Some(spk), Some(0.9)).unwrap();
        s.set_segment_analysis(
            seg,
            &SegmentAnalysis {
                text: Some("said before anyone knew who".into()),
                ..Default::default()
            },
        )
        .unwrap();

        s.rename_speaker(spk, "Kestrel", 99).unwrap();
        assert_eq!(
            s.transcript(None, None).unwrap()[0].speaker.as_deref(),
            Some("Kestrel")
        );
        assert_eq!(
            s.search("anyone", 10).unwrap()[0].speaker(),
            Some("Kestrel")
        );
    }

    #[test]
    fn renaming_a_speaker_that_does_not_exist_is_an_error() {
        assert!(store().rename_speaker(77, "Nobody", 0).is_err());
    }

    // ---- merge -----------------------------------------------------------

    #[test]
    fn merge_moves_rows_and_tombstones_the_source() {
        let s = store();
        let src = s.upsert_source("x", "x", 0).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let a = s.create_speaker("A", 0).unwrap();
        let b = s.create_speaker("B", 0).unwrap();

        let seg = s.insert_segment(sess, 0, 1_000, "a.wav", 0).unwrap();
        s.set_segment_speaker(seg, Some(a), Some(0.5)).unwrap();
        s.add_prototype(a, &emb("m@1", &[1.0]), None, false, 20, 0)
            .unwrap();
        s.add_golden_sample(a, "golden/a.wav", 4.0).unwrap();

        let report = s.merge_speakers(a, b).unwrap();
        assert_eq!(report.segments, 1);
        assert_eq!(report.prototypes, 1);
        assert_eq!(report.golden_samples, 1);
        assert_eq!(report.tombstones_repointed, 0);

        assert_eq!(s.resolve_speaker(a).unwrap(), b);
        assert_eq!(s.prototype_count(b).unwrap(), 1);
        // The tombstone is gone from listings but its rows landed on B.
        let live: Vec<i64> = s
            .list_speakers()
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(live, vec![b]);
        assert_eq!(s.list_speakers().unwrap()[0].segments, 1);
    }

    #[test]
    fn merging_re_points_existing_tombstones_instead_of_chaining() {
        let s = store();
        let a = s.create_speaker("A", 0).unwrap();
        let b = s.create_speaker("B", 0).unwrap();
        let c = s.create_speaker("C", 0).unwrap();

        s.merge_speakers(a, b).unwrap();
        let report = s.merge_speakers(b, c).unwrap();
        assert_eq!(
            report.tombstones_repointed, 1,
            "A's tombstone must follow B"
        );

        // Both old ids resolve in one hop.
        assert_eq!(s.resolve_speaker(a).unwrap(), c);
        assert_eq!(s.resolve_speaker(b).unwrap(), c);
        let merged_of_a: Option<i64> = s
            .conn
            .query_row(
                "SELECT merged_into FROM speakers WHERE id = ?1",
                params![a],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(merged_of_a, Some(c), "no chain may be left behind");
    }

    #[test]
    fn merging_onto_a_tombstone_lands_on_the_canonical_speaker() {
        let s = store();
        let a = s.create_speaker("A", 0).unwrap();
        let b = s.create_speaker("B", 0).unwrap();
        let c = s.create_speaker("C", 0).unwrap();
        s.merge_speakers(a, b).unwrap();
        // C -> A, but A is a tombstone for B.
        let report = s.merge_speakers(c, a).unwrap();
        assert_eq!(report.into, b);
        assert_eq!(s.resolve_speaker(c).unwrap(), b);
    }

    #[test]
    fn degenerate_merges_are_refused() {
        let s = store();
        let a = s.create_speaker("A", 0).unwrap();
        let b = s.create_speaker("B", 0).unwrap();
        assert!(s.merge_speakers(a, a).is_err());
        s.merge_speakers(a, b).unwrap();
        // A is already a tombstone.
        assert!(s.merge_speakers(a, b).is_err());
    }

    #[test]
    fn a_merged_speaker_reads_through_to_its_new_name() {
        let s = store();
        let seg = a_segment(&s);
        let a = s.create_speaker("Speaker_01", 0).unwrap();
        let b = s.create_speaker("Wren", 0).unwrap();
        s.set_segment_speaker(seg, Some(a), Some(0.4)).unwrap();
        s.merge_speakers(a, b).unwrap();
        assert_eq!(
            s.transcript(None, None).unwrap()[0].speaker.as_deref(),
            Some("Wren")
        );
        assert_eq!(s.transcript(None, Some(b)).unwrap().len(), 1);
    }

    // ---- split -----------------------------------------------------------

    /// One turn as the pipeline writes it: a segment, its embedding, and the
    /// prototype that remembers which segment it came from.
    fn a_voiced_segment(s: &Store, session: i64, speaker: i64, v: &[f32], golden: bool) -> i64 {
        let t = 1_000 * (s.segments_total().unwrap() + 1);
        let seg = s
            .insert_segment(session, t, t + 500, "segments/v.wav", 0)
            .unwrap();
        let e = emb("m@1", v);
        s.store_embedding(seg, &e).unwrap();
        s.set_segment_speaker(seg, Some(speaker), Some(0.8))
            .unwrap();
        s.add_prototype(speaker, &e, Some(seg), golden, 20, 0)
            .unwrap();
        seg
    }

    #[test]
    fn a_prototype_and_the_segment_it_came_from_are_one_vector() {
        let s = store();
        let src = s.upsert_source("x", "x", 0).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let spk = s.mint_speaker(0).unwrap();
        let seg = a_voiced_segment(&s, sess, spk, &[1.0, 0.0], false);

        let vectors = s.speaker_vectors(spk, "m@1").unwrap();
        assert_eq!(vectors.len(), 1, "the same audio must not vote twice");
        assert_eq!(vectors[0].segment_id, Some(seg));
        assert!(vectors[0].prototype_id.is_some());
        assert!(!vectors[0].is_golden);

        // A labelled segment the bank never enrolled is still evidence.
        let lone = s
            .insert_segment(sess, 9_000, 9_500, "segments/l.wav", 0)
            .unwrap();
        s.store_embedding(lone, &emb("m@1", &[0.0, 1.0])).unwrap();
        s.set_segment_speaker(lone, Some(spk), Some(0.4)).unwrap();
        let vectors = s.speaker_vectors(spk, "m@1").unwrap();
        assert_eq!(vectors.len(), 2);
        assert_eq!(vectors[1].prototype_id, None);
        assert_eq!(vectors[1].segment_id, Some(lone));
    }

    #[test]
    fn a_split_only_ever_looks_at_one_embedding_space() {
        let s = store();
        let src = s.upsert_source("x", "x", 0).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let spk = s.mint_speaker(0).unwrap();
        a_voiced_segment(&s, sess, spk, &[1.0, 0.0], false);
        a_voiced_segment(&s, sess, spk, &[0.0, 1.0], false);
        // One stray vector from a different extractor: it must not be able to
        // reach a cosine against the others.
        s.add_prototype(spk, &emb("other@1", &[1.0, 0.0]), None, false, 20, 0)
            .unwrap();

        assert_eq!(s.speaker_embed_model(spk).unwrap().as_deref(), Some("m@1"));
        let vectors = s.speaker_vectors(spk, "m@1").unwrap();
        assert_eq!(vectors.len(), 2);
        assert!(vectors.iter().all(|v| v.embedding.model_id == "m@1"));
        assert!(s.speaker_embed_model(4242).unwrap().is_none());
    }

    #[test]
    fn a_split_moves_exactly_what_it_is_told_and_leaves_the_rest_alone() {
        let s = store();
        let src = s.upsert_source("x", "x", 0).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let spk = s.create_speaker("Wren", 0).unwrap();
        let mine = a_voiced_segment(&s, sess, spk, &[1.0, 0.0], false);
        let theirs = a_voiced_segment(&s, sess, spk, &[0.0, 1.0], false);
        let fence = a_voiced_segment(&s, sess, spk, &[1.0, 1.0], false);
        let moving = s.speaker_vectors(spk, "m@1").unwrap()[1]
            .prototype_id
            .unwrap();

        let report = s
            .split_speaker(
                spk,
                &SplitWrite {
                    prototypes: vec![moving],
                    segments: vec![(theirs, 0.94)],
                    ambiguous: vec![(fence, 0.51)],
                },
                77,
            )
            .unwrap();
        assert_eq!(report.kept, spk);
        assert_ne!(report.minted, spk);
        assert_eq!(report.prototypes, 1);
        assert_eq!(report.segments, 1);
        assert_eq!(report.ambiguous, 1);
        assert!(report.auto_label.starts_with("Speaker_"));

        // The existing id keeps its name, its history and its rows.
        assert_eq!(s.speaker_name(spk).unwrap().as_deref(), Some("Wren"));
        assert_eq!(s.segment_state(mine).unwrap().0, Some(spk));
        assert_eq!(s.segment_state(theirs).unwrap().0, Some(report.minted));
        assert_eq!(s.prototype_count(spk).unwrap(), 2);
        assert_eq!(s.prototype_count(report.minted).unwrap(), 1);

        // The undecidable row stayed put but is no longer trusted; the
        // confident majority was not restamped at all.
        let score = |seg: i64| -> f32 {
            s.segment_fields(seg).unwrap()["match_score"]
                .as_ref()
                .unwrap()
                .parse()
                .unwrap()
        };
        assert!((score(fence) - 0.51).abs() < 1e-5);
        assert_eq!(s.segment_state(fence).unwrap().0, Some(spk));
        assert!(
            (score(mine) - 0.8).abs() < 1e-5,
            "an untouched row is untouched"
        );
        assert!((score(theirs) - 0.94).abs() < 1e-5);

        // Both voices are live, and the audit trail can name what moved.
        assert_eq!(s.list_speakers().unwrap().len(), 2);
        let labels = s.segment_labels(&[mine, theirs, 4242]).unwrap();
        assert_eq!(labels.len(), 2, "a segment that is gone is simply absent");
        assert_eq!(labels[1].segment_id, theirs);
        assert_eq!(labels[1].speaker_id, Some(report.minted));
    }

    #[test]
    fn a_tombstone_cannot_be_split_and_neither_can_nothing() {
        let s = store();
        let a = s.create_speaker("A", 0).unwrap();
        let b = s.create_speaker("B", 0).unwrap();
        s.add_prototype(a, &emb("m@1", &[1.0, 0.0]), None, false, 20, 0)
            .unwrap();
        s.merge_speakers(a, b).unwrap();
        // Splitting the tombstone would quietly cut up B instead.
        assert!(
            s.split_speaker(
                a,
                &SplitWrite {
                    prototypes: vec![1],
                    ..Default::default()
                },
                0
            )
            .is_err()
        );
        // Splitting the merge target is the whole point, but it still has to
        // move something.
        assert!(s.split_speaker(b, &SplitWrite::default(), 0).is_err());
        assert_eq!(s.list_speakers().unwrap().len(), 1, "no voice was minted");
    }

    // ---- reading ---------------------------------------------------------

    #[test]
    fn transcript_is_chronological_and_filterable() {
        let s = store();
        let src = s.upsert_source("x", "x", 0).unwrap();
        let s1 = s.begin_session(src, 0).unwrap();
        let s2 = s.begin_session(src, 0).unwrap();
        let spk = s.create_speaker("A", 0).unwrap();

        let late = s.insert_segment(s1, 3_000, 4_000, "b.wav", 0).unwrap();
        let early = s.insert_segment(s1, 1_000, 2_000, "a.wav", 0).unwrap();
        let other = s.insert_segment(s2, 2_000, 2_500, "c.wav", 0).unwrap();
        s.set_segment_speaker(early, Some(spk), Some(0.6)).unwrap();

        let all = s.transcript(None, None).unwrap();
        assert_eq!(
            all.iter().map(|r| r.segment_id).collect::<Vec<_>>(),
            vec![early, other, late]
        );
        assert_eq!(s.transcript(Some(s2), None).unwrap().len(), 1);
        assert_eq!(s.transcript(None, Some(spk)).unwrap()[0].segment_id, early);
    }

    #[test]
    fn analysis_columns_round_trip() {
        let s = store();
        let seg = a_segment(&s);
        s.set_segment_analysis(
            seg,
            &SegmentAnalysis {
                text: Some("some words".into()),
                lang: Some("en".into()),
                lang_via: Some(lang_via::CLASSIFIED.into()),
                asr_model_id: Some("parakeet@1".into()),
                overlap_frac: Some(0.25),
            },
        )
        .unwrap();
        let f = s.segment_fields(seg).unwrap();
        assert_eq!(f["text"].as_deref(), Some("some words"));
        assert_eq!(f["asr_model_id"].as_deref(), Some("parakeet@1"));
        assert!(f["overlap_frac"].as_ref().unwrap().starts_with("0.25"));
        assert_eq!(f["speaker_id"], None);
        assert_eq!(f["lang"].as_deref(), Some("en"));
        assert_eq!(f["lang_via"].as_deref(), Some(lang_via::CLASSIFIED));
    }

    #[test]
    fn embeddings_round_trip_with_their_model_id() {
        let s = store();
        let seg = a_segment(&s);
        let v = emb("eres2net_en@1", &[0.5, -0.25, 1.0]);
        s.store_embedding(seg, &v).unwrap();
        let back = s.segment_embedding(seg).unwrap().unwrap();
        assert_eq!(back, v);
    }

    // ---- v3: filters, soft delete, audit trail, roster --------------------

    #[test]
    fn filters_narrow_by_speaker_session_source_and_time() {
        let s = store();
        let vr = s.upsert_source("VRChat.exe", "VRChat.exe", 0).unwrap();
        let dc = s.upsert_source("Discord", "Discord", 0).unwrap();
        let s1 = s.begin_session(vr, 0).unwrap();
        let s2 = s.begin_session(dc, 0).unwrap();
        let spk = s.create_speaker("Wren", 0).unwrap();

        let a = s.insert_segment(s1, 1_000, 2_000, "a.wav", 0).unwrap();
        let b = s.insert_segment(s1, 5_000, 6_000, "b.wav", 0).unwrap();
        let c = s.insert_segment(s2, 9_000, 9_500, "c.wav", 0).unwrap();
        s.set_segment_speaker(a, Some(spk), Some(0.5)).unwrap();
        for (id, text) in [(a, "portal world"), (b, "portal again"), (c, "portal chat")] {
            s.set_segment_analysis(
                id,
                &SegmentAnalysis {
                    text: Some(text.into()),
                    ..Default::default()
                },
            )
            .unwrap();
        }

        let all = SegmentFilter::default();
        assert!(all.is_everything());
        assert_eq!(s.segments_matching(&all).unwrap().len(), 3);
        assert_eq!(s.search_filtered("portal", &all, 10).unwrap().len(), 3);

        let by_speaker = SegmentFilter {
            speaker: Some(spk),
            ..Default::default()
        };
        assert_eq!(
            s.segments_matching(&by_speaker).unwrap(),
            vec![(a, "a.wav".to_string())]
        );
        assert_eq!(
            s.search_filtered("portal", &by_speaker, 10).unwrap().len(),
            1
        );

        let by_source = SegmentFilter {
            source: Some("Discord".into()),
            ..Default::default()
        };
        assert_eq!(s.segments_matching(&by_source).unwrap().len(), 1);

        let by_window = SegmentFilter {
            from: Some(4_000),
            to: Some(9_000),
            ..Default::default()
        };
        assert_eq!(
            s.segments_matching(&by_window).unwrap(),
            vec![(b, "b.wav".to_string())]
        );
        assert_eq!(s.segment_rows(&by_window, 10).unwrap().len(), 1);
        assert_eq!(s.segment_rows(&all, 2).unwrap().len(), 2, "limit applies");
    }

    /// The backwards-paging primitive (0.7.4). The GUI's infinite scrollback is
    /// built on it and adds no daemon method: `{to, limit}` is "the newest
    /// `limit` rows strictly before T", which is exactly one page older than
    /// whatever is already loaded.
    ///
    /// This is the 0.7.1 ordering rule seen from the other side. `anchored` is
    /// `from.is_some() || session.is_some()` — a `to` alone does NOT anchor, so
    /// the query stays on the DESC + reverse path and the LIMIT bites at the
    /// NEW end of the range rather than the old one. Selecting ASC here would
    /// hand back the oldest rows before T and the scrollback would page the
    /// wrong way for ever.
    ///
    /// THE SHARED EXPECTATION TABLE. `gui/test/paging.test.js` asserts the mock
    /// daemon against this same fixture and these same rows — the mock is a
    /// conformance twin, not an approximation, because a divergence in this one
    /// method has shipped three bugs. Ten segments, one per 1000 ns, ids 1..=10
    /// in time order. `from` is inclusive (`>=`), `to` is exclusive (`<`).
    ///
    /// | query                          | rows      | why                         |
    /// |--------------------------------|-----------|-----------------------------|
    /// | `{limit: 3}`                   | 8, 9, 10  | unanchored: the newest 3    |
    /// | `{to: 8000, limit: 3}`         | 5, 6, 7   | the newest 3 before T       |
    /// | `{to: 5000, limit: 3}`         | 2, 3, 4   | the page before that one    |
    /// | `{to: 2000, limit: 3}`         | 1         | short page = the beginning  |
    /// | `{to: 1000, limit: 3}`         | (none)    | T is exclusive              |
    /// | `{from: 3000, limit: 3}`       | 3, 4, 5   | anchored: the oldest 3      |
    /// | `{from: 3000, to: 6000, l: 10}`| 3, 4, 5   | a bounded day, in order     |
    ///
    /// Every answer is ascending by time, always: the caller renders
    /// chronologically whichever end the LIMIT bit off.
    #[test]
    fn a_to_only_transcript_page_is_the_newest_rows_before_it() {
        let s = store();
        let src = s.upsert_source("VRChat.exe", "VRChat.exe", 0).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let ids: Vec<i64> = (1..=10)
            .map(|n| {
                s.insert_segment(sess, n * 1_000, n * 1_000 + 500, &format!("{n}.wav"), 0)
                    .unwrap()
            })
            .collect();
        // Ids are handed out in insertion order, so "row 5" below is ids[4].
        let row = |n: usize| ids[n - 1];
        let page = |from: Option<i64>, to: Option<i64>, limit: usize| {
            s.segment_rows(
                &SegmentFilter {
                    from,
                    to,
                    ..Default::default()
                },
                limit,
            )
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect::<Vec<_>>()
        };

        // The table, line by line.
        assert_eq!(
            page(None, None, 3),
            vec![row(8), row(9), row(10)],
            "unanchored and limited is the NEWEST n, ascending (the 0.7.1 rule)"
        );
        assert_eq!(
            page(None, Some(8_000), 3),
            vec![row(5), row(6), row(7)],
            "a `to` alone must not anchor: this is the newest 3 BEFORE T"
        );
        assert_eq!(
            page(None, Some(5_000), 3),
            vec![row(2), row(3), row(4)],
            "paging again from the first row of the previous page walks backwards"
        );
        assert_eq!(
            page(None, Some(2_000), 3),
            vec![row(1)],
            "a short page is how a client learns it has reached the beginning"
        );
        assert!(
            page(None, Some(1_000), 3).is_empty(),
            "`to` is exclusive, so paging from the very first row returns nothing"
        );
        assert_eq!(
            page(Some(3_000), None, 3),
            vec![row(3), row(4), row(5)],
            "a `from` DOES anchor: the oldest 3 at or after T"
        );
        assert_eq!(
            page(Some(3_000), Some(6_000), 10),
            vec![row(3), row(4), row(5)],
            "a bounded range is the date picker's query, in order"
        );

        // Walking the whole history backwards in pages of 4 visits every row
        // exactly once and terminates — the loop the GUI's scrollback runs.
        let mut seen: Vec<i64> = Vec::new();
        let mut cursor = None;
        loop {
            let rows = s
                .segment_rows(
                    &SegmentFilter {
                        to: cursor,
                        ..Default::default()
                    },
                    4,
                )
                .unwrap();
            if rows.is_empty() {
                break;
            }
            cursor = Some(rows[0].t_start_ns);
            for r in rows.into_iter().rev() {
                seen.push(r.id);
            }
        }
        seen.reverse();
        assert_eq!(
            seen, ids,
            "backwards paging visited every row, once, in order"
        );
    }

    #[test]
    fn soft_delete_hides_the_row_and_the_purge_removes_it() {
        let s = store();
        let seg = a_segment(&s);
        s.set_segment_analysis(
            seg,
            &SegmentAnalysis {
                text: Some("a thing best forgotten".into()),
                ..Default::default()
            },
        )
        .unwrap();
        s.store_embedding(seg, &emb("m@1", &[1.0])).unwrap();

        assert_eq!(s.soft_delete_segments(&[seg], 1_000).unwrap(), 1);
        assert!(s.search("forgotten", 10).unwrap().is_empty());
        assert!(s.transcript(None, None).unwrap().is_empty());
        // Still there, still undoable, and its audio is not an orphan.
        assert_eq!(s.all_audio_paths().unwrap().len(), 1);
        // Re-deleting is a no-op, so the undo window is not silently extended.
        assert_eq!(s.soft_delete_segments(&[seg], 9_999).unwrap(), 0);

        assert!(s.expired_soft_deletes(999).unwrap().is_empty());
        let expired = s.expired_soft_deletes(2_000).unwrap();
        assert_eq!(expired, vec![(seg, "segments/a.wav".to_string())]);
        assert_eq!(s.purge_segments(&[seg]).unwrap(), 1);
        assert!(s.all_audio_paths().unwrap().is_empty());
        assert!(s.segment_embedding(seg).unwrap().is_none());
    }

    #[test]
    fn ageing_out_audio_keeps_the_transcript() {
        let s = store();
        let seg = a_segment(&s);
        s.set_segment_analysis(
            seg,
            &SegmentAnalysis {
                text: Some("words outlive their audio".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(s.audio_older_than(1_500).unwrap().len(), 1);
        s.forget_audio(&[seg]).unwrap();
        assert!(s.audio_older_than(1_500).unwrap().is_empty());
        assert!(s.all_audio_paths().unwrap().is_empty());
        assert_eq!(s.search("outlive", 10).unwrap().len(), 1);
    }

    #[test]
    fn a_reassignment_replaces_the_model_score_with_nothing() {
        let s = store();
        let seg = a_segment(&s);
        let a = s.create_speaker("A", 0).unwrap();
        let b = s.create_speaker("B", 0).unwrap();
        s.set_segment_speaker(seg, Some(a), Some(0.42)).unwrap();

        s.reassign_segment(seg, Some(b)).unwrap();
        let f = s.segment_fields(seg).unwrap();
        assert_eq!(f["speaker_id"].as_deref(), Some(b.to_string().as_str()));
        assert_eq!(f["match_score"], None, "a human decision has no cosine");

        // Reassigning onto a tombstone lands on the canonical speaker.
        let c = s.create_speaker("C", 0).unwrap();
        s.merge_speakers(b, c).unwrap();
        s.reassign_segment(seg, Some(b)).unwrap();
        assert_eq!(s.segment_state(seg).unwrap().0, Some(c));
        assert!(s.reassign_segment(4242, Some(a)).is_err());
    }

    #[test]
    fn correcting_a_transcript_reindexes_it() {
        let s = store();
        let seg = a_segment(&s);
        s.set_segment_analysis(
            seg,
            &SegmentAnalysis {
                text: Some("the bell tolls".into()),
                ..Default::default()
            },
        )
        .unwrap();
        s.correct_segment_text(seg, "the belt holds").unwrap();
        assert!(s.search("tolls", 10).unwrap().is_empty());
        assert_eq!(s.search("belt", 10).unwrap().len(), 1);
        assert_eq!(
            s.segment_state(seg).unwrap().1.as_deref(),
            Some("the belt holds")
        );
    }

    #[test]
    fn operations_are_recorded_newest_first_and_outlive_their_rows() {
        let s = store();
        s.log_operation("speakers.name", "[3]", r#"{"name":"Speaker_03"}"#, 10)
            .unwrap();
        s.log_operation("speakers.merge", "[3,4]", r#"{"from":3,"into":4}"#, 20)
            .unwrap();
        let ops = s.operations(10).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].op, "speakers.merge");
        assert_eq!(ops[0].target_ids, "[3,4]");
        assert_eq!(ops[1].prior_state, r#"{"name":"Speaker_03"}"#);
        assert_eq!(s.operations(1).unwrap().len(), 1);
    }

    #[test]
    fn the_roster_tracks_presence_and_a_world_change_ends_it() {
        let s = store();
        // Non-ASCII names are the common case, not an edge case.
        s.roster_join(Some("wrld_a"), Some("12345"), "きつね", 1_000)
            .unwrap();
        s.roster_join(Some("wrld_a"), Some("12345"), "Ines", 2_000)
            .unwrap();
        // A restart re-reads the log: the same join must not duplicate.
        s.roster_join(Some("wrld_a"), Some("12345"), "きつね", 1_000)
            .unwrap();
        assert_eq!(s.roster_present().unwrap().len(), 2);

        assert!(s.roster_leave("Ines", 3_000).unwrap());
        assert!(
            !s.roster_leave("Nobody", 3_000).unwrap(),
            "a stray leave is noise"
        );
        let present = s.roster_present().unwrap();
        assert_eq!(present.len(), 1);
        assert_eq!(present[0].display_name, "きつね");

        assert_eq!(s.roster_close_all(4_000).unwrap(), 1);
        assert!(s.roster_present().unwrap().is_empty());
        // Both people overlap the window; the one who left is still on record.
        assert_eq!(s.roster_between(0, 10_000).unwrap().len(), 2);
        assert_eq!(s.roster_between(3_500, 10_000).unwrap().len(), 1);
    }

    #[test]
    fn a_v2_database_gains_the_v3_tables() {
        let dir = std::env::temp_dir().join(format!("nx-recall-mig3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            // A v2 database: everything Step 3 wrote, stamped 2.
            let s = Store::open(&dir).unwrap();
            s.conn
                .execute_batch(
                    "DROP TABLE session_roster;
                     DROP TABLE operations;
                     UPDATE schema_version SET version = 2;",
                )
                .unwrap();
            let seg = a_segment(&s);
            assert_eq!(seg, 1);
        }
        let s = Store::open(&dir).unwrap();
        let v: i64 = s
            .conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        assert_eq!(s.segment_count(1).unwrap(), 1, "v2 rows survive");
        s.roster_join(None, None, "Ines", 1).unwrap();
        s.log_operation("x", "[]", "{}", 1).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- v4: source kinds, settings, the pinned "You" speaker -------------

    #[test]
    fn a_v3_database_gains_the_source_kind_and_the_settings_table() {
        let dir = std::env::temp_dir().join(format!("nx-recall-mig4-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        {
            // A v3 database: everything Step 4 wrote, stamped 3, with the v4
            // additions taken back out.
            let s = Store::open(&dir).unwrap();
            let seg = a_segment(&s);
            assert_eq!(seg, 1);
            s.conn
                .execute_batch(
                    "DROP TABLE settings;
                     DROP INDEX idx_sources_kind;
                     ALTER TABLE sources DROP COLUMN kind;
                     UPDATE schema_version SET version = 3;",
                )
                .unwrap();
        }

        let s = Store::open(&dir).unwrap();
        let v: i64 = s
            .conn
            .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        assert_eq!(s.segment_count(1).unwrap(), 1, "v3 rows survive");

        // Backfilled, not left NULL: a pre-v4 source is an application.
        let rows = s.list_sources().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, KIND_APP);
        assert_eq!(s.session_source_kind(1).unwrap().as_deref(), Some(KIND_APP));

        // And the v4 surface works on the migrated database.
        let mic = s
            .upsert_source_kind("mic", "Microphone", KIND_MIC, 1)
            .unwrap();
        assert_eq!(
            s.list_sources()
                .unwrap()
                .iter()
                .find(|r| r.id == mic)
                .map(|r| r.kind.as_str()),
            Some(KIND_MIC)
        );
        s.set_setting("k", "v").unwrap();
        assert_eq!(s.setting("k").unwrap().as_deref(), Some("v"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_source_kind_survives_reopening_and_defaults_to_app() {
        let dir = std::env::temp_dir().join(format!("nx-recall-kind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let s = Store::open(&dir).unwrap();
            s.upsert_source("VRChat.exe", "VRChat", 1).unwrap();
            s.upsert_source_kind("mic", "Microphone", KIND_MIC, 1)
                .unwrap();
            // `set_allowed` on the mic key mirrors the switch and must not
            // demote the row back to an application.
            s.set_allowed("mic", true, 2).unwrap();
        }
        let s = Store::open(&dir).unwrap();
        let by_key: HashMap<String, String> = s
            .list_sources()
            .unwrap()
            .into_iter()
            .map(|r| (r.match_key, r.kind))
            .collect();
        assert_eq!(by_key.get("VRChat.exe").map(String::as_str), Some(KIND_APP));
        assert_eq!(by_key.get("mic").map(String::as_str), Some(KIND_MIC));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_you_speaker_is_created_once_and_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("nx-recall-you-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = {
            let s = Store::open(&dir).unwrap();
            assert_eq!(s.you_speaker_id().unwrap(), None, "nothing until asked");
            let id = s.ensure_you_speaker(10).unwrap();
            // Idempotent: every mic segment calls this.
            assert_eq!(s.ensure_you_speaker(20).unwrap(), id);
            assert_eq!(s.ensure_you_speaker(30).unwrap(), id);
            assert_eq!(s.list_speakers().unwrap().len(), 1);
            id
        };
        // A restart is a fresh Store over the same file.
        let s = Store::open(&dir).unwrap();
        assert_eq!(s.you_speaker_id().unwrap(), Some(first));
        assert_eq!(s.ensure_you_speaker(40).unwrap(), first);
        assert_eq!(s.list_speakers().unwrap().len(), 1, "no second You");

        // Naming yourself does not unpin you: the id is the identity, and the
        // generated label is what a lost settings row would re-adopt from.
        s.rename_speaker(first, "Alex", 50).unwrap();
        assert_eq!(s.ensure_you_speaker(60).unwrap(), first);
        let row = s.speaker_summary(first).unwrap().unwrap();
        assert_eq!(row.name(), Some("Alex"));
        assert_eq!(row.auto_label, YOU_AUTO_LABEL);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_lost_pin_re_adopts_the_existing_you_instead_of_minting_a_second() {
        let s = store();
        let you = s.ensure_you_speaker(10).unwrap();
        // Whatever loses the settings row — a hand-edited database, a restore
        // from an older backup — must not produce two of the user.
        s.conn
            .execute(
                "DELETE FROM settings WHERE key = ?1",
                params![YOU_SPEAKER_KEY],
            )
            .unwrap();
        assert_eq!(s.you_speaker_id().unwrap(), Some(you));
        assert_eq!(s.ensure_you_speaker(20).unwrap(), you);
        assert_eq!(s.list_speakers().unwrap().len(), 1);
        // …and the pin healed itself on the way past.
        assert_eq!(
            s.setting(YOU_SPEAKER_KEY).unwrap().as_deref(),
            Some(you.to_string().as_str())
        );
    }

    #[test]
    fn merging_you_away_moves_the_pin_and_does_not_resurrect_a_duplicate() {
        let s = store();
        let you = s.ensure_you_speaker(10).unwrap();
        let kira = s.create_speaker("Kira", 20).unwrap();

        // The user decides their mic voice and their named voice are the same
        // person, which they are. The pin has to follow the rows.
        s.merge_speakers(you, kira).unwrap();
        assert_eq!(s.you_speaker_id().unwrap(), Some(kira));
        assert_eq!(
            s.ensure_you_speaker(30).unwrap(),
            kira,
            "the next mic segment lands on the surviving voice"
        );
        // The tombstone stays a tombstone: no third speaker, and nothing has
        // re-adopted the dead row by its label.
        assert_eq!(s.list_speakers().unwrap().len(), 1);
        assert_eq!(s.resolve_speaker(you).unwrap(), kira);

        // And the other direction: merging a stranger INTO You leaves the pin
        // exactly where it was.
        let stranger = s.create_speaker("Speaker_09", 40).unwrap();
        s.merge_speakers(stranger, kira).unwrap();
        assert_eq!(s.you_speaker_id().unwrap(), Some(kira));
    }

    #[test]
    fn goldens_are_listed_longest_first_and_follow_a_merge() {
        let s = store();
        let you = s.ensure_you_speaker(10).unwrap();
        s.add_golden_sample(you, "goldens/000001/a.wav", 2.0)
            .unwrap();
        let long = s
            .add_golden_sample(you, "goldens/000001/b.wav", 7.5)
            .unwrap();
        let rows = s.golden_samples_for(you).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, long, "longest first — that is the one to keep");
        assert_eq!(rows[0].duration_s, 7.5);

        // Deleting hands back the path so the caller can unlink it.
        assert_eq!(
            s.delete_golden_sample(rows[1].id).unwrap().as_deref(),
            Some("goldens/000001/a.wav")
        );
        assert_eq!(s.delete_golden_sample(rows[1].id).unwrap(), None);

        // A merge moves the audio with everything else, and the surviving id
        // finds it. The pin follows too, so the next mic segment asks about
        // `kira` and gets the clips it kept as `you`.
        let kira = s.create_speaker("Kira", 20).unwrap();
        s.merge_speakers(you, kira).unwrap();
        assert_eq!(s.golden_samples_for(kira).unwrap().len(), 1);
        assert_eq!(s.you_speaker_id().unwrap(), Some(kira));
    }

    #[test]
    fn unanalysed_segments_are_the_ones_without_a_transcript() {
        let s = store();
        let done = a_segment(&s);
        let pending = s.insert_segment(1, 5_000, 6_000, "b.wav", 0).unwrap();
        s.set_segment_analysis(
            done,
            &SegmentAnalysis {
                asr_model_id: Some("m@1".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let todo = s.unanalysed_segments(10).unwrap();
        assert_eq!(todo.len(), 1);
        assert_eq!(todo[0].0, pending);
    }

    // ---- v5: per-speaker languages ---------------------------------------

    #[test]
    fn a_voice_speaks_any_language_until_somebody_says_otherwise() {
        let s = store();
        let id = s.create_speaker("Kira", 1).unwrap();
        assert_eq!(s.speaker_languages(id).unwrap(), None);
        assert_eq!(s.list_speakers().unwrap()[0].languages, None);

        s.set_speaker_languages(id, Some(&["de".into(), "en".into()]))
            .unwrap();
        assert_eq!(
            s.speaker_languages(id).unwrap(),
            Some(vec!["de".to_string(), "en".to_string()])
        );
        assert_eq!(
            s.list_speakers().unwrap()[0].languages,
            Some(vec!["de".to_string(), "en".to_string()])
        );

        // An empty list is the same fact as "any" and is stored the same way,
        // so the setting has exactly one representation in the database.
        s.set_speaker_languages(id, Some(&[])).unwrap();
        assert_eq!(s.speaker_languages(id).unwrap(), None);
        s.set_speaker_languages(id, Some(&["en".into()])).unwrap();
        s.set_speaker_languages(id, None).unwrap();
        assert_eq!(s.speaker_languages(id).unwrap(), None);

        assert!(s.set_speaker_languages(4242, None).is_err());
    }

    #[test]
    fn a_merged_away_id_answers_with_the_surviving_voices_languages() {
        let s = store();
        let a = s.create_speaker("A", 1).unwrap();
        let b = s.create_speaker("B", 1).unwrap();
        s.set_speaker_languages(b, Some(&["de".into()])).unwrap();
        s.merge_speakers(a, b).unwrap();
        assert_eq!(
            s.speaker_languages(a).unwrap(),
            Some(vec!["de".to_string()])
        );
    }

    // ---- v15: highlighted people (0.12.0) --------------------------------

    #[test]
    fn a_voice_is_unhighlighted_until_somebody_picks_it_out() {
        let s = store();
        let id = s.create_speaker("Kira", 1).unwrap();
        assert_eq!(s.speaker_style(id).unwrap(), Some((None, None)));
        assert_eq!(s.list_speakers().unwrap()[0].colour, None);
        assert_eq!(s.list_speakers().unwrap()[0].icon, None);

        s.set_speaker_style(id, Some("violet"), Some("\u{1f319}"))
            .unwrap();
        assert_eq!(
            s.speaker_style(id).unwrap(),
            Some((Some("violet".into()), Some("\u{1f319}".into())))
        );
        let row = &s.list_speakers().unwrap()[0];
        assert_eq!(row.colour.as_deref(), Some("violet"));
        assert_eq!(row.icon.as_deref(), Some("\u{1f319}"));

        // The two halves are independent: a colour with no emoji is a perfectly
        // ordinary highlight, and so is the reverse.
        s.set_speaker_style(id, Some("teal"), None).unwrap();
        assert_eq!(
            s.speaker_style(id).unwrap(),
            Some((Some("teal".into()), None))
        );
        s.set_speaker_style(id, None, Some("\u{2728}")).unwrap();
        assert_eq!(
            s.speaker_style(id).unwrap(),
            Some((None, Some("\u{2728}".into())))
        );

        // …and both off is back to where the voice started. This layer takes
        // `None` as "clear"; the omit-versus-null question is `speakers.set`'s,
        // because it cannot be asked once both have collapsed into an `Option`.
        s.set_speaker_style(id, None, None).unwrap();
        assert_eq!(s.speaker_style(id).unwrap(), Some((None, None)));

        // A voice that does not exist is not a voice with no highlight.
        assert_eq!(s.speaker_style(4242).unwrap(), None);
        assert!(s.set_speaker_style(4242, Some("violet"), None).is_err());
    }

    /// The whole reason `apply_v15` rebuilt `speaker_resolved` rather than
    /// leaving it alone: a merged-away id has to answer with the SURVIVING
    /// voice's highlight. Otherwise the day after somebody merges two ids their
    /// transcript is two colours for one person — which is precisely the "two
    /// people on one row" confusion the highlight exists to end.
    #[test]
    fn a_merged_away_id_wears_the_surviving_voices_highlight() {
        let s = store();
        let src = s.upsert_source("VRChat.exe", "VRChat.exe", 1).unwrap();
        let sess = s.begin_session(src, 1_000).unwrap();
        let a = s.create_speaker("A", 1).unwrap();
        let b = s.create_speaker("B", 1).unwrap();
        let seg = s.insert_segment(sess, 1_000, 2_000, "a.wav", 0).unwrap();
        s.set_segment_speaker(seg, Some(a), Some(0.8)).unwrap();

        // Highlight the survivor, then collapse a onto it. Nothing rewrites
        // `segments.speaker_id`, so the row still points at the tombstone and
        // the view is the only thing that can make it right.
        s.set_speaker_style(b, Some("rose"), Some("\u{2728}"))
            .unwrap();
        s.merge_speakers(a, b).unwrap();

        assert_eq!(
            s.speaker_style(a).unwrap(),
            Some((Some("rose".into()), Some("\u{2728}".into()))),
            "the tombstone answers for the voice holding the rows"
        );
        let row = s.segment_row(seg).unwrap().unwrap();
        assert_eq!(row.speaker_id, Some(b));
        assert_eq!(row.speaker_colour.as_deref(), Some("rose"));
        assert_eq!(row.speaker_icon.as_deref(), Some("\u{2728}"));
    }

    #[test]
    fn the_bulk_style_map_holds_only_the_voices_somebody_picked_out() {
        let s = store();
        let a = s.create_speaker("A", 1).unwrap();
        let b = s.create_speaker("B", 1).unwrap();
        let plain = s.create_speaker("C", 1).unwrap();
        assert!(s.speaker_styles().unwrap().is_empty());

        s.set_speaker_style(a, Some("amber"), None).unwrap();
        s.set_speaker_style(b, None, Some("\u{1f680}")).unwrap();
        let map = s.speaker_styles().unwrap();
        // Half a highlight is still a highlight: the map's job is "has this
        // voice anything to say", not "has it both halves".
        assert_eq!(map.len(), 2, "{map:?}");
        assert_eq!(map[&a], (Some("amber".into()), None));
        assert_eq!(map[&b], (None, Some("\u{1f680}".into())));
        assert!(
            !map.contains_key(&plain),
            "absent is how the map says 'not highlighted'"
        );

        // Clearing takes a voice back out rather than leaving a pair of nulls
        // behind for every caller to test.
        s.set_speaker_style(a, None, None).unwrap();
        let map = s.speaker_styles().unwrap();
        assert_eq!(map.len(), 1);
        assert!(!map.contains_key(&a));
    }

    // ---- v5: label provenance --------------------------------------------

    #[test]
    fn every_way_a_label_can_arrive_says_so_on_the_row() {
        let s = store();
        let seg = a_segment(&s);
        let spk = s.create_speaker("Kira", 1).unwrap();

        s.set_segment_speaker(seg, Some(spk), Some(0.7)).unwrap();
        assert_eq!(
            s.segment_fields(seg).unwrap()["label_via"].as_deref(),
            Some(label_via::MATCH)
        );

        s.set_segment_speaker_via(seg, Some(spk), None, Some(label_via::PROXIMITY))
            .unwrap();
        assert_eq!(
            s.segment_fields(seg).unwrap()["label_via"].as_deref(),
            Some(label_via::PROXIMITY)
        );

        // A person's decision outranks and overwrites an inherited one.
        s.reassign_segment(seg, Some(spk)).unwrap();
        assert_eq!(
            s.segment_fields(seg).unwrap()["label_via"].as_deref(),
            Some(label_via::MANUAL)
        );

        // Clearing the label clears the provenance: there is nothing to
        // describe the origin of any more.
        s.set_segment_speaker(seg, None, None).unwrap();
        assert_eq!(s.segment_fields(seg).unwrap()["label_via"], None);
    }

    #[test]
    fn a_re_decode_replaces_the_words_and_the_model_that_wrote_them() {
        let s = store();
        let seg = a_segment(&s);
        s.set_segment_analysis(
            seg,
            &SegmentAnalysis {
                text: Some("das ist nicht so".into()),
                lang: Some("de".into()),
                lang_via: Some(lang_via::CLASSIFIED.into()),
                asr_model_id: Some("multilingual@1".into()),
                overlap_frac: Some(0.0),
            },
        )
        .unwrap();

        s.set_segment_text_from_redecode(seg, "that is not so", "en", "english-only@1")
            .unwrap();
        let f = s.segment_fields(seg).unwrap();
        assert_eq!(f["text"].as_deref(), Some("that is not so"));
        assert_eq!(f["lang"].as_deref(), Some("en"));
        assert_eq!(f["lang_via"].as_deref(), Some(lang_via::REDECODE));
        assert_eq!(
            f["asr_model_id"].as_deref(),
            Some("english-only@1"),
            "the row must name the model that produced the words it holds"
        );
        // The index follows the new words, not the old ones.
        assert_eq!(s.search("nicht", 10).unwrap().len(), 0);
        assert_eq!(s.search("not", 10).unwrap().len(), 1);
    }

    #[test]
    fn an_unresolvable_language_disagreement_keeps_the_words_and_drops_the_tag() {
        let s = store();
        let seg = a_segment(&s);
        s.set_segment_analysis(
            seg,
            &SegmentAnalysis {
                text: Some("that is not so".into()),
                lang: Some("en".into()),
                lang_via: Some(lang_via::CLASSIFIED.into()),
                asr_model_id: Some("multilingual@1".into()),
                overlap_frac: Some(0.0),
            },
        )
        .unwrap();
        s.mark_segment_language_mismatch(seg).unwrap();
        let f = s.segment_fields(seg).unwrap();
        assert_eq!(
            f["text"].as_deref(),
            Some("that is not so"),
            "the transcript is the only record of what was said"
        );
        assert_eq!(f["lang"], None);
        assert_eq!(f["lang_via"].as_deref(), Some(lang_via::MISMATCH));
    }

    // ---- v5: neighbours and pruning --------------------------------------

    /// Three consecutive turns in one session, and one in another.
    fn a_conversation(s: &Store) -> (i64, Vec<i64>) {
        let src = s.upsert_source("VRChat.exe", "VRChat.exe", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let ids = (0..3)
            .map(|i| {
                let t = i * 10_000_000_000;
                let id = s
                    .insert_segment(sess, t, t + 4_000_000_000, &format!("s/{i}.wav"), 0)
                    .unwrap();
                s.set_segment_analysis(
                    id,
                    &SegmentAnalysis {
                        text: Some(format!("turn {i}")),
                        ..Default::default()
                    },
                )
                .unwrap();
                id
            })
            .collect();
        (sess, ids)
    }

    #[test]
    fn neighbours_stop_at_the_session_boundary() {
        let s = store();
        let (_, ids) = a_conversation(&s);
        let other_src = s.upsert_source("Discord", "Discord", 1).unwrap();
        let other = s.begin_session(other_src, 0).unwrap();
        let elsewhere = s
            .insert_segment(other, 5_000_000_000, 6_000_000_000, "o.wav", 0)
            .unwrap();

        let before = s.segments_before(ids[2], 5).unwrap();
        assert_eq!(
            before.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![ids[1], ids[0]],
            "nearest first, and never across a session"
        );
        assert!(s.segments_before(ids[0], 5).unwrap().is_empty());
        assert!(s.segments_before(elsewhere, 5).unwrap().is_empty());

        // A soft-deleted row is not a neighbour: it is not in any read path.
        s.soft_delete_segments(&[ids[1]], 1).unwrap();
        assert_eq!(
            s.segments_before(ids[2], 5).unwrap()[0].id,
            ids[0],
            "a deleted turn does not stand between two others"
        );
    }

    #[test]
    fn pruning_refuses_your_own_voice_and_every_named_one() {
        let s = store();
        let (sess, _) = a_conversation(&s);

        // A one-off: minted, never named, one short segment.
        let grunt = s.mint_speaker(1).unwrap();
        let g = s.insert_segment(sess, 0, 900_000_000, "g.wav", 0).unwrap();
        s.set_segment_speaker(g, Some(grunt), Some(0.4)).unwrap();

        // The user's own voice, with exactly as little to show for it.
        let you = s.ensure_you_speaker(1).unwrap();
        let y = s.insert_segment(sess, 0, 900_000_000, "y.wav", 0).unwrap();
        s.set_segment_speaker_via(y, Some(you), None, Some(label_via::MIC))
            .unwrap();

        // A named voice, ditto. A name is the user saying "this one matters".
        let named = s.mint_speaker(1).unwrap();
        s.rename_speaker(named, "Kira", 2).unwrap();
        let n = s.insert_segment(sess, 0, 900_000_000, "n.wav", 0).unwrap();
        s.set_segment_speaker(n, Some(named), Some(0.4)).unwrap();

        let candidates = s.prune_candidates(1, 3_000_000_000).unwrap();
        let ids: Vec<i64> = candidates.iter().map(|c| c.id).collect();
        assert!(ids.contains(&grunt), "the one-off voice is a candidate");
        assert!(
            !ids.contains(&named),
            "a named voice is never swept, whatever its counts say"
        );
        // "You" is unnamed and thin, so the *query* offers it — the guard that
        // keeps it is the pin, which only the caller can resolve.
        assert!(ids.contains(&you));
        assert_eq!(s.you_speaker_id().unwrap(), Some(you));

        // A voice somebody merged into is never a candidate either: its own
        // counts look thin but it carries another voice's history.
        let target = s.mint_speaker(1).unwrap();
        let ghost = s.mint_speaker(1).unwrap();
        s.merge_speakers(ghost, target).unwrap();
        let after: Vec<i64> = s
            .prune_candidates(1, 3_000_000_000)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert!(!after.contains(&target), "a merge target is not a one-off");
    }

    #[test]
    fn pruning_a_voice_removes_the_identity_and_soft_deletes_its_words() {
        let s = store();
        let (sess, _) = a_conversation(&s);
        let grunt = s.mint_speaker(1).unwrap();
        let seg = s.insert_segment(sess, 0, 900_000_000, "g.wav", 0).unwrap();
        s.set_segment_speaker(seg, Some(grunt), Some(0.4)).unwrap();
        s.add_prototype(grunt, &emb("m@1", &[1.0, 0.0]), Some(seg), false, 20, 0)
            .unwrap();
        s.add_golden_sample(grunt, "goldens/000009/a.wav", 1.0)
            .unwrap();

        let report = s.prune_speaker(grunt, 500).unwrap();
        assert_eq!(report.segments, vec![seg]);
        assert_eq!(report.soft_deleted, vec![seg]);
        assert_eq!(report.prototypes, 1);
        assert_eq!(report.goldens, vec!["goldens/000009/a.wav".to_string()]);

        // The identity is gone, not tombstoned: nothing was merged anywhere.
        assert!(!s.list_speakers().unwrap().iter().any(|v| v.id == grunt));
        assert!(s.speaker_name(grunt).unwrap().is_none());
        // The segment is out of every read path, and carries no dangling id.
        assert!(
            !s.transcript(None, None)
                .unwrap()
                .iter()
                .any(|r| r.segment_id == seg)
        );
        let f = s.segment_fields(seg).unwrap();
        assert_eq!(f["speaker_id"], None);
        assert_eq!(f["label_via"], None);
        // …and the undo window still applies, exactly as a delete-by-speaker.
        assert_eq!(s.expired_soft_deletes(1_000).unwrap().len(), 1);

        // A tombstone is not a voice and cannot be pruned.
        let a = s.mint_speaker(1).unwrap();
        let b = s.mint_speaker(1).unwrap();
        s.merge_speakers(a, b).unwrap();
        assert!(s.prune_speaker(a, 500).is_err());
    }

    #[test]
    fn a_row_with_no_words_and_no_voice_is_findable_as_such() {
        let s = store();
        let (sess, _) = a_conversation(&s);
        let empty = s.insert_segment(sess, 100, 900, "e.wav", 0).unwrap();
        let worded = s.insert_segment(sess, 200, 900, "w.wav", 0).unwrap();
        let labelled = s.insert_segment(sess, 300, 900, "l.wav", 0).unwrap();
        s.set_segment_analysis(
            worded,
            &SegmentAnalysis {
                text: Some("something".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let spk = s.mint_speaker(1).unwrap();
        s.set_segment_speaker(labelled, Some(spk), Some(0.8))
            .unwrap();

        let found: Vec<i64> = s
            .empty_unlabelled_older_than(1_000)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            found,
            vec![empty],
            "only the row that is neither searchable nor attributable"
        );
        // Whitespace is not a transcript either.
        s.correct_segment_text(empty, "   ").unwrap();
        assert_eq!(s.empty_unlabelled_older_than(1_000).unwrap().len(), 1);
        // Nothing before the cutoff.
        assert!(s.empty_unlabelled_older_than(50).unwrap().is_empty());
    }

    // ---- the memory graph, Tier 1 (v6) -----------------------------------

    const SEC: i64 = 1_000_000_000;

    /// A session with `script` turns, one every five seconds, threaded through
    /// the live path exactly as the pipeline threads them. Returns the segment
    /// ids in order.
    fn a_threaded_session(s: &Store, script: &[Option<i64>]) -> (i64, Vec<i64>) {
        let src = s.upsert_source("VRChat.exe", "VRChat.exe", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let cfg = crate::config::GraphConfig::default();
        let mut ids = Vec::new();
        for (i, sp) in script.iter().enumerate() {
            let at = i as i64 * 5 * SEC;
            let id = s
                .insert_segment(sess, at, at + 3 * SEC, "x.wav", 0)
                .unwrap();
            if let Some(sp) = sp {
                s.set_segment_speaker(id, Some(*sp), Some(0.8)).unwrap();
            }
            crate::threads::assign(s, &cfg, id).unwrap();
            ids.push(id);
        }
        (sess, ids)
    }

    fn thread_of(s: &Store, segment_id: i64) -> i64 {
        s.segment_row(segment_id)
            .unwrap()
            .unwrap()
            .thread_id
            .unwrap()
    }

    #[test]
    fn threading_a_live_session_matches_the_rule_it_is_written_from() {
        let s = store();
        let a = s.create_speaker("A", 1).unwrap();
        let b = s.create_speaker("B", 1).unwrap();
        let c = s.create_speaker("C", 1).unwrap();
        let d = s.create_speaker("D", 1).unwrap();
        let (_, ids) = a_threaded_session(
            &s,
            &[
                Some(a),
                Some(b),
                Some(a),
                Some(b),
                Some(c),
                Some(d),
                Some(c),
                Some(d),
            ],
        );
        let t: Vec<i64> = ids.iter().map(|id| thread_of(&s, *id)).collect();
        assert_eq!(t[0], t[1]);
        assert_eq!(t[0], t[3]);
        assert_eq!(t[4], t[7]);
        assert_ne!(t[0], t[4]);

        // The thread rows carry the span of what is in them, which is what
        // makes "open" answerable without touching segments.
        let first = s.thread_summary(t[0]).unwrap().unwrap();
        assert_eq!(first.started_ns, 0);
        assert_eq!(first.ended_ns, 18 * SEC);
        assert_eq!(first.segments, 4);
        assert_eq!(first.participants.len(), 2);
    }

    #[test]
    fn a_threads_rows_come_back_in_order_with_a_preview() {
        let s = store();
        let a = s.create_speaker("A", 1).unwrap();
        let b = s.create_speaker("B", 1).unwrap();
        let (_, ids) = a_threaded_session(&s, &[Some(a), Some(b), Some(a)]);
        for (i, id) in ids.iter().enumerate() {
            s.correct_segment_text(*id, &format!("line {i}")).unwrap();
        }
        let tid = thread_of(&s, ids[0]);
        let rows = s.thread_rows(tid).unwrap();
        assert_eq!(
            rows.iter().map(|r| r.id).collect::<Vec<_>>(),
            ids,
            "a conversation reads in the order it happened"
        );
        assert_eq!(
            s.thread_summary(tid).unwrap().unwrap().preview.as_deref(),
            Some("line 0")
        );
    }

    /// The person page's headline claim, on a session built to make it
    /// falsifiable: A talks with B constantly and with C once.
    #[test]
    fn person_edges_rank_the_people_you_actually_talk_with() {
        let s = store();
        let a = s.create_speaker("A", 1).unwrap();
        let b = s.create_speaker("B", 1).unwrap();
        let c = s.create_speaker("C", 1).unwrap();

        let src = s.upsert_source("VRChat.exe", "VRChat.exe", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let cfg = crate::config::GraphConfig::default();
        let mut at = 0i64;
        let add = |at: &mut i64, speaker: i64, seconds: i64| {
            let id = s
                .insert_segment(sess, *at, *at + seconds * SEC, "x.wav", 0)
                .unwrap();
            s.set_segment_speaker(id, Some(speaker), Some(0.8)).unwrap();
            crate::threads::assign(&s, &cfg, id).unwrap();
            *at += (seconds + 2) * SEC;
            id
        };
        // Three A-B conversations…
        for _ in 0..3 {
            add(&mut at, a, 3);
            add(&mut at, b, 3);
            add(&mut at, a, 3);
            add(&mut at, b, 3);
            at += 60 * SEC; // past the gap: a new conversation each time
        }
        // …and one short A-C one.
        add(&mut at, a, 3);
        add(&mut at, c, 2);

        let edges = s.person_edges(a).unwrap();
        assert_eq!(
            edges.iter().map(|e| e.speaker_id).collect::<Vec<_>>(),
            vec![b, c],
            "the habit outranks the one-off"
        );
        assert_eq!(edges[0].threads, 3);
        assert_eq!(edges[1].threads, 1);
        assert_eq!(
            edges[0].speech_ns,
            6 * 3 * SEC,
            "an edge counts what the OTHER person said in the shared threads"
        );
        assert_eq!(edges[1].speech_ns, 2 * SEC);
        assert!(edges[0].last_ns < edges[1].last_ns);
        // Nothing in the roster, so nothing is claimed about the roster.
        assert!(edges.iter().all(|e| e.roster_ns.is_none()));

        // …and it reads the same from the other end.
        let from_c = s.person_edges(c).unwrap();
        assert_eq!(
            from_c.iter().map(|e| e.speaker_id).collect::<Vec<_>>(),
            vec![a]
        );

        let totals = s.person_totals(a).unwrap();
        assert_eq!(totals.segments, 7);
        assert_eq!(totals.threads, 4);
        assert_eq!(totals.sessions, 1);
        assert_eq!(totals.first_ns, Some(0));
    }

    #[test]
    fn an_edge_carries_roster_seconds_only_when_both_names_are_in_the_log() {
        let s = store();
        let kira = s.create_speaker("Kira", 1).unwrap();
        s.rename_speaker(kira, "Kira", 1).unwrap();
        let ash = s.create_speaker("Ash", 1).unwrap();
        s.rename_speaker(ash, "Ash", 1).unwrap();
        let nameless = s.create_speaker("Speaker_09", 1).unwrap();
        // All four turns in one conversation: the newcomers arrive before
        // anybody has taken a second turn, so nobody starts a thread of their
        // own and Kira shares an edge with both.
        a_threaded_session(
            &s,
            &[Some(kira), Some(nameless), Some(ash), Some(kira), Some(ash)],
        );

        // Kira was in the instance for a minute; Ash joined halfway.
        s.roster_join(Some("wrld_x"), Some("1"), "Kira", 0).unwrap();
        s.roster_leave("Kira", 60 * SEC).unwrap();
        s.roster_join(Some("wrld_x"), Some("1"), "Ash", 30 * SEC)
            .unwrap();
        s.roster_leave("Ash", 90 * SEC).unwrap();

        let edges = s.person_edges(kira).unwrap();
        let to_ash = edges.iter().find(|e| e.speaker_id == ash).unwrap();
        assert_eq!(
            to_ash.roster_ns,
            Some(30 * SEC),
            "the half-minute both were in the instance"
        );
        let to_nameless = edges.iter().find(|e| e.speaker_id == nameless).unwrap();
        assert_eq!(
            to_nameless.roster_ns, None,
            "an unnamed voice cannot be linked to a roster line, and says so"
        );
    }

    #[test]
    fn a_persons_recent_conversations_are_newest_first() {
        let s = store();
        let a = s.create_speaker("A", 1).unwrap();
        let b = s.create_speaker("B", 1).unwrap();
        let (_, ids) = a_threaded_session(&s, &[Some(a), Some(b), Some(a), Some(b)]);
        let src = s.upsert_source("VRChat.exe", "VRChat.exe", 1).unwrap();
        let sess2 = s.begin_session(src, 0).unwrap();
        let cfg = crate::config::GraphConfig::default();
        let later = s
            .insert_segment(sess2, 500 * SEC, 503 * SEC, "y.wav", 0)
            .unwrap();
        s.set_segment_speaker(later, Some(a), Some(0.8)).unwrap();
        crate::threads::assign(&s, &cfg, later).unwrap();

        let recent = s.person_threads(a, 10).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].id, thread_of(&s, later), "newest first");
        assert_eq!(recent[1].id, thread_of(&s, ids[0]));
        assert_eq!(s.person_threads(a, 1).unwrap().len(), 1, "the limit holds");
    }

    #[test]
    fn deleting_the_last_row_of_a_conversation_deletes_the_conversation() {
        let s = store();
        let a = s.create_speaker("A", 1).unwrap();
        let b = s.create_speaker("B", 1).unwrap();
        let (_, ids) = a_threaded_session(&s, &[Some(a), Some(b)]);
        let tid = thread_of(&s, ids[0]);
        assert!(s.thread_summary(tid).unwrap().is_some());

        // A soft delete keeps the thread — the rows are still undoable.
        s.soft_delete_segments(&ids, 10).unwrap();
        assert!(s.thread_summary(tid).unwrap().is_some());
        assert_eq!(s.thread_summary(tid).unwrap().unwrap().segments, 0);
        assert!(s.person_edges(a).unwrap().is_empty());

        // The purge takes the index with the transcript.
        s.purge_segments(&ids).unwrap();
        assert!(s.thread_summary(tid).unwrap().is_none());
    }

    #[test]
    fn an_unlabelled_turn_still_lands_in_the_conversation_around_it() {
        let s = store();
        let a = s.create_speaker("A", 1).unwrap();
        let b = s.create_speaker("B", 1).unwrap();
        let (_, ids) = a_threaded_session(&s, &[Some(a), Some(b), None, Some(a)]);
        let tid = thread_of(&s, ids[0]);
        assert_eq!(thread_of(&s, ids[2]), tid);
        assert_eq!(thread_of(&s, ids[3]), tid);
        // …and it is not a participant, because nobody knows who it was.
        assert_eq!(s.thread_participants(tid).unwrap(), vec![a, b]);
    }

    /// The snippet column of a search indexes one past this list, so a column
    /// added to it and not counted here silently hands every search result the
    /// wrong field — which is exactly what adding `lang_via` did once.
    #[test]
    fn the_segment_column_count_matches_the_column_list() {
        assert_eq!(
            Store::SEGMENT_COLUMNS.split(',').count(),
            Store::SEGMENT_COLUMN_COUNT
        );
    }

    // ---- the conversational language prior (0.7.7) -----------------------

    /// Stamp a segment the way the analysis leg would have.
    fn stamp(s: &Store, id: i64, text: &str, lang: Option<&str>, via: &str) {
        s.set_segment_analysis(
            id,
            &SegmentAnalysis {
                text: Some(text.into()),
                lang: lang.map(str::to_string),
                lang_via: Some(via.into()),
                asr_model_id: Some("test@1".into()),
                overlap_frac: Some(0.02),
            },
        )
        .unwrap();
    }

    #[test]
    fn a_threads_language_evidence_is_its_own_and_newest_first() {
        let s = store();
        let a = s.create_speaker("A", 1).unwrap();
        let b = s.create_speaker("B", 1).unwrap();
        // Two conversations running beside each other, one German and one
        // English — which is the case the whole feature exists for, and the
        // case a per-session or per-speaker rule gets wrong.
        let c = s.create_speaker("C", 1).unwrap();
        let d = s.create_speaker("D", 1).unwrap();
        let (_, ids) = a_threaded_session(
            &s,
            &[
                Some(a),
                Some(b),
                Some(a),
                Some(b),
                Some(c),
                Some(d),
                Some(c),
                Some(d),
            ],
        );
        let de_thread = thread_of(&s, ids[0]);
        let en_thread = thread_of(&s, ids[4]);
        assert_ne!(de_thread, en_thread);
        for id in &ids[..4] {
            stamp(&s, *id, "das ist so", Some("de"), lang_via::CLASSIFIED);
        }
        for id in &ids[4..] {
            stamp(&s, *id, "that is so", Some("en"), lang_via::CLASSIFIED);
        }

        // Each conversation sees only its own turns. This is the whole point:
        // the room is bilingual and neither thread is.
        assert_eq!(
            s.thread_language_stamps(de_thread, 0, 10).unwrap(),
            vec!["de"; 4]
        );
        assert_eq!(
            s.thread_language_stamps(en_thread, 0, 10).unwrap(),
            vec!["en"; 4]
        );
        // Newest first, and capped.
        stamp(&s, ids[3], "that is so", Some("en"), lang_via::CLASSIFIED);
        assert_eq!(
            s.thread_language_stamps(de_thread, 0, 2).unwrap(),
            vec!["en", "de"],
            "the most recent two, most recent first"
        );
        // The turn being decided never votes on itself.
        assert_eq!(
            s.thread_language_stamps(de_thread, ids[3], 10).unwrap(),
            vec!["de"; 3]
        );
    }

    #[test]
    fn an_inherited_stamp_is_not_evidence_for_the_next_one() {
        // Otherwise three real German turns would inherit their way to a
        // hundred and the hundredth would look as certain as the first.
        let s = store();
        let a = s.create_speaker("A", 1).unwrap();
        let (_, ids) = a_threaded_session(&s, &[Some(a), Some(a), Some(a)]);
        let tid = thread_of(&s, ids[0]);
        stamp(&s, ids[0], "das ist so", Some("de"), lang_via::CLASSIFIED);
        stamp(&s, ids[1], "okay", None, lang_via::CLASSIFIED);
        s.set_segment_language_from_context(ids[1], "de").unwrap();
        stamp(&s, ids[2], "das ist so", Some("de"), lang_via::CLASSIFIED);

        let f = s.segment_fields(ids[1]).unwrap();
        assert_eq!(f["lang"].as_deref(), Some("de"));
        assert_eq!(f["lang_via"].as_deref(), Some(lang_via::CONTEXT));
        assert_eq!(
            f["text"].as_deref(),
            Some("okay"),
            "an inheritance changes the language and nothing else"
        );
        assert_eq!(
            s.thread_language_stamps(tid, 0, 10).unwrap(),
            vec!["de", "de"],
            "the inherited one is an echo, not a vote"
        );
    }

    #[test]
    fn a_soft_deleted_turn_stops_voting() {
        let s = store();
        let a = s.create_speaker("A", 1).unwrap();
        let (_, ids) = a_threaded_session(&s, &[Some(a), Some(a)]);
        let tid = thread_of(&s, ids[0]);
        stamp(&s, ids[0], "das ist so", Some("de"), lang_via::CLASSIFIED);
        stamp(&s, ids[1], "das ist so", Some("de"), lang_via::CLASSIFIED);
        s.soft_delete_segments(&[ids[1]], 1).unwrap();
        assert_eq!(s.thread_language_stamps(tid, 0, 10).unwrap(), vec!["de"]);
    }

    #[test]
    fn the_language_subject_reduces_a_declaration_to_the_one_that_can_act() {
        let s = store();
        let a = s.create_speaker("A", 1).unwrap();
        let (_, ids) = a_threaded_session(&s, &[Some(a)]);
        stamp(&s, ids[0], "das ist so", Some("de"), lang_via::CLASSIFIED);

        let subject = s.language_subject(ids[0]).unwrap().unwrap();
        assert_eq!(subject.thread_id, Some(thread_of(&s, ids[0])));
        assert_eq!(subject.text.as_deref(), Some("das ist so"));
        assert_eq!(subject.lang_via.as_deref(), Some(lang_via::CLASSIFIED));
        assert_eq!(subject.declared, None, "nobody has said anything");

        s.set_speaker_languages(a, Some(&["en".to_string()]))
            .unwrap();
        assert_eq!(
            s.language_subject(ids[0])
                .unwrap()
                .unwrap()
                .declared
                .as_deref(),
            Some("en")
        );
        // Bilingual is not a declaration anything can act on, so it reduces to
        // the same "nothing" as no declaration at all.
        s.set_speaker_languages(a, Some(&["de".to_string(), "en".to_string()]))
            .unwrap();
        assert_eq!(s.language_subject(ids[0]).unwrap().unwrap().declared, None);

        // A merged-away voice answers with the surviving voice's declaration.
        let b = s.create_speaker("B", 1).unwrap();
        s.set_speaker_languages(b, Some(&["de".to_string()]))
            .unwrap();
        s.merge_speakers(a, b).unwrap();
        assert_eq!(
            s.language_subject(ids[0])
                .unwrap()
                .unwrap()
                .declared
                .as_deref(),
            Some("de")
        );
        assert_eq!(s.language_subject(999_999).unwrap(), None);
    }

    #[test]
    fn the_mismatch_backlog_is_oldest_first_and_only_what_can_be_re_read() {
        let s = store();
        let a = s.create_speaker("A", 1).unwrap();
        let (_, ids) = a_threaded_session(&s, &[Some(a), Some(a), Some(a), Some(a)]);
        for id in &ids {
            stamp(&s, *id, "that is so", Some("en"), lang_via::CLASSIFIED);
        }
        // Three flagged, one of them with its audio already aged out.
        for id in &ids[..3] {
            s.mark_segment_language_mismatch(*id).unwrap();
        }
        s.forget_audio(&[ids[1]]).unwrap();

        assert_eq!(s.language_mismatch_counts().unwrap(), (3, 2));
        assert_eq!(
            s.language_mismatch_backlog(10)
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            vec![ids[0], ids[2]],
            "oldest first, and never a row there is nothing left to re-read"
        );
        // Bounded, so a repair can walk it a batch at a time.
        assert_eq!(s.language_mismatch_backlog(1).unwrap().len(), 1);

        // Settling one takes it off the list without touching the others.
        s.set_segment_language(ids[0], "en", lang_via::CLASSIFIED)
            .unwrap();
        assert_eq!(s.language_mismatch_counts().unwrap(), (2, 1));
    }

    // ---- 0.11.0: source-aware identity -----------------------------------

    /// One store with two voices and three sources, arranged the way the live
    /// database actually is: one voice heard only through an app, one heard
    /// only on a second app, and the user's own voice on the microphone.
    fn a_two_source_store() -> (Store, i64, i64, i64, i64, i64) {
        let s = store();
        let discord = s
            .upsert_source_kind("Discord", "Chromium", KIND_APP, 0)
            .unwrap();
        let vesktop = s
            .upsert_source_kind("vesktop", "Chromium", KIND_APP, 0)
            .unwrap();
        let mic = s
            .upsert_source_kind("mic", "Microphone", KIND_MIC, 0)
            .unwrap();
        let sd = s.begin_session(discord, 0).unwrap();
        let sv = s.begin_session(vesktop, 0).unwrap();
        let sm = s.begin_session(mic, 0).unwrap();

        let rowan = s.create_speaker("Rowan", 0).unwrap();
        let albe = s.create_speaker("Albe", 0).unwrap();
        let you = s.ensure_you_speaker(0).unwrap();

        let sec = 1_000_000_000i64;
        let mut t = sec;
        let put = |session: i64, speaker: i64, at: i64| {
            let id = s
                .insert_segment(session, at, at + sec, "x.wav", at)
                .unwrap();
            s.set_segment_speaker_via(id, Some(speaker), Some(0.9), Some(label_via::MATCH))
                .unwrap();
            id
        };
        for _ in 0..3 {
            put(sd, rowan, t);
            t += 10 * sec;
        }
        for _ in 0..2 {
            put(sv, albe, t);
            t += 10 * sec;
        }
        let mic_seg = put(sm, you, t);
        (s, rowan, albe, you, discord, mic_seg)
    }

    #[test]
    fn a_voice_source_history_counts_only_where_it_was_actually_heard() {
        let (s, rowan, albe, you, discord, _) = a_two_source_store();

        let h = s.speaker_sources(rowan).unwrap();
        assert_eq!(h.len(), 1, "Rowan has been heard on one source: {h:?}");
        assert_eq!(h[0].match_key, "Discord");
        assert_eq!(h[0].segments, 3);
        assert_eq!(h[0].source_id, discord);
        assert_eq!(h[0].kind, KIND_APP);

        assert_eq!(s.speaker_sources(albe).unwrap()[0].match_key, "vesktop");
        let y = s.speaker_sources(you).unwrap();
        assert_eq!(y[0].kind, KIND_MIC);

        // The matrix says the same thing for everyone at once.
        let m = s.speaker_source_matrix().unwrap();
        assert_eq!(m[&rowan][0].segments, 3);
        assert_eq!(m[&albe][0].segments, 2);
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn a_deleted_turn_stops_counting_towards_a_source() {
        let (s, rowan, ..) = a_two_source_store();
        let first = s
            .conn
            .query_row(
                "SELECT id FROM segments WHERE speaker_id = ?1 ORDER BY id LIMIT 1",
                params![rowan],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        s.soft_delete_segments(&[first], 1).unwrap();
        assert_eq!(s.speaker_sources(rowan).unwrap()[0].segments, 2);
    }

    #[test]
    fn the_standings_split_turns_here_from_turns_anywhere() {
        let (s, rowan, albe, _, discord, _) = a_two_source_store();
        let st = s.source_standings(discord).unwrap();
        let of = |id: i64| *st.iter().find(|x| x.speaker_id == id).unwrap();
        assert_eq!((of(rowan).on_source, of(rowan).total), (3, 3));
        // Albe has history, all of it somewhere else — the whole point.
        assert_eq!((of(albe).on_source, of(albe).total), (0, 2));
    }

    #[test]
    fn a_segment_knows_which_source_it_came_from() {
        let (s, _, _, _, _, mic_seg) = a_two_source_store();
        let src = s.segment_source(mic_seg).unwrap().unwrap();
        assert_eq!(src.match_key, "mic");
        assert_eq!(src.kind, KIND_MIC);
        assert!(src.t_end_ns > src.t_start_ns);
        assert!(s.segment_source(999_999).unwrap().is_none());
    }

    #[test]
    fn unassigning_a_label_clears_the_name_and_keeps_the_evidence() {
        let (s, _, _, _, _, mic_seg) = a_two_source_store();
        s.store_embedding(mic_seg, &emb("m@1", &[1.0, 0.0]))
            .unwrap();
        assert!(s.unassign_segment_speaker(mic_seg).unwrap());

        let row = s.segment_row(mic_seg).unwrap().unwrap();
        assert_eq!(row.speaker_id, None);
        assert_eq!(row.match_score, None);
        // The embedding is the thing a later reassignment argues from, so it
        // must survive the row losing its name.
        assert!(s.segment_embedding(mic_seg).unwrap().is_some());
        // Idempotent, and honest about having done nothing the second time.
        assert!(!s.unassign_segment_speaker(999_999).unwrap());
    }

    #[test]
    fn the_audit_flags_a_cross_source_label_and_only_the_first_of_its_run() {
        let (s, rowan, _, _, _, _) = a_two_source_store();
        let cfg = crate::config::IdentityConfig {
            // Three turns of history is all this fixture has.
            foreign_after_segments: 3,
            ..Default::default()
        };
        // Nothing yet: every label so far was made on the source that voice
        // already lived on.
        assert!(
            crate::identity_prior::audit(&s, &cfg)
                .unwrap()
                .foreign
                .is_empty()
        );

        // Now Rowan — three Discord turns and none on vesktop — wins two
        // vesktop turns in a row, exactly as it did in the live database at
        // 0.361 (FINDINGS §17).
        let vesktop = s
            .upsert_source_kind("vesktop", "Chromium", KIND_APP, 0)
            .unwrap();
        let sv = s.begin_session(vesktop, 0).unwrap();
        let sec = 1_000_000_000i64;
        for i in 0..2 {
            let at = 1_000 * sec + i * 10 * sec;
            let id = s.insert_segment(sv, at, at + sec, "x.wav", at).unwrap();
            s.set_segment_speaker_via(id, Some(rowan), Some(0.361), Some(label_via::MATCH))
                .unwrap();
        }

        let report = crate::identity_prior::audit(&s, &cfg).unwrap();
        assert_eq!(
            report.foreign.len(),
            1,
            "a run is flagged once, at its head: {:?}",
            report.foreign
        );
        let f = &report.foreign[0];
        assert_eq!(f.speaker_id, rowan);
        assert_eq!(f.source, "vesktop");
        // Written as f32 by the pipeline, read back as f64 by the audit.
        assert!(f.match_score.is_some_and(|s| (s - 0.361).abs() < 1e-6));
        assert_eq!(f.followed_by, 1, "and it says how long the run got");
        assert_eq!(report.considered, 8);
        // The matrix is the report's other half and carries every voice.
        assert_eq!(report.matrix.len(), 3);
    }

    #[test]
    fn your_own_voice_is_never_flagged_however_many_sources_it_appears_on() {
        let (s, _, _, you, _, _) = a_two_source_store();
        let cfg = crate::config::IdentityConfig {
            foreign_after_segments: 1,
            ..Default::default()
        };
        let vrc = s
            .upsert_source_kind("VRChat.exe", "VRChat", KIND_APP, 0)
            .unwrap();
        let sv = s.begin_session(vrc, 0).unwrap();
        let sec = 1_000_000_000i64;
        let id = s
            .insert_segment(sv, 9_000 * sec, 9_001 * sec, "x.wav", 0)
            .unwrap();
        s.set_segment_speaker_via(id, Some(you), None, Some(label_via::MIC))
            .unwrap();
        let report = crate::identity_prior::audit(&s, &cfg).unwrap();
        assert!(
            report.foreign.iter().all(|f| f.speaker_id != you),
            "the microphone follows the user everywhere: {:?}",
            report.foreign
        );
    }

    // ---- 0.11.0: learned identity ----------------------------------------

    #[test]
    fn a_store_where_nothing_was_learned_answers_with_the_globals() {
        let s = store();
        let a = s.mint_speaker(0).unwrap();
        assert!(s.learned_thresholds().unwrap().is_empty());
        let t = s.threshold_table((0.35, 0.0)).unwrap();
        assert_eq!(t.for_speaker(a), (0.35, 0.0));
        assert!(t.is_empty());
        assert!(s.installed_projection().unwrap().is_none());
    }

    // ---- 0.12.0: the learned aggregate, and prototype repair --------------

    #[test]
    fn a_store_where_nothing_was_learned_scores_a_voice_on_its_best_prototype() {
        let s = store();
        assert_eq!(
            s.learned_aggregate().unwrap(),
            crate::calib::Aggregate::Max,
            "an install that has learned nothing must behave exactly as 0.11.8 did"
        );
    }

    #[test]
    fn a_learned_aggregate_round_trips_and_can_be_taken_back() {
        let s = store();
        s.set_learned_aggregate(crate::calib::Aggregate::TopK(3))
            .unwrap();
        assert_eq!(
            s.learned_aggregate().unwrap(),
            crate::calib::Aggregate::TopK(3)
        );
        assert!(s.clear_learned_aggregate().unwrap());
        assert_eq!(s.learned_aggregate().unwrap(), crate::calib::Aggregate::Max);
        assert!(!s.clear_learned_aggregate().unwrap(), "already gone");
    }

    #[test]
    fn a_setting_nothing_wrote_is_not_a_reason_to_stop_labelling() {
        // A hand-edited or future-version value must not become an error the
        // ladder has to handle. It falls back to the shipped behaviour.
        let s = store();
        s.set_setting(AGGREGATE_KEY, "top-99999999999999999999")
            .unwrap();
        assert_eq!(s.learned_aggregate().unwrap(), crate::calib::Aggregate::Max);
    }

    #[test]
    fn the_repair_finds_a_prototype_whose_own_turn_was_somebody_else() {
        let s = store();
        let src = s.upsert_source("Discord", "Discord", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let mine = s.mint_speaker(0).unwrap();
        let theirs = s.mint_speaker(0).unwrap();
        let you = s.mint_speaker(0).unwrap();
        for (u, sp) in [("them", theirs), ("me", you)] {
            s.upsert_discord_user(u, u, 0).unwrap();
            s.set_discord_link(u, Some(sp), Some(truth_via::MANUAL), 0)
                .unwrap();
        }
        s.set_setting(YOU_SPEAKER_KEY, &you.to_string()).unwrap();

        let v = Embedding::new("m@1", vec![1.0, 0.0]);
        // 1: condemned — Discord says this turn was `theirs`, in full.
        let bad = s.insert_segment(sess, 10, 20, "a.wav", 0).unwrap();
        s.set_segment_truth(bad, Some("them"), truth_verdict::SINGLE, Some(0.95))
            .unwrap();
        let bad_p = s.add_prototype(mine, &v, Some(bad), false, 20, 0).unwrap();
        // 2: safe — the verdict agrees with the prototype's owner.
        let ok = s.insert_segment(sess, 30, 40, "b.wav", 0).unwrap();
        s.set_segment_truth(ok, Some("them"), truth_verdict::SINGLE, Some(0.95))
            .unwrap();
        let ok_p = s.add_prototype(theirs, &v, Some(ok), false, 20, 0).unwrap();
        // 3: safe — a `partial` verdict is one voice under the bar, not proof.
        let weak = s.insert_segment(sess, 50, 60, "c.wav", 0).unwrap();
        s.set_segment_truth(weak, Some("them"), truth_verdict::PARTIAL, Some(0.4))
            .unwrap();
        let weak_p = s.add_prototype(mine, &v, Some(weak), false, 20, 0).unwrap();
        // 4: safe — the verdict names the user's OWN account, and a Discord
        // client does not play your microphone back to you (§17). The same
        // rule that keeps those rows out of the headline keeps them from
        // condemning a prototype.
        let own = s.insert_segment(sess, 70, 80, "d.wav", 0).unwrap();
        s.set_segment_truth(own, Some("me"), truth_verdict::SINGLE, Some(0.95))
            .unwrap();
        let own_p = s.add_prototype(mine, &v, Some(own), false, 20, 0).unwrap();

        let found = s.condemned_prototypes("m@1").unwrap();
        let ids: Vec<i64> = found.iter().map(|c| c.prototype_id).collect();
        assert_eq!(ids, vec![bad_p.unwrap()], "{found:?}");
        assert_eq!(found[0].owner, mine);
        assert_eq!(found[0].truth_speaker, theirs);
        for other in [ok_p, weak_p, own_p] {
            assert!(!ids.contains(&other.unwrap()));
        }
    }

    // ---- 0.12.2: mint bursts and the phantoms they leave -----------------

    /// The 2026-09-04 shape, small: a real voice, a linked Discord account, and
    /// a run of unnamed voices minted from that person's turns.
    fn a_store_with_a_mint_burst() -> (Store, i64, Vec<i64>) {
        let s = store();
        let src = s.upsert_source("Discord", "Discord", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let rowan = s.create_speaker("Rowan", 0).unwrap();
        s.upsert_discord_user("rowan", "Rowan", 0).unwrap();
        s.set_discord_link("rowan", Some(rowan), Some(truth_via::MANUAL), 0)
            .unwrap();
        let v = Embedding::new("m@1", vec![1.0, 0.0]);
        let min = 60_000_000_000i64;
        let mut phantoms = Vec::new();
        for i in 0..4 {
            let at = i * min;
            let p = s.mint_speaker(at).unwrap();
            let seg = s.insert_segment(sess, at, at + 1, "a.wav", at).unwrap();
            s.set_segment_speaker(seg, Some(p), Some(0.61)).unwrap();
            s.set_segment_truth(seg, Some("rowan"), truth_verdict::SINGLE, Some(0.95))
                .unwrap();
            s.add_prototype(p, &v, Some(seg), false, 20, at).unwrap();
            phantoms.push(p);
        }
        (s, rowan, phantoms)
    }

    #[test]
    fn a_run_of_mints_from_one_person_is_a_burst() {
        let (s, rowan, phantoms) = a_store_with_a_mint_burst();
        let bursts = s.mint_bursts(3, 10 * 60_000_000_000).unwrap();
        assert_eq!(bursts.len(), 1, "{bursts:?}");
        let b = &bursts[0];
        assert_eq!(
            b.minted.iter().map(|(id, ..)| *id).collect::<Vec<_>>(),
            phantoms
        );
        assert_eq!(b.source_key, "Discord");
        assert_eq!(b.seeds_with_a_verdict, 4);
        assert_eq!(
            b.seeds_say,
            vec![(rowan, "Rowan".to_string(), 4)],
            "every seed turn is one voice the bank already had"
        );
        assert!(b.merged_into.is_empty(), "nobody has cleaned it up yet");
    }

    #[test]
    fn a_burst_already_merged_away_by_hand_is_still_reported() {
        // §44's 24 inherited prototypes: a burst whose voices somebody folded
        // into one voice keeps no rows of its own, so the seed-verdict column
        // goes blank and the merge target is the only evidence left.
        let (s, rowan, phantoms) = a_store_with_a_mint_burst();
        for p in &phantoms {
            s.merge_speakers(*p, rowan).unwrap();
        }
        let bursts = s.mint_bursts(3, 10 * 60_000_000_000).unwrap();
        assert_eq!(bursts.len(), 1, "{bursts:?}");
        assert_eq!(bursts[0].seeds_with_a_verdict, 0, "their rows moved out");
        assert_eq!(
            bursts[0].merged_into,
            vec![(rowan, "Rowan".to_string(), phantoms.len())]
        );
    }

    #[test]
    fn voices_minted_hours_apart_are_not_a_burst() {
        // The report must not cry wolf over a lobby that grew over an evening.
        let (s, _, _) = a_store_with_a_mint_burst();
        assert!(s.mint_bursts(3, 30_000_000_000).unwrap().is_empty());
        // Nor over two arrivals, however close together.
        assert!(s.mint_bursts(5, 10 * 60_000_000_000).unwrap().is_empty());
    }

    #[test]
    fn a_phantom_is_named_with_the_voice_its_rows_belong_to() {
        let (s, rowan, phantoms) = a_store_with_a_mint_burst();
        let found = s.phantom_voices("m@1").unwrap();
        assert_eq!(found.len(), phantoms.len(), "{found:?}");
        for p in &found {
            assert_eq!(p.prototypes, 1);
            assert_eq!(p.condemned, 1, "its one prototype is a Rowan recording");
            assert_eq!(p.target, Some((rowan, "Rowan".to_string())), "{p:?}");
            assert_eq!(p.refused, None);
        }
    }

    #[test]
    fn a_voice_with_a_prototype_that_stands_up_is_left_alone() {
        // The condition is *every* prototype condemned. One that ground truth
        // does not contradict is a voice, however few turns it has.
        let (s, _, phantoms) = a_store_with_a_mint_burst();
        let v = Embedding::new("m@1", vec![0.0, 1.0]);
        s.add_prototype(phantoms[0], &v, None, false, 20, 0)
            .unwrap();
        let found = s.phantom_voices("m@1").unwrap();
        let it = found.iter().find(|p| p.speaker_id == phantoms[0]).unwrap();
        assert_eq!(it.target, None);
        assert!(
            it.refused
                .as_deref()
                .is_some_and(|r| r.contains("stand up"))
        );
    }

    #[test]
    fn a_named_voice_is_never_a_phantom() {
        let (s, _, phantoms) = a_store_with_a_mint_burst();
        s.rename_speaker(phantoms[0], "Somebody", 0).unwrap();
        let found = s.phantom_voices("m@1").unwrap();
        assert!(
            found.iter().all(|p| p.speaker_id != phantoms[0]),
            "{found:?}"
        );
    }

    #[test]
    fn absorbing_a_phantom_gives_its_turns_back_and_says_what_they_were() {
        let (s, rowan, phantoms) = a_store_with_a_mint_burst();
        let ph = phantoms[0];
        let seg = s
            .conn
            .query_row(
                "SELECT id FROM segments WHERE speaker_id = ?1",
                params![ph],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        let (prior, merge) = s.absorb_phantom(ph, rowan).unwrap();
        assert_eq!(prior.len(), 1);
        assert_eq!(prior[0].segment_id, seg);
        assert_eq!(prior[0].speaker_id, Some(ph));
        assert_eq!(prior[0].label_via.as_deref(), Some(label_via::MATCH));
        assert!(
            prior[0]
                .match_score
                .is_some_and(|v| (v - 0.61).abs() < 1e-6)
        );
        assert_eq!(merge.into, rowan);
        assert_eq!(merge.prototypes, 1, "its prototype comes too");

        let row = s.segment_row(seg).unwrap().unwrap();
        assert_eq!(row.speaker_id, Some(rowan));
        assert_eq!(row.label_via.as_deref(), Some(label_via::TRUTH));
        assert_eq!(row.match_score, None, "there was no comparison to score");
        assert_eq!(s.resolve_speaker(ph).unwrap(), rowan);
    }

    #[test]
    fn a_row_with_its_own_verdict_follows_that_verdict_and_not_the_majority() {
        // The reason the rule is a majority and not unanimity: a phantom that
        // stole 168 of one person's turns and 2 of another's is still the first
        // person's turns, and the two dissenters know who they are.
        let (s, rowan, phantoms) = a_store_with_a_mint_burst();
        let aspen = s.create_speaker("Aspen", 0).unwrap();
        s.upsert_discord_user("aspen", "Aspen", 0).unwrap();
        s.set_discord_link("aspen", Some(aspen), Some(truth_via::MANUAL), 0)
            .unwrap();
        let src = s.upsert_source("Discord", "Discord", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        // Two more of Rowan's turns under the same phantom, so it holds three
        // of his and one of hers — the live Speaker_71's 168-to-2, in small.
        for t in [1001i64, 1002] {
            let g = s.insert_segment(sess, t, t + 1, "h.wav", t).unwrap();
            s.set_segment_speaker(g, Some(phantoms[0]), Some(0.6))
                .unwrap();
            s.set_segment_truth(g, Some("rowan"), truth_verdict::SINGLE, Some(0.95))
                .unwrap();
        }
        let odd = s.insert_segment(sess, 999, 1000, "z.wav", 999).unwrap();
        s.set_segment_speaker(odd, Some(phantoms[0]), Some(0.6))
            .unwrap();
        s.set_segment_truth(odd, Some("aspen"), truth_verdict::SINGLE, Some(0.95))
            .unwrap();

        let it = s
            .phantom_voices("m@1")
            .unwrap()
            .into_iter()
            .find(|p| p.speaker_id == phantoms[0])
            .unwrap();
        assert_eq!(
            it.says,
            vec![(rowan, "Rowan".into(), 3), (aspen, "Aspen".into(), 1)]
        );
        assert_eq!(it.target, Some((rowan, "Rowan".to_string())));
        s.absorb_phantom(phantoms[0], rowan).unwrap();
        assert_eq!(
            s.segment_row(odd).unwrap().unwrap().speaker_id,
            Some(aspen),
            "Aspen's row went to Aspen, not to the merge target"
        );
    }

    #[test]
    fn a_phantom_no_voice_holds_a_majority_of_is_refused() {
        let (s, _, phantoms) = a_store_with_a_mint_burst();
        let aspen = s.create_speaker("Aspen", 0).unwrap();
        s.upsert_discord_user("aspen", "Aspen", 0).unwrap();
        s.set_discord_link("aspen", Some(aspen), Some(truth_via::MANUAL), 0)
            .unwrap();
        let src = s.upsert_source("Discord", "Discord", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        // One row each way: the phantom already holds one turn Discord calls
        // Rowan, and now one it calls Aspen.
        let odd = s.insert_segment(sess, 999, 1000, "z.wav", 999).unwrap();
        s.set_segment_speaker(odd, Some(phantoms[0]), Some(0.6))
            .unwrap();
        s.set_segment_truth(odd, Some("aspen"), truth_verdict::SINGLE, Some(0.95))
            .unwrap();
        let it = s
            .phantom_voices("m@1")
            .unwrap()
            .into_iter()
            .find(|p| p.speaker_id == phantoms[0])
            .unwrap();
        assert_eq!(it.covered, 2);
        assert_eq!(it.target, None, "{it:?}");
        assert!(
            it.refused
                .as_deref()
                .is_some_and(|r| r.contains("majority")),
            "{it:?}"
        );
        let _ = aspen;
    }

    #[test]
    fn a_golden_prototype_is_never_condemned_by_a_verdict() {
        // Hand-enrolled audio is the user's own word about who this is, and it
        // outranks anything inferred from a Discord speaking ring.
        let s = store();
        let src = s.upsert_source("Discord", "Discord", 1).unwrap();
        let sess = s.begin_session(src, 0).unwrap();
        let mine = s.mint_speaker(0).unwrap();
        let theirs = s.mint_speaker(0).unwrap();
        s.upsert_discord_user("them", "them", 0).unwrap();
        s.set_discord_link("them", Some(theirs), Some(truth_via::MANUAL), 0)
            .unwrap();
        let seg = s.insert_segment(sess, 10, 20, "a.wav", 0).unwrap();
        s.set_segment_truth(seg, Some("them"), truth_verdict::SINGLE, Some(0.95))
            .unwrap();
        let g = Embedding::new("m@1", vec![1.0, 0.0]);
        s.add_prototype(mine, &g, Some(seg), true, 20, 0).unwrap();
        assert!(s.condemned_prototypes("m@1").unwrap().is_empty());
    }

    #[test]
    fn deleting_prototypes_reports_what_it_actually_removed() {
        let s = store();
        let a = s.mint_speaker(0).unwrap();
        let v = Embedding::new("m@1", vec![1.0, 0.0]);
        let p = s.add_prototype(a, &v, None, false, 20, 0).unwrap().unwrap();
        assert_eq!(s.delete_prototypes(&[p, p + 9_000]).unwrap(), 1);
        assert!(s.prototypes("m@1").unwrap().is_empty());
    }

    #[test]
    fn a_learned_threshold_round_trips_with_its_provenance() {
        let s = store();
        let a = s.mint_speaker(0).unwrap();
        let b = s.mint_speaker(0).unwrap();
        assert!(
            s.set_learned_threshold(a, 0.52, 0.04, truth_via::LEARNED, 61, 12_345)
                .unwrap()
        );
        let rows = s.learned_thresholds().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].speaker_id, a);
        assert!((rows[0].threshold - 0.52).abs() < 1e-6);
        assert!((rows[0].margin - 0.04).abs() < 1e-6);
        assert_eq!(rows[0].via, truth_via::LEARNED);
        assert_eq!(rows[0].n, 61);
        assert_eq!(rows[0].at_ns, 12_345);
        // And the voice nobody learned anything about still gets the global.
        let t = s.threshold_table((0.35, 0.0)).unwrap();
        assert_eq!(t.for_speaker(b), (0.35, 0.0));
    }

    #[test]
    fn clearing_counts_only_the_voices_that_had_a_value() {
        let s = store();
        let a = s.mint_speaker(0).unwrap();
        for _ in 0..4 {
            s.mint_speaker(0).unwrap();
        }
        s.set_learned_threshold(a, 0.5, 0.0, truth_via::LEARNED, 40, 1)
            .unwrap();
        // Five voices, one learned value: clearing is one voice, not five.
        assert_eq!(s.clear_learned_thresholds(None).unwrap(), 1);
        assert!(s.learned_thresholds().unwrap().is_empty());
        assert_eq!(s.clear_learned_thresholds(None).unwrap(), 0);
    }

    #[test]
    fn a_merged_away_voice_stops_carrying_a_learned_threshold() {
        // The table the ladder reads must never name a tombstone: the surviving
        // voice is the one segments point at, and a threshold on the dead id
        // would apply to nothing while looking like it applied to something.
        let s = store();
        let a = s.mint_speaker(0).unwrap();
        let b = s.mint_speaker(0).unwrap();
        s.set_learned_threshold(a, 0.5, 0.0, truth_via::LEARNED, 40, 1)
            .unwrap();
        s.merge_speakers(a, b).unwrap();
        assert!(s.learned_thresholds().unwrap().is_empty());
    }

    #[test]
    fn a_merge_leaves_a_bank_over_its_cap_and_the_audit_can_see_it() {
        // `add_prototype` enforces `max_prototypes`; `merge_speakers` moves
        // prototypes with a bare UPDATE and nothing re-applies it afterwards.
        // On the 2026-09-04 archive that is how Rowan came to hold 47
        // prototypes against a cap of 20 — and the 24 it inherited match
        // Rowan's own ground-truth turns at 0.167 where the enrolled ones
        // manage 0.359, which is the whole of what looked like drift
        // (FINDINGS §44). Nothing here deletes: the cap is measured and
        // reported, because every automatic trim was measured and refused.
        let s = store();
        let a = s.mint_speaker(0).unwrap();
        let b = s.mint_speaker(0).unwrap();
        for i in 0..3 {
            s.add_prototype(a, &emb("m@1", &[1.0, i as f32, 0.0]), None, false, 3, i)
                .unwrap();
            s.add_prototype(b, &emb("m@1", &[0.0, i as f32, 1.0]), None, false, 3, i)
                .unwrap();
        }
        assert_eq!(s.oversized_banks(3).unwrap(), vec![], "nothing is over yet");

        s.merge_speakers(a, b).unwrap();
        assert_eq!(
            s.prototype_count(b).unwrap(),
            6,
            "the merge moved all three"
        );
        assert_eq!(
            s.oversized_banks(3).unwrap(),
            vec![(b, format!("Speaker_{b:02}"), 6)],
            "the surviving voice is over the cap and the audit names it"
        );
        assert_eq!(
            s.oversized_banks(6).unwrap(),
            vec![],
            "at the cap is not over it"
        );
    }

    #[test]
    fn a_projection_round_trips_through_the_store() {
        let s = store();
        let rows: Vec<crate::calib::Labelled> = (0..20)
            .map(|i| crate::calib::Labelled {
                class: (i % 2) as i64,
                v: vec![i as f32 * 0.01, 1.0 - i as f32 * 0.01, 0.5],
            })
            .collect();
        let p =
            crate::calib::fit_projection("m@1", &rows, crate::calib::Whitening::default()).unwrap();
        s.install_projection(&p, 7, 999).unwrap();
        let (back, version, at) = s.installed_projection().unwrap().unwrap();
        assert_eq!(version, 7);
        assert_eq!(at, 999);
        assert_eq!(back.dim, p.dim);
        assert_eq!(back.a, p.a);
        assert_eq!(back.mean, p.mean);
        assert_eq!(back.whitening, p.whitening);
        assert_eq!(back.n_rows, p.n_rows);
        // A second install replaces rather than accumulating: "which space is
        // live" must not be a question with two answers.
        s.install_projection(&p, 8, 1000).unwrap();
        assert_eq!(s.installed_projection().unwrap().unwrap().1, 8);
        assert!(s.clear_projection().unwrap());
        assert!(s.installed_projection().unwrap().is_none());
        assert!(!s.clear_projection().unwrap());
    }

    // ---- 0.12.4: cutting a turn where the speaker changes -----------------

    /// One `overlap` turn with a transcript, a speaker and a vector — the
    /// state a resplit has to take apart and be able to put back.
    fn a_mixed_turn(s: &Store, verdict: &str) -> i64 {
        let src = s.upsert_source("Discord", "Discord", 1).unwrap();
        let sess = s.begin_session(src, 1_000).unwrap();
        let id = s
            .insert_segment(sess, 0, 4_000_000_000, "segments/000001/seg-1.wav", 0)
            .unwrap();
        s.set_segment_analysis(
            id,
            &SegmentAnalysis {
                text: Some("yeah no it isn't".into()),
                lang: Some("en".into()),
                asr_model_id: Some("parakeet@1".into()),
                overlap_frac: Some(0.4),
                ..Default::default()
            },
        )
        .unwrap();
        s.set_segment_truth(id, Some("u1"), verdict, Some(0.5))
            .unwrap();
        id
    }

    #[test]
    fn only_the_mixed_verdicts_are_offered_for_a_resplit() {
        let s = store();
        let overlap = a_mixed_turn(&s, truth_verdict::OVERLAP);
        let partial = a_mixed_turn(&s, truth_verdict::PARTIAL);
        let single = a_mixed_turn(&s, truth_verdict::SINGLE);
        let unknown = a_mixed_turn(&s, truth_verdict::UNKNOWN);
        let ids: Vec<i64> = s
            .segments_for_resplit(usize::MAX)
            .unwrap()
            .iter()
            .map(|c| c.segment_id)
            .collect();
        assert!(ids.contains(&overlap) && ids.contains(&partial));
        assert!(
            !ids.contains(&single) && !ids.contains(&unknown),
            "a turn Discord has settled is not this pass's to re-decide"
        );
    }

    #[test]
    fn a_resplit_keeps_the_original_row_and_mints_the_rest() {
        let s = store();
        let id = a_mixed_turn(&s, truth_verdict::OVERLAP);
        let minted = s
            .resplit_segment(
                id,
                &[
                    (0, 2_000_000_000, "segments/000001/seg-1-p0.wav".into()),
                    (
                        2_000_000_000,
                        4_000_000_000,
                        "segments/000001/seg-1-p1.wav".into(),
                    ),
                ],
                99,
            )
            .unwrap();
        assert_eq!(minted.len(), 1);
        assert_ne!(minted[0], id, "the original row survives the split");

        // The original now covers only its first piece, and everything the
        // analysis leg owns about audio it no longer holds is gone.
        let row: (i64, i64, String, Option<String>, Option<String>) = s
            .conn
            .query_row(
                "SELECT t_start_ns, t_end_ns, audio_path, text, truth_verdict
                   FROM segments WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(row.0, 0);
        assert_eq!(row.1, 2_000_000_000);
        assert_eq!(row.2, "segments/000001/seg-1-p0.wav");
        assert_eq!(row.3, None, "words about audio this row no longer covers");
        assert_eq!(row.4, None, "and the verdict that described the whole turn");

        // The new row is in the same session and covers the second piece.
        let new: (i64, i64, i64, String) = s
            .conn
            .query_row(
                "SELECT session_id, t_start_ns, t_end_ns, audio_path
                   FROM segments WHERE id = ?1",
                params![minted[0]],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        let sess: i64 = s
            .conn
            .query_row(
                "SELECT session_id FROM segments WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(new.0, sess);
        assert_eq!((new.1, new.2), (2_000_000_000, 4_000_000_000));
        assert_eq!(new.3, "segments/000001/seg-1-p1.wav");
    }

    #[test]
    fn one_piece_is_not_a_split_and_writes_nothing() {
        let s = store();
        let id = a_mixed_turn(&s, truth_verdict::OVERLAP);
        assert!(
            s.resplit_segment(id, &[(0, 4_000_000_000, "x.wav".into())], 99)
                .unwrap()
                .is_empty()
        );
        let text: Option<String> = s
            .conn
            .query_row(
                "SELECT text FROM segments WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(text.as_deref(), Some("yeah no it isn't"));
        assert!(s.resplit_operations(10).unwrap().is_empty());
    }

    #[test]
    fn a_resplit_of_a_row_that_moved_under_it_does_nothing() {
        let s = store();
        let id = a_mixed_turn(&s, truth_verdict::OVERLAP);
        s.conn
            .execute(
                "UPDATE segments SET deleted_at = 1 WHERE id = ?1",
                params![id],
            )
            .unwrap();
        assert!(
            s.resplit_segment(id, &[(0, 1, "a.wav".into()), (1, 2, "b.wav".into())], 99)
                .unwrap()
                .is_empty(),
            "the gather is unlocked, so the write has to re-check"
        );
    }

    #[test]
    fn a_split_turn_can_be_put_back() {
        let s = store();
        let id = a_mixed_turn(&s, truth_verdict::OVERLAP);
        let minted = s
            .resplit_segment(
                id,
                &[
                    (0, 2_000_000_000, "p0.wav".into()),
                    (2_000_000_000, 4_000_000_000, "p1.wav".into()),
                ],
                99,
            )
            .unwrap();
        let ops = s.resplit_operations(10).unwrap();
        assert_eq!(ops.len(), 1);
        let prior: serde_json::Value = serde_json::from_str(&ops[0].prior_state).unwrap();
        assert!(s.unsplit_segment(&prior, 100).unwrap());

        let row: (i64, i64, String) = s
            .conn
            .query_row(
                "SELECT t_start_ns, t_end_ns, audio_path FROM segments WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (0, 4_000_000_000, "segments/000001/seg-1.wav".into()),
            "the whole span, and the clip the split never deleted"
        );
        let gone: Option<i64> = s
            .conn
            .query_row(
                "SELECT deleted_at FROM segments WHERE id = ?1",
                params![minted[0]],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(gone, Some(100), "the minted piece is deleted, not orphaned");
        // Twice is not twice as much: the second undo has nothing to do.
        assert!(!s.unsplit_segment(&prior, 101).unwrap());
    }

    #[test]
    fn an_undone_split_leaves_the_row_waiting_for_the_analysis_leg() {
        // The span is right again and nothing has re-read the audio, so
        // restoring the transcript would be asserting a reading of audio this
        // call never looked at.
        let s = store();
        let id = a_mixed_turn(&s, truth_verdict::OVERLAP);
        s.resplit_segment(
            id,
            &[
                (0, 2_000_000_000, "p0.wav".into()),
                (2_000_000_000, 4_000_000_000, "p1.wav".into()),
            ],
            99,
        )
        .unwrap();
        let ops = s.resplit_operations(10).unwrap();
        let prior: serde_json::Value = serde_json::from_str(&ops[0].prior_state).unwrap();
        assert_eq!(prior["text"], "yeah no it isn't", "recorded, for the audit");
        s.unsplit_segment(&prior, 100).unwrap();
        let text: Option<String> = s
            .conn
            .query_row(
                "SELECT text FROM segments WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(text, None);
    }
}
