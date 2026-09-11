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
fn moment(store: &Store, id: i64) -> Result<Option<Value>, Error> {
    let conn = store.conn();
    let Some((title, note, count, created)) = db(conn
        .query_row(
            "SELECT title,note,original_count,created_ms FROM saved_moments WHERE id=?1",
            [id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
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
        "created_ms":created,"unavailable_count":count-segments.len() as i64}),
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
            let mut stmt=db(conn.prepare("SELECT m.id FROM saved_moments m WHERE EXISTS(SELECT 1 FROM saved_moment_segments r JOIN segments g ON g.id=r.segment_id WHERE r.moment_id=m.id AND g.deleted_at IS NULL) ORDER BY created_ms DESC,id DESC LIMIT ?1 OFFSET ?2"))?;
            let ids = db(
                db(stmt.query_map(params![limit, offset], |r| r.get::<_, i64>(0)))?
                    .collect::<rusqlite::Result<Vec<_>>>(),
            )?;
            let mut rows = Vec::new();
            for id in ids {
                if let Some(row) = moment(store, id)? {
                    rows.push(row)
                }
            }
            let total:i64=db(conn.query_row("SELECT COUNT(*) FROM saved_moments m WHERE EXISTS(SELECT 1 FROM saved_moment_segments r JOIN segments g ON g.id=r.segment_id WHERE r.moment_id=m.id AND g.deleted_at IS NULL)",[],|r|r.get(0)))?;
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
}
