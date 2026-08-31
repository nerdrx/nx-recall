//! SQLite storage. Step 1 uses the `sources` / `sessions` / `segments` subset
//! of the design brief's schema; ASR, embeddings and overlap columns land later.

use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};

pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone)]
pub struct SourceRow {
    pub id: i64,
    pub match_key: String,
    pub display_name: String,
    pub allowed: bool,
    pub first_seen: i64,
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

    #[cfg(test)]
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
        match current {
            None => {
                self.conn.execute(
                    "INSERT INTO schema_version (version) VALUES (?1)",
                    params![SCHEMA_VERSION],
                )?;
            }
            Some(v) if v == SCHEMA_VERSION => {}
            Some(v) => bail!(
                "database schema version {v} was written by a different recalld \
                 (this build speaks version {SCHEMA_VERSION}); refusing to touch it"
            ),
        }
        Ok(())
    }

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

    #[cfg(test)]
    pub fn segment_count(&self, session_id: i64) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM segments WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
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
        // first_seen is the first sighting, not the latest.
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
}
