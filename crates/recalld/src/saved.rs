//! User-created saved searches and moments. Moments reference source rows;
//! no captured text or audio is copied into the saved-item tables.
use crate::{
    clock,
    proto::{Error, Request},
    service::segment_json,
    store::Store,
};
use anyhow::Result as AnyResult;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub fn migrate_v22(conn: &Connection) -> AnyResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS saved_searches (
        id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL,
        query TEXT NOT NULL, filters TEXT NOT NULL, created_ms INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS saved_moments (
        id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, note TEXT NOT NULL,
        original_count INTEGER NOT NULL, created_ms INTEGER NOT NULL);
      CREATE TABLE IF NOT EXISTS saved_moment_segments (
        moment_id INTEGER NOT NULL REFERENCES saved_moments(id) ON DELETE CASCADE,
        segment_id INTEGER NOT NULL REFERENCES segments(id) ON DELETE CASCADE,
        ordinal INTEGER NOT NULL, PRIMARY KEY(moment_id,segment_id));
      CREATE INDEX IF NOT EXISTS idx_saved_moment_segment ON saved_moment_segments(segment_id);
      CREATE TRIGGER IF NOT EXISTS saved_moment_last_source BEFORE DELETE ON segments BEGIN
        DELETE FROM saved_moments WHERE id IN (
          SELECT moment_id FROM saved_moment_segments WHERE segment_id=old.id
          AND NOT EXISTS (SELECT 1 FROM saved_moment_segments other
            WHERE other.moment_id=saved_moment_segments.moment_id AND other.segment_id<>old.id));
      END;",
    )?;
    Ok(())
}
/// Collections are organizational references, never independent copies of source content.
pub fn migrate_v23(conn: &Connection) -> AnyResult<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS saved_collections (
      id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL COLLATE NOCASE UNIQUE,
      created_ms INTEGER NOT NULL);",
    )?;
    let has_column = conn
        .prepare("PRAGMA table_info(saved_moments)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "collection_id");
    if !has_column {
        conn.execute_batch("ALTER TABLE saved_moments ADD COLUMN collection_id INTEGER REFERENCES saved_collections(id) ON DELETE SET NULL;")?;
    }
    conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_saved_moments_collection ON saved_moments(collection_id,created_ms,id);
      CREATE INDEX IF NOT EXISTS idx_history_visible ON segments(t_start_ns,id) WHERE deleted_at IS NULL;")?;
    Ok(())
}

/// A cursor carries nanoseconds as an opaque JSON string, never a JS number.
/// The snapshot ID bound prevents later inserts from shifting the walk.
pub fn history_page(store: &Store, req: &Request) -> Result<Value, Error> {
    let from = clock::parse_iso8601(req.str("from")?)
        .ok_or_else(|| Error::params("from must be an ISO timestamp"))?;
    let to = clock::parse_iso8601(req.str("to")?)
        .ok_or_else(|| Error::params("to must be an ISO timestamp"))?;
    if from >= to {
        return Err(Error::params("from must precede to"));
    }
    let limit = req.opt_i64("limit")?.unwrap_or(100);
    if !(1..=200).contains(&limit) {
        return Err(Error::params("limit must be 1–200"));
    }
    let conn = store.conn();
    let tx = db(conn.unchecked_transaction())?;
    let (last_ns, last_id, max_id) = if let Some(cursor) = req.opt_str("cursor")? {
        if cursor.len() > 512 {
            return Err(Error::params("invalid history cursor"));
        }
        let v: Value =
            serde_json::from_str(cursor).map_err(|_| Error::params("invalid history cursor"))?;
        let n = |key| {
            v.get(key)
                .and_then(Value::as_i64)
                .ok_or_else(|| Error::params("invalid history cursor"))
        };
        if n("from")? != from || n("to")? != to {
            return Err(Error::params("cursor belongs to another date range"));
        }
        let tuple = (n("time")?, n("id")?, n("max")?);
        if tuple.0 < from || tuple.0 >= to || tuple.1 <= 0 || tuple.2 < tuple.1 {
            return Err(Error::params("invalid history cursor"));
        }
        tuple
    } else {
        (
            from,
            -1,
            db(
                conn.query_row("SELECT COALESCE(MAX(id),0) FROM segments", [], |r| {
                    r.get::<_, i64>(0)
                }),
            )?,
        )
    };
    let mut stmt=db(conn.prepare("SELECT id,t_start_ns FROM segments WHERE deleted_at IS NULL AND t_start_ns>=?1 AND t_start_ns<?2 AND id<=?3 AND (t_start_ns,id)>(?4,?5) ORDER BY t_start_ns,id LIMIT ?6"))?;
    let ids = db(db(stmt.query_map(
        params![from, to, max_id, last_ns, last_id, limit + 1],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    ))?
    .collect::<rusqlite::Result<Vec<_>>>())?;
    let has_more = ids.len() > limit as usize;
    let page = &ids[..ids.len().min(limit as usize)];
    let mut segments = Vec::new();
    for (id, _) in page {
        if let Some(row) = store.segment_row(*id)? {
            segments.push(segment_json(&row));
        }
    }
    let cursor = if has_more {
        page.last().map(|(id, time)| {
            json!({"from":from,"to":to,"time":time,"id":id,"max":max_id}).to_string()
        })
    } else {
        None
    };
    drop(stmt);
    db(tx.commit())?;
    Ok(json!({"segments":segments,"next_cursor":cursor}))
}
fn collection(conn: &Connection, id: i64) -> Result<Value, Error> {
    let row = db(conn
        .query_row(
            "SELECT name,created_ms FROM saved_collections WHERE id=?1",
            [id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )
        .optional())?
    .ok_or_else(|| Error::not_found("collection not found"))?;
    let count:i64=db(conn.query_row("SELECT COUNT(*) FROM saved_moments m WHERE collection_id=?1 AND EXISTS(SELECT 1 FROM saved_moment_segments r JOIN segments s ON s.id=r.segment_id WHERE r.moment_id=m.id AND s.deleted_at IS NULL)",[id],|r|r.get(0)))?;
    Ok(json!({"id":id,"name":row.0,"created_ms":row.1,"count":count}))
}
fn collection_param(conn: &Connection, req: &Request) -> Result<Option<i64>, Error> {
    let id = if req.params.get("collection_id").is_some_and(Value::is_null) {
        None
    } else {
        req.opt_i64("collection_id")?
    };
    if let Some(id) = id {
        if id <= 0 {
            return Err(Error::params("collection_id must be positive or null"));
        }
        collection(conn, id)?;
    }
    Ok(id)
}

fn db<T>(r: rusqlite::Result<T>) -> Result<T, Error> {
    r.map_err(|e| Error::internal(e.to_string()))
}
fn text(req: &Request, key: &str, max: usize, required: bool) -> Result<String, Error> {
    let value = req.opt_str(key)?.unwrap_or("").trim();
    if value.chars().count() > max || (required && value.is_empty()) {
        return Err(Error::params(format!(
            "{key} must contain {} to {max} characters",
            usize::from(required)
        )));
    }
    Ok(value.to_string())
}
fn positive(req: &Request, key: &str) -> Result<i64, Error> {
    let id = req.i64(key)?;
    if id <= 0 {
        return Err(Error::params(format!("{key} must be positive")));
    }
    Ok(id)
}
fn page(req: &Request) -> Result<(i64, i64), Error> {
    let limit = req.opt_i64("limit")?.unwrap_or(100);
    let offset = req.opt_i64("offset")?.unwrap_or(0);
    if !(1..=200).contains(&limit) || offset < 0 {
        return Err(Error::params("limit must be 1–200 and offset nonnegative"));
    }
    Ok((limit, offset))
}
fn search(conn: &Connection, id: i64) -> Result<Value, Error> {
    let row = db(conn
        .query_row(
            "SELECT name,query,filters,created_ms FROM saved_searches WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            },
        )
        .optional())?
    .ok_or_else(|| Error::not_found("saved search not found"))?;
    let filters: Value =
        serde_json::from_str(&row.2).map_err(|e| Error::internal(e.to_string()))?;
    Ok(json!({"id":id,"name":row.0,"query":row.1,"filters":filters,"created_ms":row.3}))
}
pub(crate) fn moment(store: &Store, id: i64) -> Result<Option<Value>, Error> {
    let conn = store.conn();
    let Some((title, note, count, created, collection_id)) = db(conn
        .query_row(
            "SELECT title,note,original_count,created_ms,collection_id FROM saved_moments WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                ))
            },
        )
        .optional())?
    else {
        return Ok(None);
    };
    let mut stmt = db(conn.prepare(
        "SELECT segment_id FROM saved_moment_segments WHERE moment_id=?1 ORDER BY ordinal",
    ))?;
    let ids = db(
        db(stmt.query_map([id], |r| r.get::<_, i64>(0)))?.collect::<rusqlite::Result<Vec<_>>>()
    )?;
    let mut visible = Vec::new();
    let mut segments = Vec::new();
    for id in ids {
        if let Some(row) = store.segment_row(id)? {
            visible.push(id);
            segments.push(segment_json(&row));
        }
    }
    if segments.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        json!({"id":id,"title":title,"note":note,"segment_ids":visible,"segments":segments,
        "created_ms":created,"unavailable_count":count-segments.len() as i64,"collection_id":collection_id}),
    ))
}
/// Nearby *visible* turns of the same conversation (or source session).
pub fn context(store: &Store, req: &Request) -> Result<Value, Error> {
    let id = positive(req, "id")?;
    let anchor = store
        .segment_row(id)?
        .ok_or_else(|| Error::not_found("segment not found"))?;
    let before = req.opt_i64("before")?.unwrap_or(3);
    let after = req.opt_i64("after")?.unwrap_or(3);
    if !(0..=10).contains(&before) || !(0..=10).contains(&after) {
        return Err(Error::params("before and after must be 0–10"));
    }
    let group = if let Some(thread) = anchor.thread_id {
        ("thread_id", thread)
    } else {
        ("session_id", anchor.session_id)
    };
    let mut ids = Vec::new();
    for (op, order, limit) in [("<", "DESC", before), (">", "ASC", after)] {
        let sql = format!(
            "SELECT id FROM segments WHERE deleted_at IS NULL AND {}=?1 AND (t_start_ns,id) {op} (?2,?3) ORDER BY t_start_ns {order},id {order} LIMIT ?4",
            group.0
        );
        let mut stmt = db(store.conn().prepare(&sql))?;
        let mut found = db(db(stmt
            .query_map(params![group.1, anchor.t_start_ns, id, limit], |r| {
                r.get::<_, i64>(0)
            }))?
        .collect::<rusqlite::Result<Vec<_>>>())?;
        if op == "<" {
            found.reverse();
            ids.extend(found);
            ids.push(id)
        } else {
            ids.extend(found)
        }
    }
    let mut rows = Vec::new();
    for id in ids {
        if let Some(row) = store.segment_row(id)? {
            rows.push(segment_json(&row));
        }
    }
    Ok(json!({"anchor":segment_json(&anchor),"segments":rows}))
}

pub fn handle(store: &Store, req: &Request) -> Result<Value, Error> {
    let conn = store.conn();
    match req.method.as_str() {
        "segments.context" => context(store, req),
        "history.page" => history_page(store, req),
        "saved.collections.list" => {
            let mut stmt =
                db(conn
                    .prepare("SELECT id FROM saved_collections ORDER BY name COLLATE NOCASE,id"))?;
            let ids = db(db(stmt.query_map([], |r| r.get::<_, i64>(0)))?
                .collect::<rusqlite::Result<Vec<_>>>())?;
            let rows = ids
                .into_iter()
                .map(|id| collection(conn, id))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({"collections":rows}))
        }
        "saved.collections.save" => {
            let name = text(req, "name", 120, true)?;
            let existing = req.opt_i64("id")?;
            let duplicate:bool=db(conn.query_row("SELECT EXISTS(SELECT 1 FROM saved_collections WHERE name=?1 COLLATE NOCASE AND id<>?2)",params![name,existing.unwrap_or(-1)],|r|r.get(0)))?;
            if duplicate {
                return Err(Error::params("a collection with this name already exists"));
            }
            let id = if let Some(id) = existing {
                if db(conn.execute(
                    "UPDATE saved_collections SET name=?1 WHERE id=?2",
                    params![name, id],
                ))? == 0
                {
                    return Err(Error::not_found("collection not found"));
                }
                id
            } else {
                db(conn.execute(
                    "INSERT INTO saved_collections(name,created_ms) VALUES(?1,?2)",
                    params![name, clock::utc_now_ns() / 1_000_000],
                ))?;
                conn.last_insert_rowid()
            };
            Ok(json!({"collection":collection(conn,id)?}))
        }
        "saved.collections.delete" => {
            let removed = db(conn.execute(
                "DELETE FROM saved_collections WHERE id=?1",
                [positive(req, "id")?],
            ))? > 0;
            Ok(json!({"removed":removed}))
        }
        "saved.moments.get" => Ok(
            json!({"moment":moment(store,positive(req,"id")?)?.ok_or_else(||Error::not_found("saved moment has no retained source turns"))?}),
        ),
        "saved.moments.move" => {
            let id = positive(req, "id")?;
            let target = collection_param(conn, req)?;
            if db(conn.execute(
                "UPDATE saved_moments SET collection_id=?1 WHERE id=?2",
                params![target, id],
            ))? == 0
            {
                return Err(Error::not_found("saved moment not found"));
            }
            Ok(json!({"moment":moment(store,id)?}))
        }
        "saved.searches.list" => {
            let (limit, offset) = page(req)?;
            let mut stmt = db(conn.prepare(
                "SELECT id FROM saved_searches ORDER BY created_ms DESC,id DESC LIMIT ?1 OFFSET ?2",
            ))?;
            let ids = db(
                db(stmt.query_map(params![limit, offset], |r| r.get::<_, i64>(0)))?
                    .collect::<rusqlite::Result<Vec<_>>>(),
            )?;
            let rows = ids
                .into_iter()
                .map(|id| search(conn, id))
                .collect::<Result<Vec<_>, _>>()?;
            let total: i64 =
                db(conn.query_row("SELECT COUNT(*) FROM saved_searches", [], |r| r.get(0)))?;
            Ok(json!({"searches":rows,"total":total}))
        }
        "saved.searches.save" => {
            let name = text(req, "name", 120, true)?;
            let query = text(req, "query", 4096, false)?;
            let filters = req.param("filters").cloned().unwrap_or(json!({}));
            if !filters.is_object() || filters.to_string().len() > 16384 {
                return Err(Error::params("filters must be an object of at most 16 KiB"));
            }
            let id = if let Some(id) = req.opt_i64("id")? {
                if db(conn.execute(
                    "UPDATE saved_searches SET name=?1,query=?2,filters=?3 WHERE id=?4",
                    params![name, query, filters.to_string(), id],
                ))? == 0
                {
                    return Err(Error::not_found("saved search not found"));
                };
                id
            } else {
                db(conn.execute(
                    "INSERT INTO saved_searches(name,query,filters,created_ms) VALUES(?1,?2,?3,?4)",
                    params![
                        name,
                        query,
                        filters.to_string(),
                        clock::utc_now_ns() / 1_000_000
                    ],
                ))?;
                conn.last_insert_rowid()
            };
            Ok(json!({"search":search(conn,id)?}))
        }
        "saved.moments.list" => {
            let (limit, offset) = page(req)?;
            let scoped = req.params.get("collection_id").is_some();
            let target = collection_param(conn, req)?;
            let mut stmt=db(conn.prepare("SELECT m.id FROM saved_moments m WHERE (?3=0 OR m.collection_id IS ?4) AND EXISTS(SELECT 1 FROM saved_moment_segments r JOIN segments g ON g.id=r.segment_id WHERE r.moment_id=m.id AND g.deleted_at IS NULL) ORDER BY created_ms DESC,id DESC LIMIT ?1 OFFSET ?2"))?;
            let ids = db(
                db(stmt.query_map(params![limit, offset, scoped, target], |r| {
                    r.get::<_, i64>(0)
                }))?
                .collect::<rusqlite::Result<Vec<_>>>(),
            )?;
            let mut rows = Vec::new();
            for id in ids {
                if let Some(row) = moment(store, id)? {
                    rows.push(row)
                }
            }
            let total:i64=db(conn.query_row("SELECT COUNT(*) FROM saved_moments m WHERE (?1=0 OR m.collection_id IS ?2) AND EXISTS(SELECT 1 FROM saved_moment_segments r JOIN segments g ON g.id=r.segment_id WHERE r.moment_id=m.id AND g.deleted_at IS NULL)",params![scoped,target],|r|r.get(0)))?;
            Ok(json!({"moments":rows,"total":total}))
        }
        "saved.moments.save" => {
            let title = text(req, "title", 180, false)?;
            let note = text(req, "note", 4096, false)?;
            let raw = req
                .param("segment_ids")
                .and_then(Value::as_array)
                .ok_or_else(|| Error::params("segment_ids is required"))?;
            if raw.is_empty() || raw.len() > 20 {
                return Err(Error::params("save between 1 and 20 consecutive turns"));
            }
            // Validate and insert in one snapshot. A conflicting external writer
            // aborts the transaction rather than saving stale source references.
            let tx = db(conn.unchecked_transaction())?;
            let mut rows = Vec::new();
            for value in raw {
                let id = value
                    .as_i64()
                    .filter(|id| *id > 0)
                    .ok_or_else(|| Error::params("segment_ids must contain positive integers"))?;
                rows.push(
                    store
                        .segment_row(id)?
                        .ok_or_else(|| Error::not_found("a source segment is unavailable"))?,
                );
            }
            rows.sort_by_key(|r| (r.t_start_ns, r.id));
            if rows.windows(2).any(|w| w[0].id == w[1].id) {
                return Err(Error::params("segment_ids must be unique"));
            }
            let first = &rows[0];
            let last = rows.last().unwrap();
            let same_thread =
                first.thread_id.is_some() && rows.iter().all(|r| r.thread_id == first.thread_id);
            if !same_thread && rows.iter().any(|r| r.session_id != first.session_id) {
                return Err(Error::params(
                    "a moment must belong to one conversation or session",
                ));
            }
            if rows.iter().map(|r| r.t_end_ns).max().unwrap() - first.t_start_ns > 600_000_000_000 {
                return Err(Error::params("a moment must not exceed ten minutes"));
            }
            let (field, group) = if same_thread {
                ("thread_id", first.thread_id.unwrap())
            } else {
                ("session_id", first.session_id)
            };
            let count:i64=db(conn.query_row(&format!("SELECT COUNT(*) FROM segments WHERE deleted_at IS NULL AND {field}=?1 AND (t_start_ns,id)>=(?2,?3) AND (t_start_ns,id)<=(?4,?5)"),params![group,first.t_start_ns,first.id,last.t_start_ns,last.id],|r|r.get(0)))?;
            if count != rows.len() as i64 {
                return Err(Error::params("choose a consecutive range of turns"));
            }
            let id = if let Some(id) = req.opt_i64("id")? {
                if db(conn.execute(
                    "UPDATE saved_moments SET title=?1,note=?2,original_count=?3 WHERE id=?4",
                    params![title, note, rows.len() as i64, id],
                ))? == 0
                {
                    return Err(Error::not_found("saved moment not found"));
                }
                db(conn.execute("DELETE FROM saved_moment_segments WHERE moment_id=?1", [id]))?;
                id
            } else {
                db(conn.execute("INSERT INTO saved_moments(title,note,original_count,created_ms) VALUES(?1,?2,?3,?4)",params![title,note,rows.len() as i64,clock::utc_now_ns()/1_000_000]))?;
                conn.last_insert_rowid()
            };
            if req.params.get("collection_id").is_some() {
                let target = collection_param(conn, req)?;
                db(conn.execute(
                    "UPDATE saved_moments SET collection_id=?1 WHERE id=?2",
                    params![target, id],
                ))?;
            }
            for (ordinal, row) in rows.iter().enumerate() {
                db(conn.execute("INSERT INTO saved_moment_segments(moment_id,segment_id,ordinal) VALUES(?1,?2,?3)",params![id,row.id,ordinal as i64]))?;
            }
            let result =
                moment(store, id)?.ok_or_else(|| Error::not_found("source unavailable"))?;
            db(tx.commit())?;
            Ok(json!({"moment":result}))
        }
        "saved.searches.delete" | "saved.moments.delete" => {
            let id = positive(req, "id")?;
            let table = if req.method == "saved.searches.delete" {
                "saved_searches"
            } else {
                "saved_moments"
            };
            let removed = db(conn.execute(&format!("DELETE FROM {table} WHERE id=?1"), [id]))? > 0;
            Ok(json!({"removed":removed}))
        }
        _ => Err(Error::new("method", "unknown saved-item method")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(s: &Store, method: &str, params: Value) -> Result<Value, Error> {
        handle(
            s,
            &Request {
                id: json!(1),
                method: method.into(),
                params,
            },
        )
    }
    fn seed() -> (Store, Vec<i64>) {
        let s = Store::open_in_memory().unwrap();
        let source = s.upsert_source("synthetic", "Synthetic", 1).unwrap();
        let session = s.begin_session(source, 1).unwrap();
        let ids = (0..4)
            .map(|i| {
                s.insert_segment(
                    session,
                    (i + 1) * 1_000_000_000,
                    (i + 2) * 1_000_000_000,
                    "",
                    1,
                )
                .unwrap()
            })
            .collect();
        (s, ids)
    }
    #[test]
    fn saves_search_filters_without_reinterpreting_relative_or_fixed_dates() {
        let (s, _) = seed();
        let filters = json!({"date_scope":"week","speaker":2,"from":"2026-09-01"});
        let out = call(
            &s,
            "saved.searches.save",
            json!({"name":"World links","query":"portal","filters":filters}),
        )
        .unwrap();
        let id = out["search"]["id"].as_i64().unwrap();
        assert_eq!(out["search"]["filters"], filters);
        call(
            &s,
            "saved.searches.save",
            json!({"id":id,"name":"Renamed","query":"roof","filters":{"date_scope":"all"}}),
        )
        .unwrap();
        let list = call(&s, "saved.searches.list", json!({})).unwrap();
        assert_eq!(list["total"], 1);
        assert_eq!(list["searches"][0]["name"], "Renamed");
        assert_eq!(
            call(&s, "saved.searches.delete", json!({"id":id})).unwrap()["removed"],
            true
        );
    }
    #[test]
    fn moment_sources_follow_edits_soft_delete_undo_and_purge_without_copying_text() {
        let (s, ids) = seed();
        let out = call(
            &s,
            "saved.moments.save",
            json!({"segment_ids":[ids[0]],"title":"Useful","note":"My annotation"}),
        )
        .unwrap();
        let id = out["moment"]["id"].as_i64().unwrap();
        s.conn()
            .execute(
                "UPDATE segments SET text='corrected synthetic words' WHERE id=?1",
                [ids[0]],
            )
            .unwrap();
        assert_eq!(
            moment(&s, id).unwrap().unwrap()["segments"][0]["text"],
            "corrected synthetic words"
        );
        s.soft_delete_segments(&[ids[0]], 2).unwrap();
        assert!(moment(&s, id).unwrap().is_none());
        s.conn()
            .execute("UPDATE segments SET deleted_at=NULL WHERE id=?1", [ids[0]])
            .unwrap();
        assert!(moment(&s, id).unwrap().is_some());
        s.purge_segments(&[ids[0]]).unwrap();
        let n: i64 = s
            .conn()
            .query_row("SELECT COUNT(*) FROM saved_moments", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }
    #[test]
    fn rejects_noncontiguous_and_invalid_ranges_atomically() {
        let (s, ids) = seed();
        assert_eq!(
            call(
                &s,
                "saved.moments.save",
                json!({"segment_ids":[ids[0],ids[2]]})
            )
            .unwrap_err()
            .code,
            "params"
        );
        assert!(
            call(
                &s,
                "saved.moments.save",
                json!({"segment_ids":[ids[0],ids[0]]})
            )
            .is_err()
        );
        let saved = call(
            &s,
            "saved.moments.save",
            json!({"segment_ids":[ids[0],ids[1]],"title":"Before"}),
        )
        .unwrap();
        let id = saved["moment"]["id"].as_i64().unwrap();
        assert!(
            call(
                &s,
                "saved.moments.save",
                json!({"id":id,"segment_ids":[ids[0],999],"title":"After"})
            )
            .is_err()
        );
        assert_eq!(moment(&s, id).unwrap().unwrap()["title"], "Before");
        let changed = call(
            &s,
            "saved.moments.save",
            json!({"id":id,"segment_ids":[ids[1]],"title":"Changed"}),
        )
        .unwrap();
        assert_eq!(changed["moment"]["segment_ids"], json!([ids[1]]));
    }
    #[test]
    fn context_is_bounded_ordered_and_omits_deleted_turns() {
        let (s, ids) = seed();
        s.soft_delete_segments(&[ids[0]], 2).unwrap();
        let r = call(
            &s,
            "segments.context",
            json!({"id":ids[2],"before":1,"after":1}),
        )
        .unwrap();
        assert_eq!(
            r["segments"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            ids[1..].to_vec()
        );
        assert!(call(&s, "segments.context", json!({"id":ids[0]})).is_err());
        assert!(call(&s, "segments.context", json!({"id":ids[1],"before":11})).is_err());
    }
    #[test]
    fn purging_shared_sources_keeps_only_moments_with_remaining_sources() {
        let (s, ids) = seed();
        let a = call(
            &s,
            "saved.moments.save",
            json!({"segment_ids":[ids[0],ids[1]],"note":"Keep until both gone"}),
        )
        .unwrap()["moment"]["id"]
            .as_i64()
            .unwrap();
        let b = call(
            &s,
            "saved.moments.save",
            json!({"segment_ids":[ids[0]],"note":"Only first"}),
        )
        .unwrap()["moment"]["id"]
            .as_i64()
            .unwrap();
        s.purge_segments(&[ids[0]]).unwrap();
        assert!(moment(&s, b).unwrap().is_none());
        let remaining = moment(&s, a).unwrap().unwrap();
        assert_eq!(remaining["segment_ids"], json!([ids[1]]));
        assert_eq!(remaining["unavailable_count"], 1);
        assert_eq!(remaining["note"], "Keep until both gone");
        s.purge_segments(&[ids[1]]).unwrap();
        assert!(moment(&s, a).unwrap().is_none());
        assert_eq!(
            s.conn()
                .query_row("SELECT COUNT(*) FROM saved_moments", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn saved_items_survive_reopen() {
        let path = std::env::temp_dir().join(format!("nx-saved-reopen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        {
            let s = Store::open(&path).unwrap();
            call(
                &s,
                "saved.searches.save",
                json!({"name":"Later","query":"test"}),
            )
            .unwrap();
            let source = s.upsert_source("synthetic", "Synthetic", 1).unwrap();
            let session = s.begin_session(source, 1).unwrap();
            let id = s.insert_segment(session, 1, 2, "", 1).unwrap();
            call(
                &s,
                "saved.moments.save",
                json!({"segment_ids":[id],"note":"Remember"}),
            )
            .unwrap();
        }
        {
            let s = Store::open(&path).unwrap();
            assert_eq!(
                call(&s, "saved.searches.list", json!({})).unwrap()["total"],
                1
            );
            assert_eq!(
                call(&s, "saved.moments.list", json!({})).unwrap()["moments"][0]["note"],
                "Remember"
            );
        }
        let _ = std::fs::remove_dir_all(path);
    }
    #[test]
    fn collections_crud_move_filter_and_retention() {
        let (s, ids) = seed();
        let c=call(&s,"saved.collections.save",json!({"name":"Projects"})).unwrap()["collection"]["id"].as_i64().unwrap();
        let m = call(
            &s,
            "saved.moments.save",
            json!({"segment_ids":[ids[0],ids[1]],"collection_id":c,"note":"Personal"}),
        )
        .unwrap()["moment"]["id"]
            .as_i64()
            .unwrap();
        assert_eq!(
            call(&s, "saved.collections.list", json!({})).unwrap()["collections"][0]["count"],
            1
        );
        assert_eq!(
            call(&s, "saved.moments.list", json!({"collection_id":null})).unwrap()["total"],
            0
        );
        call(&s, "saved.collections.save", json!({"id":c,"name":"Plans"})).unwrap();
        assert!(call(&s, "saved.collections.save", json!({"name":"plans"})).is_err());
        assert!(
            call(
                &s,
                "saved.moments.move",
                json!({"id":m,"collection_id":999})
            )
            .is_err()
        );
        assert_eq!(
            call(&s, "saved.moments.get", json!({"id":m})).unwrap()["moment"]["collection_id"],
            c
        );
        call(
            &s,
            "saved.moments.save",
            json!({"id":m,"segment_ids":[ids[0],ids[1]],"note":"Changed"}),
        )
        .unwrap();
        assert_eq!(
            call(&s, "saved.moments.get", json!({"id":m})).unwrap()["moment"]["collection_id"],
            c
        );
        s.soft_delete_segments(&[ids[0]], 2).unwrap();
        let visible = call(&s, "saved.moments.get", json!({"id":m})).unwrap();
        assert_eq!(visible["moment"]["segment_ids"], json!([ids[1]]));
        assert_eq!(visible["moment"]["unavailable_count"], 1);
        call(
            &s,
            "saved.moments.move",
            json!({"id":m,"collection_id":null}),
        )
        .unwrap();
        assert_eq!(
            call(&s, "saved.moments.list", json!({"collection_id":null})).unwrap()["total"],
            1
        );
        call(&s, "saved.moments.move", json!({"id":m,"collection_id":c})).unwrap();
        call(&s, "saved.collections.delete", json!({"id":c})).unwrap();
        assert!(
            call(&s, "saved.moments.get", json!({"id":m})).unwrap()["moment"]["collection_id"]
                .is_null()
        );
        s.conn()
            .execute(
                "DELETE FROM segments WHERE id IN (?1,?2)",
                params![ids[0], ids[1]],
            )
            .unwrap();
        assert!(call(&s, "saved.moments.get", json!({"id":m})).is_err());
        assert_eq!(
            call(&s, "saved.moments.list", json!({})).unwrap()["total"],
            0
        );
    }
    #[test]
    fn history_cursor_ties_deletions_and_later_inserts_do_not_skip_or_duplicate() {
        let (s, ids) = seed();
        s.conn()
            .execute("UPDATE segments SET t_start_ns=1000000000", [])
            .unwrap();
        let params = json!({"from":"1970-01-01T00:00:00Z","to":"1970-01-02T00:00:00Z","limit":2});
        let first = call(&s, "history.page", params.clone()).unwrap();
        assert_eq!(
            first["segments"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            ids[..2]
        );
        let session = s.segment_row(ids[0]).unwrap().unwrap().session_id;
        let late = s
            .insert_segment(session, 1000000000, 2000000000, "", 1)
            .unwrap();
        s.soft_delete_segments(&[ids[2]], 2).unwrap();
        let mut next = params.clone();
        next["cursor"] = first["next_cursor"].clone();
        let second = call(&s, "history.page", next.clone()).unwrap();
        assert_eq!(
            second["segments"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            vec![ids[3]]
        );
        assert!(second["next_cursor"].is_null());
        next["to"] = json!("1970-01-03T00:00:00Z");
        assert_eq!(call(&s, "history.page", next).unwrap_err().code, "params");
        let all = call(
            &s,
            "history.page",
            json!({"from":"1970-01-01T00:00:00Z","to":"1970-01-02T00:00:00Z","limit":200}),
        )
        .unwrap();
        assert!(
            all["segments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == late)
        );
    }
    #[test]
    fn history_and_collections_survive_reopen() {
        let path = std::env::temp_dir().join(format!(
            "nx-history-collections-reopen-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        let (cursor, last, collection_id) = {
            let s = Store::open(&path).unwrap();
            let source = s.upsert_source("synthetic", "Synthetic", 1).unwrap();
            let session = s.begin_session(source, 1).unwrap();
            let a = s.insert_segment(session, 1, 2, "", 1).unwrap();
            let b = s.insert_segment(session, 1, 3, "", 1).unwrap();
            let c=call(&s,"saved.collections.save",json!({"name":"Keep"})).unwrap()["collection"]["id"].as_i64().unwrap();
            call(
                &s,
                "saved.moments.save",
                json!({"segment_ids":[a,b],"collection_id":c}),
            )
            .unwrap();
            let page = call(
                &s,
                "history.page",
                json!({"from":"1970-01-01T00:00:00Z","to":"1970-01-02T00:00:00Z","limit":1}),
            )
            .unwrap();
            (page["next_cursor"].clone(), b, c)
        };
        {
            let s = Store::open(&path).unwrap();
            let page=call(&s,"history.page",json!({"from":"1970-01-01T00:00:00Z","to":"1970-01-02T00:00:00Z","limit":1,"cursor":cursor})).unwrap();
            assert_eq!(page["segments"][0]["id"], last);
            assert_eq!(
                call(
                    &s,
                    "saved.moments.list",
                    json!({"collection_id":collection_id})
                )
                .unwrap()["total"],
                1
            );
        }
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn collections_migration_is_idempotent_on_v22_and_reopen() {
        let (s, ids) = seed();
        s.conn().execute_batch("DROP INDEX idx_saved_moments_collection; ALTER TABLE saved_moments DROP COLUMN collection_id; DROP TABLE saved_collections; DROP INDEX idx_history_visible; UPDATE schema_version SET version=22;").unwrap();
        migrate_v23(s.conn()).unwrap();
        let c=call(&s,"saved.collections.save",json!({"name":"Migrated"})).unwrap()["collection"]["id"].as_i64().unwrap();
        call(
            &s,
            "saved.moments.save",
            json!({"segment_ids":[ids[0]],"collection_id":c}),
        )
        .unwrap();
        migrate_v23(s.conn()).unwrap();
        assert_eq!(
            call(&s, "saved.moments.list", json!({"collection_id":c})).unwrap()["total"],
            1
        );
    }
}
