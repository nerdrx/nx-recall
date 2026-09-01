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
//! Existing databases are migrated in place.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::embed::Embedding;

pub const SCHEMA_VERSION: i64 = 5;

/// `sources.kind` for an application playback stream — the only kind before v4.
pub const KIND_APP: &str = "app";
/// `sources.kind` for the user's own microphone.
pub const KIND_MIC: &str = "mic";

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
}

/// `settings` key holding the id of the pinned "You" speaker.
pub const YOU_SPEAKER_KEY: &str = "you_speaker_id";
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
    pub text: Option<String>,
    pub overlap_frac: Option<f32>,
    pub match_score: Option<f32>,
    pub audio_path: String,
    /// The transcript's language, when one is known (v5).
    pub lang: Option<String>,
    /// How the speaker got here (`store::label_via`), so a client can distrust
    /// an inherited label without distrusting a matched one.
    pub label_via: Option<String>,
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

/// What sweeping one one-off voice removed. `goldens` are paths the caller
/// unlinks — the rows are already gone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub speaker_id: i64,
    /// Every segment that pointed at the voice, live or already soft-deleted.
    pub segments: Vec<i64>,
    /// How many of those this call was the one to soft-delete.
    pub soft_deleted: usize,
    pub prototypes: usize,
    pub goldens: Vec<String>,
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
}

impl SegmentFilter {
    pub fn is_everything(&self) -> bool {
        *self == Self::default()
    }
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
        self.apply_v3()?;
        self.apply_v4()?;
        self.apply_v5()?;
        if migrating {
            // Backfill the index for rows that predate it. New rows arrive
            // through the triggers.
            self.conn
                .execute(
                    "INSERT INTO segments_fts(segments_fts) VALUES('rebuild')",
                    [],
                )
                .context("rebuilding the transcript index")?;
        }

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

    pub fn list_sources(&self) -> Result<Vec<SourceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.match_key, s.display_name, COALESCE(s.kind, ?1), s.allowed,
                    s.first_seen, COALESCE(s.last_seen, s.first_seen),
                    (SELECT COUNT(*) FROM sessions ss
                      WHERE ss.source_id = s.id AND ss.ended_at_utc_ns IS NULL)
             FROM sources s ORDER BY s.allowed DESC, s.match_key ASC",
        )?;
        let rows = stmt
            .query_map(params![KIND_APP], |r| {
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
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn begin_session(&self, source_id: i64, started_at_utc_ns: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO sessions (source_id, started_at_utc_ns) VALUES (?1, ?2)",
            params![source_id, started_at_utc_ns],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

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

    pub fn segment_count(&self, session_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM segments WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?)
    }

    // ---- analysis --------------------------------------------------------

    pub fn set_segment_analysis(&self, segment_id: i64, a: &SegmentAnalysis) -> Result<()> {
        self.conn.execute(
            "UPDATE segments
             SET text = ?2, lang = ?3, lang_via = ?4, asr_model_id = ?5, overlap_frac = ?6
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
        self.conn.execute(
            "UPDATE segments SET text = ?2, lang = ?3, lang_via = ?4, asr_model_id = ?5
             WHERE id = ?1",
            params![segment_id, text, lang, lang_via::REDECODE, asr_model_id],
        )?;
        Ok(())
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
        self.conn.execute(
            "UPDATE segments SET speaker_id = ?2, match_score = ?3, label_via = ?4 WHERE id = ?1",
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

    fn speaker_prototypes(
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
    pub fn mint_speaker(&self, created_at: i64) -> Result<i64> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM speakers", [], |r| r.get(0))?;
        self.create_speaker(&format!("Speaker_{:02}", n + 1), created_at)
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
    ) -> Result<i64> {
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
                None => return Ok(0),
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
        Ok(self.conn.last_insert_rowid())
    }

    pub fn prototype_count(&self, speaker_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM speaker_prototypes WHERE speaker_id = ?1",
            params![speaker_id],
            |r| r.get(0),
        )?)
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
        let segments = tx.execute(
            "UPDATE segments SET speaker_id = ?2 WHERE speaker_id = ?1",
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

        let mut stmt = self.conn.prepare(
            "SELECT id, vector, is_golden, source_segment_id
             FROM speaker_prototypes
             WHERE speaker_id = ?1 AND embed_model_id = ?2
             ORDER BY id",
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
            // A moved row carries a score again, so its provenance is a match
            // whatever it was before — including an inherited label, which the
            // re-cluster has just replaced with a measured one.
            let mut move_segment = tx.prepare(
                "UPDATE segments SET speaker_id = ?2, match_score = ?3, label_via = 'match'
                 WHERE id = ?1 AND speaker_id = ?4",
            )?;
            for (id, score) in &write.segments {
                segments += move_segment.execute(params![id, minted, *score as f64, from])?;
            }
            // The undecidable ones keep their speaker and lose their
            // confidence: the correction UI reads `match_score` to know which
            // labels to distrust (DESIGN §6).
            let mut soften = tx.prepare(
                "UPDATE segments SET match_score = ?2 WHERE id = ?1 AND speaker_id = ?3",
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
                    s.languages
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
    const SEGMENT_COLUMNS: &'static str =
        "g.id, g.session_id, sc.match_key, g.t_start_ns, g.t_end_ns,
         sp.canonical_id, sp.display_name, g.text, g.overlap_frac, g.match_score, g.audio_path,
         g.lang, g.label_via";

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
        })
    }

    /// One segment, in the shape clients read.
    pub fn segment_row(&self, segment_id: i64) -> Result<Option<SegmentRow>> {
        let sql = format!(
            "SELECT {}
             FROM segments g
             JOIN sessions ss ON ss.id = g.session_id
             JOIN sources sc ON sc.id = ss.source_id
             LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
             WHERE g.id = ?1",
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
             ORDER BY g.t_start_ns ASC
             LIMIT ?7",
            Self::SEGMENT_COLUMNS
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
                    limit as i64
                ],
                |r| {
                    Ok(SearchHit {
                        row: Self::segment_row_from(r)?,
                        snippet: r.get(13)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// A transcript page: chronological, filtered, capped.
    pub fn segment_rows(&self, filter: &SegmentFilter, limit: usize) -> Result<Vec<SegmentRow>> {
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
             ORDER BY g.t_start_ns ASC, g.id ASC
             LIMIT ?6",
            Self::SEGMENT_COLUMNS
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![
                    filter.speaker,
                    filter.session,
                    filter.source,
                    filter.from,
                    filter.to,
                    limit as i64
                ],
                Self::segment_row_from,
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
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
        let mut stmt = self.conn.prepare(
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
             ORDER BY g.t_start_ns ASC, g.id ASC",
        )?;
        let rows = stmt
            .query_map(
                params![
                    filter.speaker,
                    filter.session,
                    filter.source,
                    filter.from,
                    filter.to
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
            let mut orphan_prototypes = tx.prepare(
                "UPDATE speaker_prototypes SET source_segment_id = NULL WHERE source_segment_id = ?1",
            )?;
            let mut drop_segment = tx.prepare("DELETE FROM segments WHERE id = ?1")?;
            for id in ids {
                drop_embeddings.execute(params![id])?;
                orphan_prototypes.execute(params![id])?;
                n += drop_segment.execute(params![id])?;
            }
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

    pub fn vacuum(&self) -> Result<()> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    // ---- manual labelling ------------------------------------------------

    /// The fields an undo would have to put back.
    pub fn segment_state(&self, segment_id: i64) -> Result<(Option<i64>, Option<String>)> {
        let row = self
            .conn
            .query_row(
                "SELECT speaker_id, text FROM segments WHERE id = ?1",
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
             WHERE id = ?1",
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
            "UPDATE segments SET text = ?2 WHERE id = ?1",
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
                    s.languages
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
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Remove one voice and everything that rests on it, in one transaction.
    ///
    /// The cascade is the one `delete.run` performs, plus the identity itself:
    /// the voice's segments are soft-deleted (so the undo window still applies
    /// and the sweeper still finalises them), their labels are cleared so the
    /// foreign key can go, and the prototypes and goldens that made this a
    /// recognisable voice are removed for real. The golden files are returned
    /// rather than unlinked — the row goes first, because a file with no row is
    /// residue the reconciliation sweep understands and a row with no file is a
    /// lie.
    pub fn prune_speaker(&self, speaker_id: i64, at_utc_ns: i64) -> Result<PruneReport> {
        if self.resolve_speaker(speaker_id)? != speaker_id {
            bail!("speaker {speaker_id} is a tombstone, not a voice");
        }
        let segments: Vec<i64> = {
            let mut stmt = self
                .conn
                .prepare("SELECT id FROM segments WHERE speaker_id = ?1 ORDER BY id")?;
            stmt.query_map(params![speaker_id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let goldens: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT audio_path FROM golden_samples WHERE speaker_id = ?1")?;
            stmt.query_map(params![speaker_id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        let tx = self.conn.unchecked_transaction()?;
        let soft_deleted = tx.execute(
            "UPDATE segments SET deleted_at = ?2
             WHERE speaker_id = ?1 AND deleted_at IS NULL",
            params![speaker_id, at_utc_ns],
        )?;
        tx.execute(
            "UPDATE segments SET speaker_id = NULL, match_score = NULL, label_via = NULL
             WHERE speaker_id = ?1",
            params![speaker_id],
        )?;
        let prototypes = tx.execute(
            "DELETE FROM speaker_prototypes WHERE speaker_id = ?1",
            params![speaker_id],
        )?;
        tx.execute(
            "DELETE FROM golden_samples WHERE speaker_id = ?1",
            params![speaker_id],
        )?;
        tx.execute("DELETE FROM speakers WHERE id = ?1", params![speaker_id])?;
        tx.commit()?;

        Ok(PruneReport {
            speaker_id,
            segments,
            soft_deleted,
            prototypes,
            goldens,
        })
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
                    label_via
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
                Ok(())
            },
        )?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
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
        s.add_prototype(spk, &emb("m@1", &[1.0, 0.0]), None, true, 1, 0)
            .unwrap();
        s.add_prototype(spk, &emb("m@1", &[0.0, 1.0]), None, true, 1, 0)
            .unwrap();
        assert_eq!(s.prototype_count(spk).unwrap(), 2);

        // An auto-enrolled vector cannot displace them, so it is dropped.
        s.add_prototype(spk, &emb("m@1", &[1.0, 0.0]), None, false, 1, 0)
            .unwrap();
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
        assert_eq!(report.soft_deleted, 1);
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
}
