//! World memory (0.10.0): where a conversation happened.
//!
//! VRChat's log already tells the daemon which world it is in — `roster.rs` has
//! parsed `Joining wrld_…:12345~region(eu)` and `Entering Room: <name>` since
//! Step 4, but only ever used the pair to stamp a roster row and to teach the
//! ASR a proper noun (`vocab::remember_world`). Nothing remembered that an
//! *evening* happened somewhere. This module is that memory:
//!
//! * `visits` — one row per world entry, closed when the next one arrives.
//! * `threads.world_id` — the visit that was open when the conversation began.
//!
//! Both are **observations, never inferences**. A visit is a line in a log
//! file; a thread's world is whichever visit covered its first turn, or NULL.
//! NULL is the ordinary case for anything that is not VRChat: a Discord call
//! and a microphone-only session happen nowhere, and saying so is more useful
//! than guessing at the last world the user happened to be in.
//!
//! ### Instances are not worlds
//!
//! `instance_id` is recorded and never grouped on. "The Great Pug" is a place a
//! person remembers; `12345~region(eu)` is a lobby number that changes every
//! time the door is opened. Everything a client is shown is grouped by
//! `world_id` alone.
//!
//! ### What the name is worth
//!
//! `world_name` arrives on a *separate log line* from the id, a moment later,
//! and only when VRChat felt like writing it. So the name is nullable and the
//! id is not, and a world nobody has a name for renders as its id rather than
//! as nothing.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use crate::store::Store;

/// How many worlds a person page shows.
pub const PERSON_WORLDS: usize = 8;
/// Default page size for `worlds.list`.
pub const WORLDS_LIMIT: usize = 20;
/// How many voices a `worlds.list` row names.
pub const WORLD_PEOPLE: usize = 6;
/// How many topics a `worlds.list` row names.
pub const WORLD_TOPICS: usize = 5;

/// One visit: the daemon was in this world, from then until then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Visit {
    pub id: i64,
    pub session_id: Option<i64>,
    pub world_id: String,
    pub world_name: Option<String>,
    pub instance_id: Option<String>,
    pub t_start_ns: i64,
    /// NULL while it is the current world, and until the next entry or the
    /// session's end closes it.
    pub t_end_ns: Option<i64>,
}

/// One world on a person's page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersonWorld {
    pub world_id: String,
    pub name: Option<String>,
    pub visits: i64,
    pub last_ns: i64,
    /// Wall-clock nanoseconds of the conversations this person took part in
    /// there. Not "time in the world" — the daemon knows when it was in a
    /// world, not when a *voice* was.
    pub together_ns: i64,
}

/// One world in the Memory view's list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldRow {
    pub world_id: String,
    pub name: Option<String>,
    pub visits: i64,
    pub last_ns: i64,
    pub people: Vec<(i64, String)>,
    pub topics: Vec<String>,
}

// ---------------------------------------------------------------------------
// schema v12
// ---------------------------------------------------------------------------

/// Schema v12, additive and idempotent like every migration before it: one new
/// table and one new column, referenced by nothing that already exists.
///
/// There is deliberately **no backfill against `segments`**. A visit is a line
/// in a log file; inventing one for a thread that predates the table would be
/// a claim about where somebody was, which is exactly the kind of guess the
/// tier boundary exists to prevent. What *is* recovered — and only because the
/// evidence is still on disk — is every world entry in whatever
/// `output_log_*.txt` files VRChat has not yet rotated away; see
/// [`backfill_from_logs`].
pub fn migrate_v12(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS visits (
             id          INTEGER PRIMARY KEY,
             -- The capture session that was open when the world was entered,
             -- if any. Nullable on purpose: the roster comes from a log file
             -- that knows nothing about capture, and a world entered while
             -- nothing was being recorded is still a visit.
             session_id  INTEGER REFERENCES sessions(id),
             world_id    TEXT    NOT NULL,
             world_name  TEXT,
             instance_id TEXT,
             t_start_ns  INTEGER NOT NULL,
             t_end_ns    INTEGER
         );

         -- The tailer replays a log from the top on every start, so the same
         -- entry is offered again and again. This is what makes writing it a
         -- no-op rather than a duplicate.
         CREATE UNIQUE INDEX IF NOT EXISTS idx_visits_entry
             ON visits(world_id, t_start_ns);
         CREATE INDEX IF NOT EXISTS idx_visits_start ON visits(t_start_ns);
         CREATE INDEX IF NOT EXISTS idx_visits_world ON visits(world_id, t_start_ns);
         CREATE INDEX IF NOT EXISTS idx_visits_open ON visits(t_end_ns, t_start_ns);",
    )?;
    if !column_exists(conn, "threads", "world_id")? {
        conn.execute("ALTER TABLE threads ADD COLUMN world_id TEXT", [])?;
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_threads_world ON threads(world_id, started_ns);",
    )?;
    Ok(())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// writing
// ---------------------------------------------------------------------------

/// Record a world entry, closing whatever was open before it.
///
/// Idempotent on `(world_id, t_start_ns)`: the same entry offered twice — which
/// is what a daemon restart does, every time — writes one row. Returns the
/// visit's id whether it was inserted now or already there.
pub fn open_visit(
    store: &Store,
    world_id: &str,
    instance_id: Option<&str>,
    t_start_ns: i64,
) -> Result<i64> {
    let conn = store.conn();
    let tx = conn.unchecked_transaction()?;
    // Anything still open that began before this entry ends here. `<` and not
    // `<=`: an entry cannot close itself on a re-read.
    tx.execute(
        "UPDATE visits SET t_end_ns = ?1
         WHERE t_end_ns IS NULL AND t_start_ns < ?1",
        params![t_start_ns],
    )?;
    let session_id: Option<i64> = tx
        .query_row(
            "SELECT id FROM sessions
             WHERE started_at_utc_ns <= ?1
               AND (ended_at_utc_ns IS NULL OR ended_at_utc_ns >= ?1)
             ORDER BY id DESC LIMIT 1",
            params![t_start_ns],
            |r| r.get(0),
        )
        .optional()?;
    tx.execute(
        "INSERT OR IGNORE INTO visits
             (session_id, world_id, world_name, instance_id, t_start_ns, t_end_ns)
         VALUES (?1, ?2, NULL, ?3, ?4, NULL)",
        params![session_id, world_id, instance_id, t_start_ns],
    )?;
    let id: i64 = tx.query_row(
        "SELECT id FROM visits WHERE world_id = ?1 AND t_start_ns = ?2",
        params![world_id, t_start_ns],
        |r| r.get(0),
    )?;
    tx.commit()?;
    Ok(id)
}

/// Attach the human-readable name to the visit that is open at `t_ns`.
///
/// The name is written to **every** visit of that world, not only this one: the
/// id is the identity and the name is a label for it, and a world whose name
/// was missed on one evening is the same world.
pub fn name_visit(store: &Store, name: &str, t_ns: i64) -> Result<bool> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(false);
    }
    let conn = store.conn();
    let world: Option<String> = conn
        .query_row(
            "SELECT world_id FROM visits
             WHERE t_start_ns <= ?1 AND (t_end_ns IS NULL OR t_end_ns > ?1)
             ORDER BY t_start_ns DESC LIMIT 1",
            params![t_ns],
            |r| r.get(0),
        )
        .optional()?;
    let Some(world) = world else {
        return Ok(false);
    };
    let n = conn.execute(
        "UPDATE visits SET world_name = ?2 WHERE world_id = ?1",
        params![world, name],
    )?;
    Ok(n > 0)
}

/// Close the current visit — a session ending, or the daemon shutting down.
pub fn close_open(store: &Store, t_end_ns: i64) -> Result<usize> {
    Ok(store.conn().execute(
        "UPDATE visits SET t_end_ns = ?1 WHERE t_end_ns IS NULL AND t_start_ns < ?1",
        params![t_end_ns],
    )?)
}

/// The visit covering an instant, if any.
pub fn visit_at(conn: &Connection, t_ns: i64) -> Result<Option<Visit>> {
    Ok(conn
        .query_row(
            "SELECT id, session_id, world_id, world_name, instance_id, t_start_ns, t_end_ns
             FROM visits
             WHERE t_start_ns <= ?1 AND (t_end_ns IS NULL OR t_end_ns > ?1)
             ORDER BY t_start_ns DESC LIMIT 1",
            params![t_ns],
            visit_from,
        )
        .optional()?)
}

fn visit_from(r: &rusqlite::Row<'_>) -> rusqlite::Result<Visit> {
    Ok(Visit {
        id: r.get(0)?,
        session_id: r.get(1)?,
        world_id: r.get(2)?,
        world_name: r.get(3)?,
        instance_id: r.get(4)?,
        t_start_ns: r.get(5)?,
        t_end_ns: r.get(6)?,
    })
}

/// Every visit, newest first.
pub fn visits(store: &Store, limit: usize) -> Result<Vec<Visit>> {
    let mut stmt = store.conn().prepare(
        "SELECT id, session_id, world_id, world_name, instance_id, t_start_ns, t_end_ns
         FROM visits ORDER BY t_start_ns DESC, id DESC LIMIT ?1",
    )?;
    Ok(stmt
        .query_map(params![limit as i64], visit_from)?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

// ---------------------------------------------------------------------------
// the backfill, such as it is
// ---------------------------------------------------------------------------

/// Recover every world entry from the VRChat logs still on disk.
///
/// This is the whole of the backfill, and it is honest about its reach: VRChat
/// rotates `output_log_*.txt` on a world change and prunes the old ones, so
/// what comes back is the last few days at most, and a database older than
/// those files has threads with no world for ever. Nothing is inferred to fill
/// that in.
///
/// Idempotent (the unique index does the work), so it can run on every start.
/// Returns how many entries were written.
pub fn backfill_from_logs(store: &Store, dirs: &[std::path::PathBuf]) -> Result<usize> {
    let mut logs: Vec<std::path::PathBuf> = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with("output_log_") && name.ends_with(".txt") {
                logs.push(path.clone());
            }
        }
    }
    logs.sort();
    let mut written = 0usize;
    for log in logs {
        let Ok(text) = std::fs::read(&log) else {
            continue;
        };
        for line in String::from_utf8_lossy(&text).lines() {
            let Some(parsed) = crate::roster::parse_line(line) else {
                continue;
            };
            match &parsed.event {
                crate::roster::Event::World { world_id, instance } => {
                    open_visit(store, world_id, Some(instance), parsed.t_utc_ns)?;
                    written += 1;
                }
                crate::roster::Event::Room { name } => {
                    name_visit(store, name, parsed.t_utc_ns)?;
                }
                _ => {}
            }
        }
    }
    Ok(written)
}

/// Give every conversation that has no world the one the visits say it was in.
///
/// Run after [`backfill_from_logs`], and only ever writes where the column is
/// NULL: a thread that already carries a world was stamped when it was created
/// and that stamp is the better evidence. Returns how many were filled in.
pub fn stamp_threads(store: &Store) -> Result<usize> {
    Ok(store.conn().execute(
        "UPDATE threads SET world_id = (
             SELECT v.world_id FROM visits v
              WHERE v.t_start_ns <= threads.started_ns
                AND (v.t_end_ns IS NULL OR v.t_end_ns > threads.started_ns)
              ORDER BY v.t_start_ns DESC LIMIT 1)
         WHERE world_id IS NULL
           AND EXISTS (SELECT 1 FROM visits v2
                        WHERE v2.t_start_ns <= threads.started_ns
                          AND (v2.t_end_ns IS NULL OR v2.t_end_ns > threads.started_ns))",
        [],
    )?)
}

// ---------------------------------------------------------------------------
// reading
// ---------------------------------------------------------------------------

/// Where this voice's conversations happened, most time first.
///
/// `together_ns` is the wall-clock length of the conversations they took part
/// in there — NOT how long they were in the world, which the daemon does not
/// know. It knows when *it* was in a world and when a voice was talking, and
/// the honest intersection of those two is the conversation.
pub fn person_worlds(store: &Store, speaker_id: i64, limit: usize) -> Result<Vec<PersonWorld>> {
    let mut stmt = store.conn().prepare(
        "SELECT t.world_id,
                (SELECT v.world_name FROM visits v
                  WHERE v.world_id = t.world_id AND v.world_name IS NOT NULL
                  ORDER BY v.t_start_ns DESC LIMIT 1),
                COUNT(DISTINCT (SELECT v2.id FROM visits v2
                                 WHERE v2.world_id = t.world_id
                                   AND v2.t_start_ns <= t.started_ns
                                 ORDER BY v2.t_start_ns DESC LIMIT 1)),
                MAX(t.ended_ns),
                SUM(MAX(t.ended_ns - t.started_ns, 0))
         FROM threads t
         WHERE t.world_id IS NOT NULL
           AND EXISTS (SELECT 1 FROM segments g
                        JOIN speaker_resolved sp ON sp.id = g.speaker_id
                        WHERE g.thread_id = t.id AND g.deleted_at IS NULL
                          AND sp.canonical_id = ?1)
         GROUP BY t.world_id
         ORDER BY SUM(MAX(t.ended_ns - t.started_ns, 0)) DESC, MAX(t.ended_ns) DESC
         LIMIT ?2",
    )?;
    Ok(stmt
        .query_map(params![speaker_id, limit as i64], |r| {
            Ok(PersonWorld {
                world_id: r.get(0)?,
                name: r.get(1)?,
                visits: r.get(2)?,
                last_ns: r.get(3)?,
                together_ns: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Every world with a visit, most recent first.
pub fn list(store: &Store, limit: usize) -> Result<Vec<WorldRow>> {
    let conn = store.conn();
    let mut stmt = conn.prepare(
        "SELECT world_id,
                (SELECT v2.world_name FROM visits v2
                  WHERE v2.world_id = v.world_id AND v2.world_name IS NOT NULL
                  ORDER BY v2.t_start_ns DESC LIMIT 1),
                COUNT(*),
                MAX(t_start_ns)
         FROM visits v
         GROUP BY world_id
         ORDER BY MAX(t_start_ns) DESC
         LIMIT ?1",
    )?;
    let base = stmt
        .query_map(params![limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut out = Vec::with_capacity(base.len());
    for (world_id, name, visits, last_ns) in base {
        out.push(WorldRow {
            people: people_in(conn, &world_id, WORLD_PEOPLE)?,
            topics: topics_in(conn, &world_id, WORLD_TOPICS)?,
            world_id,
            name,
            visits,
            last_ns,
        });
    }
    Ok(out)
}

/// Who has been heard in a world, most talkative first.
fn people_in(conn: &Connection, world_id: &str, limit: usize) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT sp.canonical_id,
                COALESCE(s.display_name, ''),
                COALESCE(s.auto_label, ''),
                SUM(g.t_end_ns - g.t_start_ns) AS spoke
         FROM segments g
         JOIN threads t ON t.id = g.thread_id
         JOIN speaker_resolved sp ON sp.id = g.speaker_id
         LEFT JOIN speakers s ON s.id = sp.canonical_id
         WHERE t.world_id = ?1 AND g.deleted_at IS NULL
         GROUP BY sp.canonical_id
         ORDER BY spoke DESC, sp.canonical_id ASC
         LIMIT ?2",
    )?;
    Ok(stmt
        .query_map(params![world_id, limit as i64], |r| {
            let id: i64 = r.get(0)?;
            let name: String = r.get(1)?;
            let auto: String = r.get(2)?;
            let label = if name.is_empty() { auto } else { name };
            Ok((id, label))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// What gets talked about there. Tier 2 output (`threads.topic`), so it is
/// absent on a machine that has never run enrichment — which is most of them.
fn topics_in(conn: &Connection, world_id: &str, limit: usize) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT topic, COUNT(*) AS n FROM threads
         WHERE world_id = ?1 AND topic IS NOT NULL AND TRIM(topic) <> ''
         GROUP BY topic ORDER BY n DESC, topic ASC LIMIT ?2",
    )?;
    Ok(stmt
        .query_map(params![world_id, limit as i64], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Resolve a `world` facet to the world ids it selects.
///
/// A facet is either an id (`wrld_…`, matched exactly) or a case-insensitive
/// substring of a name ("pug"). An id that is not in the table still resolves
/// to itself — a search for a world nothing was recorded in must come back
/// empty, not come back as everything.
pub fn resolve_facet(store: &Store, facet: &str) -> Result<Vec<String>> {
    let facet = facet.trim();
    if facet.is_empty() {
        return Ok(Vec::new());
    }
    if facet.starts_with("wrld_") {
        return Ok(vec![facet.to_string()]);
    }
    let mut stmt = store.conn().prepare(
        "SELECT DISTINCT world_id FROM visits
         WHERE world_name IS NOT NULL
           AND LOWER(world_name) LIKE '%' || LOWER(?1) || '%'
         ORDER BY world_id",
    )?;
    let mut ids: Vec<String> = stmt
        .query_map(params![facet], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if ids.is_empty() {
        // Nothing matched. Selecting an id that cannot exist is how an empty
        // answer stays an empty answer rather than becoming an unfiltered one.
        ids.push(format!("\u{0}no-such-world:{facet}"));
    }
    Ok(ids)
}

/// The name to show a world by, or `None` when only its id is known.
pub fn name_of(store: &Store, world_id: &str) -> Result<Option<String>> {
    Ok(store
        .conn()
        .query_row(
            "SELECT world_name FROM visits
             WHERE world_id = ?1 AND world_name IS NOT NULL
             ORDER BY t_start_ns DESC LIMIT 1",
            params![world_id],
            |r| r.get(0),
        )
        .optional()?)
}

/// Every world name the daemon knows, for the question parser. Longest first,
/// so "The Great Pug" beats "Pug" when both exist.
pub fn known_names(store: &Store) -> Result<Vec<(String, String)>> {
    let mut stmt = store.conn().prepare(
        "SELECT world_id, world_name FROM visits
         WHERE world_name IS NOT NULL
         GROUP BY world_id
         ORDER BY MAX(t_start_ns) DESC",
    )?;
    let mut rows: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.sort_by_key(|(_, name)| std::cmp::Reverse(name.split_whitespace().count()));
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: i64 = 1_000_000_000;
    const PUG: &str = "wrld_aaaaaaaa-0000-0000-0000-000000000000";
    const CLUB: &str = "wrld_bbbbbbbb-0000-0000-0000-000000000000";

    struct Rig {
        dir: std::path::PathBuf,
        store: Store,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn rig(name: &str) -> Rig {
        let dir =
            std::env::temp_dir().join(format!("nx-recall-worlds-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir).unwrap();
        Rig { dir, store }
    }

    #[test]
    fn a_world_entry_closes_the_one_before_it() {
        let r = rig("close");
        open_visit(&r.store, PUG, Some("11"), 100 * SEC).unwrap();
        name_visit(&r.store, "The Great Pug", 150 * SEC).unwrap();
        open_visit(&r.store, CLUB, Some("22"), 200 * SEC).unwrap();

        let all = visits(&r.store, 10).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].world_id, CLUB);
        assert_eq!(all[0].t_end_ns, None, "the current world is still open");
        assert_eq!(all[1].world_id, PUG);
        assert_eq!(all[1].t_end_ns, Some(200 * SEC));
        assert_eq!(all[1].world_name.as_deref(), Some("The Great Pug"));
        assert_eq!(all[1].instance_id.as_deref(), Some("11"));
    }

    #[test]
    fn the_same_entry_offered_twice_is_one_visit() {
        // A daemon restart re-reads the log from the top. Every entry in it is
        // offered again, and every one of them must be a no-op.
        let r = rig("idempotent");
        for _ in 0..3 {
            open_visit(&r.store, PUG, Some("11"), 100 * SEC).unwrap();
            open_visit(&r.store, CLUB, Some("22"), 200 * SEC).unwrap();
        }
        let all = visits(&r.store, 10).unwrap();
        assert_eq!(all.len(), 2, "got {all:#?}");
        // And the earlier one is still closed at the later one's start, not at
        // some instant a re-read invented.
        assert_eq!(all[1].t_end_ns, Some(200 * SEC));
    }

    #[test]
    fn the_visit_at_an_instant_is_the_one_that_covers_it() {
        let r = rig("at");
        open_visit(&r.store, PUG, Some("11"), 100 * SEC).unwrap();
        open_visit(&r.store, CLUB, Some("22"), 200 * SEC).unwrap();
        let conn = r.store.conn();
        assert_eq!(visit_at(conn, 50 * SEC).unwrap(), None);
        assert_eq!(visit_at(conn, 100 * SEC).unwrap().unwrap().world_id, PUG);
        assert_eq!(visit_at(conn, 199 * SEC).unwrap().unwrap().world_id, PUG);
        assert_eq!(visit_at(conn, 200 * SEC).unwrap().unwrap().world_id, CLUB);
        assert_eq!(
            visit_at(conn, 10_000 * SEC).unwrap().unwrap().world_id,
            CLUB
        );
    }

    #[test]
    fn a_name_belongs_to_the_world_and_not_to_one_evening() {
        let r = rig("names");
        open_visit(&r.store, PUG, Some("11"), 100 * SEC).unwrap();
        open_visit(&r.store, CLUB, Some("22"), 200 * SEC).unwrap();
        // Back to the Pug, and this time VRChat wrote the name.
        open_visit(&r.store, PUG, Some("33"), 300 * SEC).unwrap();
        name_visit(&r.store, "The Great Pug", 310 * SEC).unwrap();

        let all = visits(&r.store, 10).unwrap();
        let pugs: Vec<_> = all.iter().filter(|v| v.world_id == PUG).collect();
        assert_eq!(pugs.len(), 2);
        assert!(
            pugs.iter()
                .all(|v| v.world_name.as_deref() == Some("The Great Pug")),
            "a world whose name was missed on one evening is the same world"
        );
        assert_eq!(
            name_of(&r.store, PUG).unwrap().as_deref(),
            Some("The Great Pug")
        );
        assert_eq!(name_of(&r.store, CLUB).unwrap(), None);
    }

    #[test]
    fn a_facet_is_an_id_or_a_piece_of_a_name() {
        let r = rig("facet");
        open_visit(&r.store, PUG, Some("11"), 100 * SEC).unwrap();
        name_visit(&r.store, "The Great Pug", 110 * SEC).unwrap();
        open_visit(&r.store, CLUB, Some("22"), 200 * SEC).unwrap();
        name_visit(&r.store, "Ghost Club", 210 * SEC).unwrap();

        assert_eq!(resolve_facet(&r.store, PUG).unwrap(), vec![PUG.to_string()]);
        assert_eq!(
            resolve_facet(&r.store, "pug").unwrap(),
            vec![PUG.to_string()]
        );
        assert_eq!(
            resolve_facet(&r.store, "GHOST").unwrap(),
            vec![CLUB.to_string()]
        );
        // A facet nobody has a world for selects nothing — never everything.
        let none = resolve_facet(&r.store, "atlantis").unwrap();
        assert_eq!(none.len(), 1);
        assert!(none[0].starts_with('\u{0}'), "got {none:?}");
        assert!(resolve_facet(&r.store, "   ").unwrap().is_empty());
    }

    #[test]
    fn the_backfill_reads_whatever_logs_are_still_on_disk() {
        let r = rig("backfill");
        let logs = r.dir.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("output_log_2026-08-31_18-00-00.txt"),
            "2026.08.31 18:00:00 Debug      -  [Behaviour] Joining wrld_aaaaaaaa-0000-0000-0000-000000000000:11\n\
             2026.08.31 18:00:01 Debug      -  [Behaviour] Entering Room: The Great Pug\n\
             2026.08.31 20:00:00 Debug      -  [Behaviour] Joining wrld_bbbbbbbb-0000-0000-0000-000000000000:22\n",
        )
        .unwrap();
        std::fs::write(logs.join("not-a-log.txt"), "Joining wrld_cccc:9\n").unwrap();

        let n = backfill_from_logs(&r.store, std::slice::from_ref(&logs)).unwrap();
        assert_eq!(n, 2);
        let all = visits(&r.store, 10).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[1].world_name.as_deref(), Some("The Great Pug"));
        // Running it again writes nothing new: the whole point of the unique
        // index is that a start-up backfill can be unconditional.
        backfill_from_logs(&r.store, std::slice::from_ref(&logs)).unwrap();
        assert_eq!(visits(&r.store, 10).unwrap().len(), 2);
    }
}
