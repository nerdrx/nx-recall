//! A bounded inbox for uncertain words, not uncertain speaker identities.
//! State references source rows only; text/audio never get copied into the inbox.
use crate::{
    proto::{Error, Request},
    service::segment_json,
    store::Store,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};

pub fn migrate_v24(conn: &Connection) -> anyhow::Result<()> {
    let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='recognition_review')", [], |r| r.get(0))?;
    if exists {
        return Ok(());
    }
    conn.execute_batch("SAVEPOINT recognition_review_v24")?;
    let result = conn.execute_batch("CREATE TABLE recognition_review (
      segment_id INTEGER PRIMARY KEY REFERENCES segments(id) ON DELETE CASCADE,
      revision INTEGER NOT NULL DEFAULT 1, pending INTEGER NOT NULL,
      reviewed_revision INTEGER);
      CREATE INDEX idx_recognition_review_pending ON recognition_review(segment_id DESC) WHERE pending=1;
      INSERT INTO recognition_review(segment_id,pending)
        SELECT id,1 FROM segments WHERE deleted_at IS NULL AND length(trim(coalesce(text,'')))>0
          AND (asr_confidence='shaky' OR lang_via='mismatch');
      CREATE TRIGGER recognition_review_insert AFTER INSERT ON segments
        WHEN new.deleted_at IS NULL AND length(trim(coalesce(new.text,'')))>0
          AND (new.asr_confidence='shaky' OR new.lang_via='mismatch') BEGIN
        INSERT INTO recognition_review(segment_id,pending) VALUES(new.id,1);
      END;
      CREATE TRIGGER recognition_review_update AFTER UPDATE OF text,asr_model_id,asr_confidence,confidence_at_ns,lang_via,deleted_at ON segments
        WHEN old.text IS NOT new.text OR old.asr_model_id IS NOT new.asr_model_id
          OR old.asr_confidence IS NOT new.asr_confidence OR old.confidence_at_ns IS NOT new.confidence_at_ns
          OR old.lang_via IS NOT new.lang_via OR old.deleted_at IS NOT new.deleted_at BEGIN
        INSERT INTO recognition_review(segment_id,pending)
          SELECT new.id,coalesce(new.deleted_at IS NULL AND length(trim(coalesce(new.text,'')))>0
            AND (new.asr_confidence='shaky' OR new.lang_via='mismatch'),0)
          WHERE EXISTS(SELECT 1 FROM recognition_review WHERE segment_id=new.id)
            OR (new.deleted_at IS NULL AND length(trim(coalesce(new.text,'')))>0
              AND (new.asr_confidence='shaky' OR new.lang_via='mismatch'))
          ON CONFLICT(segment_id) DO UPDATE SET revision=revision+1,
            pending=excluded.pending, reviewed_revision=NULL;
      END;");
    match result {
        Ok(()) => {
            conn.execute_batch("RELEASE recognition_review_v24")?;
            Ok(())
        }
        Err(error) => {
            conn.execute_batch(
                "ROLLBACK TO recognition_review_v24; RELEASE recognition_review_v24",
            )?;
            Err(error.into())
        }
    }
}
fn db<T, E: std::fmt::Display>(result: Result<T, E>) -> Result<T, Error> {
    result.map_err(|e| Error::new("database", e.to_string()))
}

pub fn handle(store: &Store, req: &Request) -> Result<Value, Error> {
    match req.method.as_str() {
        "review.list" => {
            let limit = req.opt_i64("limit")?.unwrap_or(30);
            let before = req.opt_i64("before_id")?.unwrap_or(i64::MAX);
            if !(1..=100).contains(&limit) || before <= 0 {
                return Err(Error::params("limit must be 1–100 and before_id positive"));
            }
            let tx = db(store.conn().unchecked_transaction())?;
            let mut stmt=db(tx.prepare("SELECT segment_id,revision FROM recognition_review WHERE pending=1 AND segment_id<?1 ORDER BY segment_id DESC LIMIT ?2"))?;
            let mut rows = db(db(stmt.query_map(params![before, limit + 1], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            }))?
            .collect::<rusqlite::Result<Vec<_>>>())?;
            let more = rows.len() > limit as usize;
            rows.truncate(limit as usize);
            let next = if more { rows.last().map(|r| r.0) } else { None };
            let mut items = Vec::new();
            for (id, revision) in rows {
                if let Some(row) = db(store.segment_row(id))? {
                    let mut item = segment_json(&row);
                    item["review_revision"] = json!(revision);
                    let mut reasons = Vec::new();
                    if row.asr_confidence.as_deref() == Some("shaky") {
                        reasons.push("decoder_disagreement");
                    }
                    if row.lang_via.as_deref() == Some("mismatch") {
                        reasons.push("language_mismatch");
                    }
                    item["review_reasons"] = json!(reasons);
                    items.push(item);
                }
            }
            drop(stmt);
            db(tx.commit())?;
            Ok(json!({"items":items,"next_before_id":next}))
        }
        "review.get" => {
            let id = req.i64("segment_id")?;
            if id <= 0 {
                return Err(Error::params("segment_id must be positive"));
            }
            let tx = db(store.conn().unchecked_transaction())?;
            let revision: Option<i64> = db(tx
                .query_row(
                    "SELECT revision FROM recognition_review WHERE segment_id=?1",
                    [id],
                    |r| r.get(0),
                )
                .optional())?;
            let row = db(store.segment_row(id))?;
            let item = match (revision, row) {
                (Some(revision), Some(row)) => {
                    let mut v = segment_json(&row);
                    v["review_revision"] = json!(revision);
                    v
                }
                _ => return Err(Error::not_found("Review source is unavailable")),
            };
            db(tx.commit())?;
            Ok(json!({"item":item}))
        }
        "review.mark" => {
            let id = req.i64("segment_id")?;
            let revision = req.i64("revision")?;
            if id <= 0 || revision <= 0 {
                return Err(Error::params("segment_id and revision must be positive"));
            }
            let changed=db(store.conn().execute("UPDATE recognition_review SET pending=0,reviewed_revision=revision
              WHERE segment_id=?1 AND revision=?2 AND EXISTS(SELECT 1 FROM segments WHERE id=?1 AND deleted_at IS NULL)",params![id,revision]))?;
            if changed == 0 {
                return Err(Error::new(
                    "conflict",
                    "Transcript or review evidence changed. Refresh before marking reviewed.",
                ));
            }
            Ok(json!({"reviewed":true}))
        }
        _ => Err(Error::new("method", "unknown recognition review method")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(s: &Store, method: &str, p: Value) -> Result<Value, Error> {
        handle(
            s,
            &Request {
                id: json!(1),
                method: method.into(),
                params: p,
            },
        )
    }
    fn fixture() -> (Store, i64) {
        let s = Store::open_in_memory().unwrap();
        migrate_v24(s.conn()).unwrap();
        let source = s.upsert_source("synthetic", "synthetic", 1).unwrap();
        let session = s.begin_session(source, 1).unwrap();
        let id = s.insert_segment(session, 1, 2, "", 1).unwrap();
        s.correct_segment_text(id, "synthetic words").unwrap();
        (s, id)
    }
    #[test]
    fn only_word_evidence_enters_inbox_and_mark_is_revision_guarded() {
        let (s, id) = fixture();
        assert_eq!(
            call(&s, "review.list", json!({})).unwrap()["items"],
            json!([])
        );
        s.set_segment_confidence(id, Some("shaky"), 1).unwrap();
        let list = call(&s, "review.list", json!({})).unwrap();
        let rev = list["items"][0]["review_revision"].as_i64().unwrap();
        assert_eq!(
            list["items"][0]["review_reasons"],
            json!(["decoder_disagreement"])
        );
        call(&s, "review.mark", json!({"segment_id":id,"revision":rev})).unwrap();
        assert_eq!(
            call(&s, "review.list", json!({})).unwrap()["items"],
            json!([])
        );
        s.set_segment_confidence(id, Some("shaky"), 2).unwrap();
        assert!(call(&s, "review.mark", json!({"segment_id":id,"revision":rev})).is_err());
        assert_eq!(
            call(&s, "review.list", json!({})).unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn correction_model_changes_and_deletion_invalidate_review() {
        let (s, id) = fixture();
        s.mark_segment_language_mismatch(id).unwrap();
        let first = call(&s, "review.list", json!({})).unwrap();
        let rev = first["items"][0]["review_revision"].as_i64().unwrap();
        call(&s, "review.mark", json!({"segment_id":id,"revision":rev})).unwrap();
        s.correct_segment_text(id, "changed words").unwrap();
        assert!(call(&s, "review.mark", json!({"segment_id":id,"revision":rev})).is_err());
        // Language mismatch may survive correction; current words must be reviewed anew.
        s.mark_segment_language_mismatch(id).unwrap();
        s.conn()
            .execute(
                "UPDATE segments SET asr_model_id='new-model' WHERE id=?1",
                [id],
            )
            .unwrap();
        assert_eq!(
            call(&s, "review.list", json!({})).unwrap()["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        s.soft_delete_segments(&[id], 2).unwrap();
        assert_eq!(
            call(&s, "review.list", json!({})).unwrap()["items"],
            json!([])
        );
        s.purge_segments(&[id]).unwrap();
        let count: i64 = s
            .conn()
            .query_row("SELECT count(*) FROM recognition_review", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
    #[test]
    fn failed_migration_rolls_back_and_success_does_not_commit_outer_transaction() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE segments(id INTEGER PRIMARY KEY); CREATE TABLE schema_version(version INTEGER); INSERT INTO schema_version VALUES(23);").unwrap();
        assert!(migrate_v24(&conn).is_err());
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='recognition_review')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !exists,
            "failed DDL must not leave a table that bypasses future migration"
        );
        assert_eq!(
            conn.query_row("SELECT version FROM schema_version", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            23
        );
        conn.execute_batch("ALTER TABLE segments ADD COLUMN text TEXT; ALTER TABLE segments ADD COLUMN deleted_at INTEGER;
          ALTER TABLE segments ADD COLUMN asr_model_id TEXT; ALTER TABLE segments ADD COLUMN asr_confidence TEXT;
          ALTER TABLE segments ADD COLUMN confidence_at_ns INTEGER; ALTER TABLE segments ADD COLUMN lang_via TEXT; BEGIN;").unwrap();
        migrate_v24(&conn).unwrap();
        conn.execute_batch("ROLLBACK").unwrap();
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='recognition_review')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !exists,
            "releasing helper savepoint must not commit its caller"
        );
    }

    #[test]
    fn paging_reaches_older_evidence_without_duplicates() {
        let (s, id) = fixture();
        s.set_segment_confidence(id, Some("shaky"), 1).unwrap();
        let session: i64 = s
            .conn()
            .query_row("SELECT session_id FROM segments WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        let mut expected = vec![id];
        for i in 2..6 {
            let next = s.insert_segment(session, i, i + 1, "", i).unwrap();
            s.correct_segment_text(next, "synthetic words").unwrap();
            s.mark_segment_language_mismatch(next).unwrap();
            expected.push(next);
        }
        expected.reverse();
        let mut ids = Vec::new();
        let mut before = None;
        loop {
            let reply = call(&s, "review.list", json!({"limit":2,"before_id":before})).unwrap();
            let items = reply["items"].as_array().unwrap();
            assert!(items.len() <= 2);
            ids.extend(items.iter().map(|item| item["id"].as_i64().unwrap()));
            before = reply["next_before_id"].as_i64();
            if before.is_none() {
                break;
            }
        }
        assert_eq!(ids, expected);
    }

    #[test]
    fn review_survives_reopen_and_voice_metadata_does_not_requeue_words() {
        let dir = std::env::temp_dir().join(format!(
            "nx-review-{}-{}",
            std::process::id(),
            crate::clock::monotonic_ns()
        ));
        let s = Store::open(&dir).unwrap();
        migrate_v24(s.conn()).unwrap();
        let source = s.upsert_source("synthetic", "synthetic", 1).unwrap();
        let session = s.begin_session(source, 1).unwrap();
        let id = s.insert_segment(session, 1, 2, "", 1).unwrap();
        s.correct_segment_text(id, "synthetic words").unwrap();
        s.set_segment_confidence(id, Some("shaky"), 1).unwrap();
        let item = call(&s, "review.get", json!({"segment_id":id})).unwrap();
        call(
            &s,
            "review.mark",
            json!({"segment_id":id,"revision":item["item"]["review_revision"]}),
        )
        .unwrap();
        drop(s);
        let s = Store::open(&dir).unwrap();
        migrate_v24(s.conn()).unwrap();
        s.conn()
            .execute(
                "UPDATE segments SET match_score=0.1,label_via='manual' WHERE id=?1",
                [id],
            )
            .unwrap();
        assert_eq!(
            call(&s, "review.list", json!({})).unwrap()["items"],
            json!([])
        );
        s.soft_delete_segments(&[id], 2).unwrap();
        assert!(call(&s, "review.get", json!({"segment_id":id})).is_err());
        assert!(
            call(
                &s,
                "review.mark",
                json!({"segment_id":id,"revision":item["item"]["review_revision"]})
            )
            .is_err()
        );
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn bounded_paging_and_migration_preserve_acknowledgements() {
        let (s, id) = fixture();
        s.set_segment_confidence(id, Some("shaky"), 1).unwrap();
        let first = call(&s, "review.list", json!({"limit":1})).unwrap();
        assert!(first["next_before_id"].is_null());
        call(
            &s,
            "review.mark",
            json!({"segment_id":id,"revision":first["items"][0]["review_revision"]}),
        )
        .unwrap();
        migrate_v24(s.conn()).unwrap();
        assert_eq!(
            call(&s, "review.list", json!({})).unwrap()["items"],
            json!([])
        );
        for params in [
            json!({"limit":0}),
            json!({"limit":101}),
            json!({"before_id":-1}),
        ] {
            assert!(call(&s, "review.list", params).is_err());
        }
    }
}
