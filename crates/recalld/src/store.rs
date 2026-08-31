//! SQLite storage.
//!
//! Schema v1 (Step 1) was `sources` / `sessions` / `segments`. v2 adds the
//! analysis columns, the voicebank (`speakers`, `speaker_prototypes`,
//! `embeddings`, `golden_samples`) and a full-text index over transcripts.
//! Existing v1 databases are migrated in place.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

use crate::embed::Embedding;

pub const SCHEMA_VERSION: i64 = 2;

#[derive(Debug, Clone)]
pub struct SourceRow {
    pub id: i64,
    pub match_key: String,
    pub display_name: String,
    pub allowed: bool,
    pub first_seen: i64,
}

/// What the analysis leg learned about one turn.
#[derive(Debug, Clone, Default)]
pub struct SegmentAnalysis {
    pub text: Option<String>,
    pub lang: Option<String>,
    pub asr_model_id: Option<String>,
    pub overlap_frac: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct SpeakerSummary {
    pub id: i64,
    pub display_name: String,
    pub segments: i64,
    pub speech_ns: i64,
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

#[derive(Debug, Clone)]
pub struct SearchHit {
    pub segment_id: i64,
    pub t_start_ns: i64,
    pub speaker: Option<String>,
    pub snippet: String,
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

    fn add_column_if_missing(&self, table: &str, column: &str, decl: &str) -> Result<()> {
        let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let existing: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<rusqlite::Result<_>>()?;
        if existing.iter().any(|c| c == column) {
            return Ok(());
        }
        self.conn
            .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))
            .with_context(|| format!("adding {table}.{column}"))?;
        Ok(())
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
        self.conn.execute(
            "INSERT INTO sources (match_key, display_name, allowed, first_seen)
             VALUES (?1, ?2, 0, ?3)
             ON CONFLICT(match_key) DO UPDATE SET display_name = excluded.display_name",
            params![match_key, display_name, first_seen],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM sources WHERE match_key = ?1",
            params![match_key],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    /// Mirror a config rule into the DB so `sources` can show it.
    pub fn set_allowed(&self, match_key: &str, allowed: bool, first_seen: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sources (match_key, display_name, allowed, first_seen)
             VALUES (?1, ?1, ?2, ?3)
             ON CONFLICT(match_key) DO UPDATE SET allowed = excluded.allowed",
            params![match_key, allowed as i64, first_seen],
        )?;
        Ok(())
    }

    pub fn list_sources(&self) -> Result<Vec<SourceRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, match_key, display_name, allowed, first_seen
             FROM sources ORDER BY allowed DESC, match_key ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SourceRow {
                    id: r.get(0)?,
                    match_key: r.get(1)?,
                    display_name: r.get(2)?,
                    allowed: r.get::<_, i64>(3)? != 0,
                    first_seen: r.get(4)?,
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
             SET text = ?2, lang = ?3, asr_model_id = ?4, overlap_frac = ?5
             WHERE id = ?1",
            params![
                segment_id,
                a.text,
                a.lang,
                a.asr_model_id,
                a.overlap_frac.map(|v| v as f64)
            ],
        )?;
        Ok(())
    }

    pub fn set_segment_speaker(
        &self,
        segment_id: i64,
        speaker_id: Option<i64>,
        match_score: Option<f32>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE segments SET speaker_id = ?2, match_score = ?3 WHERE id = ?1",
            params![segment_id, speaker_id, match_score.map(|v| v as f64)],
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

    pub fn create_speaker(&self, display_name: &str, created_at: i64) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO speakers (display_name, created_at) VALUES (?1, ?2)",
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

    pub fn rename_speaker(&self, speaker_id: i64, display_name: &str) -> Result<()> {
        let n = self.conn.execute(
            "UPDATE speakers SET display_name = ?2 WHERE id = ?1",
            params![speaker_id, display_name],
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

    pub fn list_speakers(&self) -> Result<Vec<SpeakerSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.display_name,
                    COUNT(g.id),
                    COALESCE(SUM(g.t_end_ns - g.t_start_ns), 0)
             FROM speakers s
             LEFT JOIN segments g
                 ON g.speaker_id = s.id AND g.deleted_at IS NULL
             WHERE s.merged_into IS NULL
             GROUP BY s.id
             ORDER BY 4 DESC, s.id ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SpeakerSummary {
                    id: r.get(0)?,
                    display_name: r.get(1)?,
                    segments: r.get(2)?,
                    speech_ns: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
        let mut stmt = self.conn.prepare(
            "SELECT g.id, g.t_start_ns, sp.display_name,
                    snippet(segments_fts, 0, '[', ']', '…', 12)
             FROM segments_fts
             JOIN segments g ON g.id = segments_fts.rowid
             LEFT JOIN speaker_resolved sp ON sp.id = g.speaker_id
             WHERE segments_fts MATCH ?1 AND g.deleted_at IS NULL
             ORDER BY g.t_start_ns ASC
             LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![query, limit as i64], |r| {
                Ok(SearchHit {
                    segment_id: r.get(0)?,
                    t_start_ns: r.get(1)?,
                    speaker: r.get(2)?,
                    snippet: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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

    /// Raw column read, used by tests and by the CLI's single-row lookups.
    pub fn segment_fields(&self, segment_id: i64) -> Result<HashMap<String, Option<String>>> {
        let mut out = HashMap::new();
        self.conn.query_row(
            "SELECT text, asr_model_id, overlap_frac, speaker_id, match_score
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
        assert_eq!(s.list_sources().unwrap().len(), 1);

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
        assert_eq!(hits[0].segment_id, seg);
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
        assert_eq!(hits[0].speaker.as_deref(), Some("Mira"));

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

        s.rename_speaker(spk, "Kestrel").unwrap();
        assert_eq!(
            s.transcript(None, None).unwrap()[0].speaker.as_deref(),
            Some("Kestrel")
        );
        assert_eq!(
            s.search("anyone", 10).unwrap()[0].speaker.as_deref(),
            Some("Kestrel")
        );
    }

    #[test]
    fn renaming_a_speaker_that_does_not_exist_is_an_error() {
        assert!(store().rename_speaker(77, "Nobody").is_err());
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
}
