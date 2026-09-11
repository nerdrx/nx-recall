//! Related saved moments from visible original words, with explicit lexical evidence.
use crate::{
    proto::{Error, Request},
    saved,
    store::Store,
};
use rusqlite::params;
use serde_json::{Value, json};
use std::collections::BTreeSet;
const CANDIDATES: usize = 200;
fn words(text: &str) -> BTreeSet<String> {
    const STOP: &[&str] = &[
        "this", "that", "with", "from", "have", "will", "your", "they", "them", "what", "when",
        "then", "there", "their", "about", "just", "like", "would", "could", "should", "been",
        "were", "also", "some", "into", "than", "very", "really", "because", "eine", "einen",
        "einer", "einem", "nicht", "dass", "aber", "auch", "noch", "schon", "oder", "sich", "wird",
        "sind", "haben", "dann", "dies", "eine", "über",
    ];
    text.split(|c: char| !c.is_alphanumeric())
        .filter_map(|part| {
            let word = part.to_lowercase();
            (word.chars().count() >= 4 && word.len() <= 96 && !STOP.contains(&word.as_str()))
                .then_some(word)
        })
        .collect()
}
fn moment_words(moment: &Value) -> BTreeSet<String> {
    moment["segments"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|row| {
            words(
                &row["text"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(4096)
                    .collect::<String>(),
            )
        })
        .collect()
}
pub fn handle(store: &Store, req: &Request) -> Result<Value, Error> {
    let id = req.i64("id")?;
    let limit = req.opt_i64("limit")?.unwrap_or(5);
    if id <= 0 || !(1..=10).contains(&limit) {
        return Err(Error::params("id must be positive and limit 1–10"));
    }
    let source = saved::moment(store, id)?
        .ok_or_else(|| Error::not_found("saved moment has no retained source turns"))?;
    let source_words = moment_words(&source);
    // Bound the FTS expression independently of the length/number of saved turns.
    let query = source_words
        .iter()
        .take(24)
        .map(|w| format!("\"{}\"", w.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ");
    if query.is_empty() {
        return Ok(
            json!({"available":true,"method":"shared_words","moments":[],"candidate_limit":CANDIDATES}),
        );
    }
    let conn = store.conn();
    let mut stmt=conn.prepare("SELECT r.moment_id,COUNT(DISTINCT r.segment_id) AS matches FROM segments_fts
        JOIN segments s ON s.id=segments_fts.rowid JOIN saved_moment_segments r ON r.segment_id=s.id
        WHERE segments_fts MATCH ?1 AND s.deleted_at IS NULL AND r.moment_id<>?2
        AND NOT EXISTS(SELECT 1 FROM saved_moment_segments a JOIN saved_moment_segments b ON a.segment_id=b.segment_id
          WHERE a.moment_id=r.moment_id AND b.moment_id=?2)
        GROUP BY r.moment_id ORDER BY matches DESC,r.moment_id DESC LIMIT ?3").map_err(|e| Error::internal(e.to_string()))?;
    let ids = stmt
        .query_map(params![query, id, CANDIDATES as i64], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(|e| Error::internal(e.to_string()))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| Error::internal(e.to_string()))?;
    let mut ranked = Vec::new();
    for other in ids {
        let Some(mut moment) = saved::moment(store, other)? else {
            continue;
        };
        let target = moment_words(&moment);
        let shared: Vec<_> = source_words.intersection(&target).cloned().collect();
        // One generic shared token is not enough evidence to recommend a moment.
        if shared.len() < 2 {
            continue;
        }
        let union = source_words.union(&target).count();
        let score = shared.len() as f64 / union.max(1) as f64;
        let evidence: Vec<_> = shared.into_iter().take(6).collect();
        moment["reason"] = json!(format!("Shared words: {}", evidence.join(", ")));
        moment["shared_words"] = json!(evidence);
        ranked.push((score, other, moment));
    }
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    Ok(
        json!({"available":true,"method":"shared_words","moments":ranked.into_iter().take(limit as usize).map(|r|r.2).collect::<Vec<_>>(),"candidate_limit":CANDIDATES}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    fn save(s: &Store, session: i64, text: &str, note: &str) -> (i64, i64) {
        let seg = s.insert_segment(session, 1, 2, "", 1).unwrap();
        s.correct_segment_text(seg, text).unwrap();
        let r = saved::handle(
            s,
            &Request {
                id: json!(1),
                method: "saved.moments.save".into(),
                params: json!({"segment_ids":[seg],"note":note}),
            },
        )
        .unwrap();
        (r["moment"]["id"].as_i64().unwrap(), seg)
    }
    fn get(s: &Store, id: i64) -> Value {
        handle(
            s,
            &Request {
                id: json!(1),
                method: "saved.moments.related".into(),
                params: json!({"id":id}),
            },
        )
        .unwrap()
    }
    #[test]
    fn relations_use_visible_original_words_not_notes_and_follow_edits() {
        let s = Store::open_in_memory().unwrap();
        let src = s.upsert_source("test", "Synthetic", 1).unwrap();
        let session = s.begin_session(src, 1).unwrap();
        let (a, _) = save(&s, session, "shader rendering performance", "unrelated");
        let (b, seg) = save(&s, session, "shader rendering optimization", "");
        let (_, _) = save(
            &s,
            session,
            "different conversation entirely",
            "shader rendering performance",
        );
        let r = get(&s, a);
        assert_eq!(r["moments"].as_array().unwrap().len(), 1);
        assert_eq!(r["moments"][0]["id"], b);
        assert_eq!(
            r["moments"][0]["shared_words"],
            json!(["rendering", "shader"])
        );
        s.correct_segment_text(seg, "corrected unrelated discussion")
            .unwrap();
        assert!(get(&s, a)["moments"].as_array().unwrap().is_empty());
        s.correct_segment_text(seg, "shader rendering").unwrap();
        s.soft_delete_segments(&[seg], 3).unwrap();
        assert!(get(&s, a)["moments"].as_array().unwrap().is_empty());
    }
    #[test]
    fn words_are_literal_bounded_tokens_and_common_words_do_not_recommend() {
        assert!(words("this that with eine nicht").is_empty());
        assert_eq!(
            words("\"shader\" OR rendering <script>"),
            BTreeSet::from(["shader".into(), "rendering".into(), "script".into()])
        );
    }
}
