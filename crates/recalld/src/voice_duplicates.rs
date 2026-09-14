//! Optional, source-scoped suppression of acoustically confirmed microphone copies.
//! Observations preserve shared recognition provenance without duplicate audio/history.
use crate::store::{SegmentFilter, Store};
use anyhow::{Result, bail};
use rusqlite::params;

pub const SOURCES_KEY: &str = "voice_mic_duplicate_sources";

pub fn sources(store: &Store) -> Result<Vec<String>> {
    match store.setting(SOURCES_KEY)? {
        Some(raw) => Ok(serde_json::from_str(&raw)?),
        None => Ok(Vec::new()),
    }
}

pub fn set_sources(store: &Store, sources: &[String]) -> Result<()> {
    if sources.len() > 16
        || sources.iter().any(|s| {
            s.is_empty()
                || s.len() > 200
                || s.trim() != s
                || s.chars().any(char::is_control)
                || matches!(s.as_str(), "mic" | "room")
        })
    {
        bail!("sources must contain at most 16 application source keys");
    }
    let mut clean = sources.to_vec();
    clean.sort();
    clean.dedup();
    store.set_setting(SOURCES_KEY, &serde_json::to_string(&clean)?)?;
    Ok(())
}

pub fn migrate(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS voice_source_observations (
        canonical_segment_id INTEGER NOT NULL REFERENCES segments(id) ON DELETE CASCADE,
        session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
        t_start_ns INTEGER NOT NULL,
        t_end_ns INTEGER NOT NULL CHECK(t_end_ns > t_start_ns),
        correlation REAL NOT NULL CHECK(correlation >= 0 AND correlation <= 1),
        PRIMARY KEY(session_id,t_start_ns,t_end_ns)
    ); CREATE INDEX IF NOT EXISTS voice_source_observation_time ON voice_source_observations(session_id,t_start_ns);
       CREATE INDEX IF NOT EXISTS voice_source_observation_canonical ON voice_source_observations(canonical_segment_id);")?;
    Ok(())
}

/// A failed insert or an absent/deleted/wordless mic row must retain the app audio.
pub fn observe(
    store: &Store,
    canonical: i64,
    session: i64,
    start: i64,
    end: i64,
    correlation: f32,
) -> Result<bool> {
    if !correlation.is_finite() || !(0.0..=1.0).contains(&correlation) || end <= start {
        return Ok(false);
    }
    let n = store.conn().execute("INSERT OR IGNORE INTO voice_source_observations(canonical_segment_id,session_id,t_start_ns,t_end_ns,correlation)
        SELECT g.id,?2,?3,?4,?5 FROM segments g
        JOIN sessions ss ON ss.id=g.session_id JOIN sources sc ON sc.id=ss.source_id
        WHERE g.id=?1 AND g.deleted_at IS NULL AND sc.kind='mic' AND LENGTH(TRIM(COALESCE(g.text,''))) > 0
        AND EXISTS(SELECT 1 FROM sessions app JOIN sources appsc ON appsc.id=app.source_id WHERE app.id=?2 AND appsc.kind='app')",
        params![canonical,session,start,end,correlation])?;
    Ok(n == 1)
}

/// Only the bounded voice query reads these observations; ordinary history stays canonical.
pub fn transcript(
    store: &Store,
    source: &str,
    from: i64,
    to: i64,
    limit: usize,
) -> Result<Vec<serde_json::Value>> {
    let filter = SegmentFilter {
        source: Some(source.into()),
        from: Some(from),
        to: Some(to),
        ..Default::default()
    };
    let mut rows: Vec<_> = store
        .segment_rows(&filter, limit)?
        .iter()
        .map(crate::service::segment_json)
        .collect();
    let mut stmt = store.conn().prepare("SELECT o.canonical_segment_id,o.session_id,o.t_start_ns,o.t_end_ns,o.correlation
        FROM voice_source_observations o JOIN sessions ss ON ss.id=o.session_id JOIN sources sc ON sc.id=ss.source_id
        JOIN segments g ON g.id=o.canonical_segment_id
        WHERE sc.match_key=?1 AND o.t_start_ns>=?2 AND o.t_start_ns<?3 AND g.deleted_at IS NULL AND LENGTH(TRIM(COALESCE(g.text,''))) > 0
        ORDER BY o.t_start_ns,o.canonical_segment_id LIMIT ?4")?;
    let observations = stmt
        .query_map(params![source, from, to, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, f64>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, session, start, end, correlation) in observations {
        if let Some(canonical) = store.segment_row(id)? {
            let mut row = crate::service::segment_json(&canonical);
            row["canonical_source"] = serde_json::json!(canonical.source);
            row["canonical_segment_id"] = serde_json::json!(id);
            row["source"] = serde_json::json!(source);
            row["session_id"] = serde_json::json!(session);
            row["t_start_ns"] = serde_json::json!(start.to_string());
            row["t_end_ns"] = serde_json::json!(end.to_string());
            row["provenance"] = serde_json::json!("confirmed_mic_audio");
            row["audio_correlation"] = serde_json::json!(correlation);
            rows.push(row);
        }
    }
    rows.sort_by_key(|row| {
        (
            row["t_start_ns"]
                .as_str()
                .and_then(|v| v.parse::<i64>().ok()),
            row["id"].as_i64(),
        )
    });
    rows.truncate(limit);
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Store, i64, i64, i64) {
        let s = Store::open_in_memory().unwrap();
        let mic = s.upsert_source_kind("mic", "Microphone", "mic", 0).unwrap();
        let app = s.upsert_source("vesktop", "Virtual in/out", 0).unwrap();
        let ms = s.begin_session(mic, 0).unwrap();
        let app = s.begin_session(app, 0).unwrap();
        let id = s.insert_segment(ms, 1_000, 2_000, "mic.wav", 0).unwrap();
        s.correct_segment_text(id, "Hello Lanalu").unwrap();
        (s, id, ms, app)
    }
    #[test]
    fn scoped_observation_keeps_canonical_history_and_shared_source_times() {
        let (s, id, ms, app) = fixture();
        assert!(sources(&s).unwrap().is_empty());
        set_sources(&s, &["vesktop".into()]).unwrap();
        assert_eq!(sources(&s).unwrap(), vec!["vesktop"]);
        assert!(observe(&s, id, app, 1_200, 2_200, 0.99).unwrap());
        assert_eq!(s.segment_count(ms).unwrap(), 1);
        assert_eq!(s.segment_count(app).unwrap(), 0);
        let ordinary = s
            .segment_rows(
                &SegmentFilter {
                    source: Some("vesktop".into()),
                    ..Default::default()
                },
                64,
            )
            .unwrap();
        assert!(ordinary.is_empty());
        let rows = transcript(&s, "vesktop", 0, 3000, 64).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["id"], id);
        assert_eq!(rows[0]["source"], "vesktop");
        assert_eq!(rows[0]["canonical_source"], "mic");
        assert_eq!(rows[0]["t_start_ns"], "1200");
        assert_eq!(rows[0]["text"], "Hello Lanalu");
        assert!(transcript(&s, "discord", 0, 3000, 64).unwrap().is_empty());
        s.correct_segment_text(id, "Updated words").unwrap();
        assert_eq!(
            transcript(&s, "vesktop", 0, 3000, 64).unwrap()[0]["text"],
            "Updated words"
        );
        s.soft_delete_segments(&[id], 3_000).unwrap();
        assert!(transcript(&s, "vesktop", 0, 3000, 64).unwrap().is_empty());
    }
    #[test]
    fn observation_failure_does_not_suppress_and_merged_results_are_chronological() {
        let (s, id, ms, app) = fixture();
        assert!(!observe(&s, id, ms, 1200, 2200, 0.99).unwrap());
        assert!(!observe(&s, 999, app, 1200, 2200, 0.99).unwrap());
        assert!(!observe(&s, id, app, 1200, 2200, f32::NAN).unwrap());
        assert!(observe(&s, id, app, 1200, 2200, 0.99).unwrap());
        let later = s.insert_segment(app, 2300, 2900, "other.wav", 0).unwrap();
        s.correct_segment_text(later, "Other person").unwrap();
        let rows = transcript(&s, "vesktop", 0, 3000, 1).unwrap();
        assert_eq!(rows[0]["id"], id);
        s.conn().execute_batch("CREATE TRIGGER fail_observation BEFORE INSERT ON voice_source_observations BEGIN SELECT RAISE(ABORT,'test failure'); END;").unwrap();
        assert!(observe(&s, id, app, 1250, 2250, 0.99).is_err());
        assert_eq!(s.segment_count(ms).unwrap(), 1);
        assert_eq!(s.segment_count(app).unwrap(), 1);
        for values in [
            vec!["mic".into()],
            vec!["room".into()],
            vec![" x".into()],
            vec!["x".repeat(201)],
        ] {
            assert!(set_sources(&s, &values).is_err());
        }
    }
}
