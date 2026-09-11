//! Semantic search over transcript text (DESIGN §6/§7).
//!
//! FTS5 answers "which turn contains these words". This answers the question
//! the user actually has, which is "which turn *meant* this" — including when
//! the turn was in the other language. A German query finding an English
//! sentence it shares no word with is not a bonus feature here; it is the
//! reason the leg exists, because the two languages in this house are German
//! and English and a memory does not remember which one it was in.
//!
//! ## What runs
//!
//! `intfloat/multilingual-e5-small`, the int8 ONNX export, 384 dimensions,
//! 118 MB. Measured against `sentence-transformers/paraphrase-multilingual-
//! MiniLM-L12-v2` on a 48-segment de/en corpus with 43 in-language and 10
//! cross-language probes:
//!
//! | | in-language R@1 | cross-language R@1 | worst cross rank | short-query R@1 |
//! |---|---|---|---|---|
//! | e5-small int8 | 1.00 | 0.70 | 13 | 0.75 |
//! | paraphrase-MiniLM int8 | 0.98 | 0.90 | 32 | 0.69 |
//!
//! (Both rows are the raw model. What actually ships adds the language-bias
//! correction of [`Whitening`], which takes e5 to 0.94 short-query R@1 and 0.80
//! cross-language R@1 without moving in-language at all.)
//!
//! MiniLM wins the cross-language *median* and loses the tail, badly: it is a
//! symmetric paraphrase model, and on a bare keyword query — which is what a
//! search box gets — it collapses onto whatever short filler is in the corpus.
//! Its worst case in the fixture is the query `"Brezeln Bäckerei"` ranking
//! `"Yeah."` and `"Ja genau."` above the sentence about pretzels, at rank 32.
//! e5 is trained for asymmetric query→passage retrieval, which is the shape of
//! this problem, and its worst case is a rank in single digits.
//!
//! int8 rather than the 470 MB fp32 export: doc vectors agree with fp32 at
//! mean cosine 0.996 (min 0.987), 95.7% of 69 probe queries keep the same top
//! hit, and it embeds at 1.7 ms against 5.1 ms on one thread. A 4x smaller
//! download matters here — the model is optional, and the line it comes down
//! is slow.
//!
//! ## The prefixes
//!
//! e5 was trained with `"query: "` on the search side and `"passage: "` on the
//! indexed side, and it is *not* decoration: on the fixture, prefixing
//! correctly takes cross-language R@1 from 0.50 to 0.70. Getting this wrong
//! costs quality silently — nothing errors, the results are just worse — so
//! there is exactly one function that adds a prefix ([`Prefix`]) and both
//! call sites go through it.
//!
//! ## Where the work happens
//!
//! Passage vectors are computed on the inference thread, the same deprioritised
//! worker the ASR and identity legs run on, right after the transcript exists.
//! An idle-priority repair worker fills older/corrected text from the durable
//! queue, sharing the same model and yielding to live analysis and queries.
//! Never on the capture thread. The query vector is computed on the connection
//! thread that asked for it, because it is one 1.7 ms forward pass and making
//! it asynchronous would cost more than it saves.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ort::session::Session;
use ort::value::Tensor;
use rusqlite::{Connection, OptionalExtension, params};
use tokenizers::Tokenizer;
use tracing::debug;

use crate::embed::Embedding;
use crate::store::{SegmentFilter, Store};

/// Dimensionality of the catalogued model. Stored per row anyway — a database
/// must be able to say what is in it without consulting the binary that wrote
/// it — but asserted on load so a swapped model file is an error rather than a
/// bank of vectors that compare wrongly.
pub const DIM: usize = 384;

/// Longest input in tokens. e5-small's position embeddings run to 512; turns
/// are 1-3 s of speech and never come close, so this only bounds the damage
/// from a pathological transcript.
pub const MAX_TOKENS: usize = 256;

/// How many segments one backfill batch embeds before committing. Small enough
/// that a Ctrl-C loses at most this much work, large enough that the
/// transaction overhead disappears.
pub const BACKFILL_BATCH: usize = 128;

/// Reciprocal-rank fusion's damping constant. 60 is the value from the original
/// Cormack/Clarke/Buettcher result and the one every implementation since has
/// used; it is here as a named constant rather than a literal because changing
/// it changes result order and that should be a decision, not a typo.
pub const RRF_K: f64 = 60.0;

// ---------------------------------------------------------------------------
// the model
// ---------------------------------------------------------------------------

/// Which side of the retrieval the text is on. e5 is asymmetric and both
/// spellings must be exactly the ones it was trained with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefix {
    /// What the user typed.
    Query,
    /// What was said, and is being indexed.
    Passage,
}

impl Prefix {
    pub fn apply(self, text: &str) -> String {
        match self {
            Prefix::Query => format!("query: {text}"),
            Prefix::Passage => format!("passage: {text}"),
        }
    }
}

/// The sentence-embedding model, loaded.
pub struct TextEmbedder {
    session: Session,
    tokenizer: Tokenizer,
    model_id: String,
    /// The export declares a `token_type_ids` input; some re-exports do not.
    wants_token_type: bool,
}

impl TextEmbedder {
    /// Load from a resolved [`crate::models::SemanticModel`].
    pub fn load(m: &crate::models::SemanticModel) -> Result<Self> {
        Self::load_at(&m.model, &m.tokenizer, m.model_id())
    }

    pub fn load_at(model: &Path, tokenizer: &Path, model_id: String) -> Result<Self> {
        let ort_err = |what: &'static str| move |e: ort::Error<_>| anyhow::anyhow!("{what}: {e}");
        // One thread, like every other model in this daemon: throughput is not
        // the constraint, never stealing a VR frame is.
        let session = Session::builder()
            .map_err(|e| anyhow::anyhow!("creating ONNX session builder: {e}"))?
            .with_intra_threads(1)
            .map_err(ort_err("configuring intra-op threads"))?
            .with_inter_threads(1)
            .map_err(ort_err("configuring inter-op threads"))?
            .commit_from_file(model)
            .with_context(|| format!("loading the text embedding model {}", model.display()))?;

        let mut tokenizer = Tokenizer::from_file(tokenizer).map_err(|e| {
            anyhow::anyhow!("loading the tokenizer for the text embedding model: {e}")
        })?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: MAX_TOKENS,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("configuring tokenizer truncation: {e}"))?;

        let names: Vec<&str> = session.inputs().iter().map(|i| i.name()).collect();
        let wants_token_type = names.contains(&"token_type_ids");
        if !names.contains(&"input_ids") || !names.contains(&"attention_mask") {
            bail!("unrecognised text embedding export; inputs are {names:?}");
        }

        Ok(Self {
            session,
            tokenizer,
            model_id,
            wants_token_type,
        })
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// One text, prefixed for its side of the retrieval, mean-pooled over the
    /// attention mask and L2-normalised.
    ///
    /// Normalising here rather than at comparison time is what makes the search
    /// scan a dot product: every vector in `segment_vectors` is a unit vector,
    /// so cosine similarity is `a·b` with no square roots in the inner loop.
    pub fn embed(&mut self, text: &str, side: Prefix) -> Result<Embedding> {
        let prefixed = side.apply(text);
        let enc = self
            .tokenizer
            .encode(prefixed.as_str(), true)
            .map_err(|e| anyhow::anyhow!("tokenising for the text embedding model: {e}"))?;

        let ids: Vec<i64> = enc.get_ids().iter().map(|&i| i as i64).collect();
        let mask: Vec<i64> = enc.get_attention_mask().iter().map(|&i| i as i64).collect();
        if ids.is_empty() {
            bail!("the tokenizer produced no tokens for {text:?}");
        }
        let n = ids.len() as i64;

        let input_ids = Tensor::from_array((vec![1i64, n], ids))?;
        let attention = Tensor::from_array((vec![1i64, n], mask.clone()))?;
        let out = if self.wants_token_type {
            let types = Tensor::from_array((vec![1i64, n], vec![0i64; n as usize]))?;
            self.session.run(ort::inputs![
                "input_ids" => input_ids,
                "attention_mask" => attention,
                "token_type_ids" => types,
            ])?
        } else {
            self.session.run(ort::inputs![
                "input_ids" => input_ids,
                "attention_mask" => attention,
            ])?
        };

        let key = out
            .keys()
            .find(|k| *k == "last_hidden_state")
            .map(str::to_string)
            .or_else(|| out.keys().next().map(str::to_string))
            .context("the text embedding model returned no outputs")?;
        let (shape, hidden) = out[key.as_str()].try_extract_tensor::<f32>()?;
        if shape.len() != 3 || shape[0] != 1 {
            bail!("unexpected embedding output shape {shape:?}; expected [1, tokens, dim]");
        }
        let tokens = shape[1] as usize;
        let dim = shape[2] as usize;
        if tokens != mask.len() {
            bail!(
                "the model returned {tokens} token vectors for {} tokens",
                mask.len()
            );
        }

        let mut sum = vec![0f32; dim];
        let mut kept = 0f32;
        for (t, &m) in mask.iter().enumerate() {
            if m == 0 {
                continue;
            }
            kept += 1.0;
            let row = &hidden[t * dim..(t + 1) * dim];
            for (acc, v) in sum.iter_mut().zip(row) {
                *acc += *v;
            }
        }
        if kept == 0.0 {
            bail!("every token was masked out");
        }
        for v in &mut sum {
            *v /= kept;
        }
        normalise(&mut sum);
        Ok(Embedding::new(self.model_id.clone(), sum))
    }

    pub fn embed_query(&mut self, text: &str) -> Result<Embedding> {
        self.embed(text, Prefix::Query)
    }

    pub fn embed_passage(&mut self, text: &str) -> Result<Embedding> {
        self.embed(text, Prefix::Passage)
    }
}

/// Scale a vector to unit length in place. A zero vector is left alone rather
/// than turned into NaNs.
pub fn normalise(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm <= f32::EPSILON {
        return;
    }
    for x in v.iter_mut() {
        *x /= norm;
    }
}

/// Dot product of two equal-length slices. Both sides are unit vectors coming
/// out of [`TextEmbedder::embed`], so this *is* the cosine — see the note there
/// about why the normalisation lives at write time.
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

// ---------------------------------------------------------------------------
// schema v9
// ---------------------------------------------------------------------------

/// Everything schema v9 adds, written so it is a no-op on a v9 database and
/// applies cleanly to a v6, v7 or v8 one.
///
/// It is deliberately standalone: one table, three indexes, no column added to
/// an existing table and no backfill. Nothing here reads anything a v7 or v8
/// migration might have introduced, so the order it lands in relative to them
/// does not matter — which is the only way a schema change can be developed on
/// a branch while another one is being developed on the trunk.
///
/// `seq` is not decoration. The in-memory index ([`VectorIndex`]) has to be
/// able to ask "what has changed since I last looked", and `segment_id` cannot
/// answer that: re-embedding a corrected transcript rewrites a row without
/// moving its id. A monotone counter can, and it is what lets the daemon pick
/// up vectors that `recalld semantic backfill` wrote in a *different process*
/// without reloading the whole matrix.
///
/// `text_hash` is the other half of the same idea, seen from the writer's side:
/// a vector is stale exactly when the text it was computed from has changed, so
/// the backfill's work list is "no row, wrong model, or wrong hash" and a
/// corrected segment re-embeds itself on the next pass with no extra plumbing.
pub fn migrate_v9(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS segment_vectors (
             segment_id INTEGER PRIMARY KEY REFERENCES segments(id),
             seq        INTEGER NOT NULL,
             vector     BLOB    NOT NULL,
             model_id   TEXT    NOT NULL,
             dim        INTEGER NOT NULL,
             text_hash  INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS idx_segment_vectors_seq
             ON segment_vectors(model_id, seq);
         -- The backfill's work list is an anti-join against this table over
         -- live, transcribed segments; without an index on the transcript side
         -- of it every batch is a full scan of `segments`.
         CREATE INDEX IF NOT EXISTS idx_segments_text_present
             ON segments(deleted_at, id) WHERE text IS NOT NULL;",
    )?;
    Ok(())
}

/// Durable mutation tracking and a resumable dirty-text queue (schema v21).
/// All trigger work participates in the statement that changed the transcript.
pub fn migrate_v21(conn: &Connection) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='semantic_state')",
        [],
        |r| r.get(0),
    )?;
    if exists {
        return Ok(());
    }
    conn.execute_batch("SAVEPOINT semantic_v21")?;
    let result = (|| -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE semantic_state (
                id INTEGER PRIMARY KEY CHECK(id=1), seq INTEGER NOT NULL,
                deletions INTEGER NOT NULL, eligible INTEGER NOT NULL DEFAULT 0);
             INSERT INTO semantic_state(id,seq,deletions) SELECT 1, COALESCE(MAX(seq),0), 0 FROM segment_vectors;
             CREATE TABLE semantic_models(model_id TEXT PRIMARY KEY, embedded INTEGER NOT NULL);
             CREATE TABLE semantic_dirty (
                segment_id INTEGER PRIMARY KEY REFERENCES segments(id) ON DELETE CASCADE);
             CREATE INDEX idx_segment_vectors_global_seq ON segment_vectors(seq);
             INSERT INTO semantic_dirty SELECT id FROM segments
                WHERE deleted_at IS NULL AND text IS NOT NULL AND trim(text,char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) <> '';
             DROP TRIGGER IF EXISTS segments_fts_update;
             CREATE TRIGGER segments_fts_update AFTER UPDATE OF text ON segments
                WHEN old.text IS NOT new.text BEGIN
                INSERT INTO segments_fts(segments_fts,rowid,text) VALUES('delete',old.id,old.text);
                INSERT INTO segments_fts(rowid,text) VALUES(new.id,new.text);
             END;
             CREATE TRIGGER semantic_vector_insert AFTER INSERT ON segment_vectors BEGIN
                INSERT INTO semantic_models VALUES(new.model_id,1)
                    ON CONFLICT(model_id) DO UPDATE SET embedded=embedded+1;
                UPDATE semantic_state SET seq=seq+1 WHERE id=1;
                UPDATE segment_vectors SET seq=(SELECT seq FROM semantic_state WHERE id=1)
                    WHERE segment_id=new.segment_id;
                DELETE FROM semantic_dirty WHERE segment_id=new.segment_id;
             END;
             CREATE TRIGGER semantic_vector_update AFTER UPDATE OF vector,model_id,dim,text_hash
                ON segment_vectors BEGIN
                UPDATE semantic_models SET embedded=embedded-1 WHERE model_id=old.model_id;
                INSERT INTO semantic_models VALUES(new.model_id,1)
                    ON CONFLICT(model_id) DO UPDATE SET embedded=embedded+1;
                UPDATE semantic_state SET seq=seq+1, deletions=deletions+
                    (old.model_id IS NOT new.model_id OR old.dim IS NOT new.dim) WHERE id=1;
                UPDATE segment_vectors SET seq=(SELECT seq FROM semantic_state WHERE id=1)
                    WHERE segment_id=new.segment_id;
                DELETE FROM semantic_dirty WHERE segment_id=new.segment_id;
             END;
             CREATE TRIGGER semantic_vector_delete AFTER DELETE ON segment_vectors BEGIN
                UPDATE semantic_models SET embedded=embedded-1 WHERE model_id=old.model_id;
                UPDATE semantic_state SET seq=seq+1,deletions=deletions+1 WHERE id=1;
                INSERT OR IGNORE INTO semantic_dirty SELECT id FROM segments
                    WHERE id=old.segment_id AND deleted_at IS NULL AND trim(text,char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) <> '';
             END;
             CREATE TRIGGER semantic_text_insert AFTER INSERT ON segments BEGIN
                UPDATE semantic_state SET seq=seq+1,
                    eligible=eligible+coalesce(new.deleted_at IS NULL AND trim(new.text,char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))<>'',0) WHERE id=1;
                INSERT OR IGNORE INTO semantic_dirty SELECT new.id
                    WHERE new.deleted_at IS NULL AND trim(new.text,char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) <> '';
             END;
             CREATE TRIGGER semantic_text_update AFTER UPDATE OF text,deleted_at ON segments
                WHEN old.text IS NOT new.text OR old.deleted_at IS NOT new.deleted_at BEGIN
                UPDATE semantic_state SET seq=seq+1,
                    eligible=eligible+coalesce(new.deleted_at IS NULL AND trim(new.text,char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))<>'',0)
                      -coalesce(old.deleted_at IS NULL AND trim(old.text,char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))<>'',0) WHERE id=1;
                DELETE FROM segment_vectors WHERE segment_id=new.id;
                DELETE FROM semantic_dirty WHERE segment_id=new.id;
                INSERT OR IGNORE INTO semantic_dirty SELECT new.id
                    WHERE new.deleted_at IS NULL AND trim(new.text,char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) <> '';
             END;
             CREATE TRIGGER semantic_text_delete BEFORE DELETE ON segments BEGIN
                DELETE FROM segment_vectors WHERE segment_id=old.id;
                DELETE FROM semantic_dirty WHERE segment_id=old.id;
                UPDATE semantic_state SET seq=seq+1,
                    eligible=eligible-coalesce(old.deleted_at IS NULL AND trim(old.text,char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))<>'',0) WHERE id=1;
             END;"
        )?;
        // One migration-only hash reconciliation. Future edits invalidate the
        // vector transactionally, so no background batch rehashes the archive.
        let mut stmt = conn.prepare(
            "SELECT v.segment_id,g.text,v.text_hash FROM segment_vectors v
            JOIN segments g ON g.id=v.segment_id WHERE g.deleted_at IS NULL",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for (id, text, hash) in rows {
            if text
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty() && text_hash(t) == hash)
            {
                conn.execute(
                    "DELETE FROM semantic_dirty WHERE segment_id=?1",
                    params![id],
                )?;
            } else {
                conn.execute(
                    "DELETE FROM segment_vectors WHERE segment_id=?1",
                    params![id],
                )?;
            }
        }
        conn.execute_batch("DELETE FROM segment_vectors WHERE segment_id NOT IN
                (SELECT id FROM segments WHERE deleted_at IS NULL AND trim(text,char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))<>'');
            DELETE FROM semantic_models;
            INSERT INTO semantic_models SELECT model_id,COUNT(*) FROM segment_vectors GROUP BY model_id;
            UPDATE semantic_state SET eligible=(SELECT COUNT(*) FROM segments
                WHERE deleted_at IS NULL AND trim(text,char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288))<>'') WHERE id=1;")?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("RELEASE semantic_v21")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK TO semantic_v21; RELEASE semantic_v21");
            Err(e)
        }
    }
}

fn mutation_state(conn: &Connection) -> Result<(i64, i64)> {
    Ok(conn.query_row(
        "SELECT seq,deletions FROM semantic_state WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}

/// Stable, cheap, and deliberately not cryptographic: this only has to notice
/// that a transcript changed, and it is stored as an i64 in SQLite.
///
/// FNV-1a, spelled out rather than pulled in, because the one property that
/// matters is that it never changes — a different hash function in a later
/// version would invalidate every vector in the database at once.
pub fn text_hash(text: &str) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h as i64
}

/// Write (or replace) one segment's vector.
pub fn put_vector(conn: &Connection, segment_id: i64, v: &Embedding, hash: i64) -> Result<i64> {
    write_vector(conn, segment_id, v, hash, None)?.context("vector was not written")
}

fn write_vector(
    conn: &Connection,
    segment_id: i64,
    v: &Embedding,
    hash: i64,
    expected_text: Option<&str>,
) -> Result<Option<i64>> {
    // A trigger allocates the durable sequence in this same atomic statement.
    // The optional text predicate rejects an inference result whose words
    // were corrected/deleted by another process while the model was running.
    let changed = conn.execute(
        "INSERT INTO segment_vectors (segment_id,seq,vector,model_id,dim,text_hash)
         SELECT ?1,0,?2,?3,?4,?5 WHERE ?6 IS NULL OR EXISTS(
            SELECT 1 FROM segments WHERE id=?1 AND deleted_at IS NULL AND text=?6)
         ON CONFLICT(segment_id) DO UPDATE SET vector=excluded.vector,
            model_id=excluded.model_id,dim=excluded.dim,text_hash=excluded.text_hash",
        params![
            segment_id,
            v.to_blob(),
            v.model_id,
            v.dim() as i64,
            hash,
            expected_text
        ],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    Ok(Some(conn.query_row(
        "SELECT seq FROM segment_vectors WHERE segment_id=?1",
        params![segment_id],
        |r| r.get(0),
    )?))
}

/// [`put_vector`] against a store rather than a raw connection. The backfill
/// takes the connection because it owns a transaction; everything else — and
/// every test outside this crate — has a `Store`.
pub fn put_segment_vector(store: &Store, segment_id: i64, v: &Embedding, hash: i64) -> Result<i64> {
    put_vector(store.conn(), segment_id, v, hash)
}

/// How much of the transcript this model has actually indexed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Live segments that have a transcript at all — the denominator.
    pub eligible: i64,
    /// Of those, the ones with a current vector under this model.
    pub embedded: i64,
}

impl Coverage {
    pub fn pending(&self) -> i64 {
        (self.eligible - self.embedded).max(0)
    }
    pub fn complete(&self) -> bool {
        self.pending() == 0
    }
}

pub fn coverage(store: &Store, model_id: &str) -> Result<Coverage> {
    let eligible =
        store
            .conn()
            .query_row("SELECT eligible FROM semantic_state WHERE id=1", [], |r| {
                r.get(0)
            })?;
    let embedded = store
        .conn()
        .query_row(
            "SELECT embedded FROM semantic_models WHERE model_id=?1",
            params![model_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0);
    Ok(Coverage { eligible, embedded })
}

/// The next batch of segments needing a vector, oldest first.
///
/// The indexed dirty queue tracks new/corrected text; the model index adds
/// vectors written in a different embedding space. No batch hashes existing
/// transcripts. Both lists survive restart and successful writes dequeue their
/// row in the same transaction as the vector.
pub fn pending_segments(store: &Store, model_id: &str, limit: usize) -> Result<Vec<(i64, String)>> {
    pending_after(store, model_id, limit, 0)
}

fn pending_after(
    store: &Store,
    model_id: &str,
    limit: usize,
    after: i64,
) -> Result<Vec<(i64, String)>> {
    let mut stmt = store.conn().prepare(
        "SELECT g.id,g.text FROM segments g JOIN (
            SELECT segment_id FROM (
                SELECT segment_id FROM semantic_dirty WHERE segment_id > ?3
                ORDER BY segment_id LIMIT ?2)
            UNION SELECT segment_id FROM (
                SELECT segment_id FROM segment_vectors
                WHERE segment_id > ?3 AND model_id<>?1
                  AND EXISTS(SELECT 1 FROM semantic_models WHERE model_id<>?1 AND embedded>0)
                ORDER BY segment_id LIMIT ?2)
         ) pending ON pending.segment_id=g.id
         WHERE g.deleted_at IS NULL AND g.text IS NOT NULL AND trim(g.text,char(9,10,11,12,13,32,133,160,5760,8192,8193,8194,8195,8196,8197,8198,8199,8200,8201,8202,8232,8233,8239,8287,12288)) <> ''
         ORDER BY g.id LIMIT ?2")?;
    Ok(stmt
        .query_map(params![model_id, limit as i64, after], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<rusqlite::Result<_>>()?)
}

// ---------------------------------------------------------------------------
// the index
// ---------------------------------------------------------------------------

/// Every vector for one model, resident, as one flat `f32` matrix.
///
/// Measured rather than assumed (`semantic::tests::brute_force_at_scale`): at
/// 100k segments the matrix is 147 MB and a full scan with a top-k heap is
/// tens of milliseconds, which is what makes the brute force honest. Reading
/// the same 147 MB back out of SQLite per query is an order of magnitude
/// slower, so the matrix is loaded once and kept.
///
/// `seq` and the deletion generation (see [`migrate_v21`]) survive writes
/// from other processes and count-neutral replacements. Deletion changes
/// invalidate the resident matrix independently of its row count. Anything unexpected reloads from scratch; being slow
/// once is always better than answering from a stale matrix.
#[derive(Clone)]
pub struct VectorIndex {
    model_id: String,
    dim: usize,
    ids: Vec<i64>,
    at: HashMap<i64, usize>,
    /// Row-major, `ids.len() * dim`. Rows are *whitened* once the index is big
    /// enough for [`Whitening`] to be estimated, raw before that; the database
    /// always holds the raw vector.
    data: Vec<f32>,
    seq: i64,
    deletions: i64,
    whitening: Option<Whitening>,
    /// How many rows were resident when the whitening was last estimated. 0
    /// means never.
    fitted_at: usize,
}

/// The language-bias correction: subtract the corpus mean, then project out the
/// top few principal directions of what is left.
///
/// This is "all but the top" (Mu & Viswanath, ICLR 2018) and it is here for a
/// specific, measured failure. e5 puts a large shared component into every
/// vector, and in a corpus that is half German and half English a big part of
/// that component is *which language this is*. The symptom on the fixture: the
/// query `"Zahnschmerzen, ich sollte mal zum Arzt"` scores an unrelated German
/// sentence about a humming fridge (0.841) above the English sentence about
/// needing a dentist (0.833) — the two turns that mean the same thing lose to
/// the two that are merely in the same language, which defeats the entire
/// point of a multilingual index.
///
/// Removing two directions on the 48-segment de/en fixture:
///
/// | | in-language R@1 | short-query R@1 | cross-language R@1 | mean cross rank |
/// |---|---|---|---|---|
/// | raw | 1.00 | 0.75 | 0.70 | 3.3 |
/// | centred only | 1.00 | 0.75 | 0.50 | 5.6 |
/// | centred, −1 component | 1.00 | 0.75 | 0.80 | 2.3 |
/// | **centred, −2 components** | **1.00** | **0.94** | **0.80** | **2.7** |
///
/// Centring *alone* makes cross-language retrieval worse, which is why the
/// component removal is not optional decoration on top of it — the two are one
/// transform. Two components is also what the literature suggests for this
/// dimensionality (~d/100 rounded down hard), so it is not a number fitted to
/// this fixture.
///
/// It lives in the index and never in the database: stored vectors stay raw, so
/// changing or removing this costs a reload rather than a re-embedding, and the
/// `model_id` contract is untouched.
#[derive(Debug, Clone)]
pub struct Whitening {
    mean: Vec<f32>,
    /// Orthonormal, `WHITENING_COMPONENTS` of them.
    comps: Vec<Vec<f32>>,
}

/// Rows the index needs before the correction is estimated at all. Below this
/// the top principal direction of the corpus is noise rather than language, and
/// projecting it out would remove signal.
pub const MIN_WHITENING_ROWS: usize = 256;

/// Directions removed. See [`Whitening`] for the measurement.
pub const WHITENING_COMPONENTS: usize = 2;

/// At most this many rows are sampled to estimate the correction, evenly spaced
/// through the index. A refit is rare but it happens under the search lock, and
/// the estimate does not get meaningfully better past a few thousand rows.
const WHITENING_SAMPLE: usize = 8192;

/// Power-iteration steps per component. Fixed rather than
/// converged-to-tolerance so that two daemons with the same index compute the
/// same correction.
const POWER_ITERS: usize = 24;

impl Whitening {
    /// Estimate from a row-major matrix. `None` when there is not enough to
    /// estimate from, which the caller treats as "carry on with raw vectors".
    pub fn fit(data: &[f32], dim: usize, k: usize) -> Option<Self> {
        let n = data.len() / dim.max(1);
        if dim == 0 || k == 0 || n < MIN_WHITENING_ROWS {
            return None;
        }
        let stride = n.div_ceil(WHITENING_SAMPLE).max(1);
        let sample: Vec<&[f32]> = (0..n)
            .step_by(stride)
            .map(|r| &data[r * dim..(r + 1) * dim])
            .collect();

        let mut mean = vec![0f32; dim];
        for row in &sample {
            for (m, v) in mean.iter_mut().zip(*row) {
                *m += *v;
            }
        }
        for m in &mut mean {
            *m /= sample.len() as f32;
        }
        let centred: Vec<Vec<f32>> = sample
            .iter()
            .map(|row| row.iter().zip(&mean).map(|(v, m)| v - m).collect())
            .collect();

        let mut comps: Vec<Vec<f32>> = Vec::with_capacity(k);
        for c in 0..k {
            // Deterministic start: a fixed, non-degenerate direction. Random
            // initialisation would make the same index whiten differently on
            // two runs, and search results must not depend on that.
            let mut v: Vec<f32> = (0..dim)
                .map(|i| (((i * 2_654_435_761 + c * 40_503) % 1_000) as f32 / 500.0) - 1.0)
                .collect();
            orthogonalise(&mut v, &comps);
            normalise(&mut v);
            for _ in 0..POWER_ITERS {
                // next = X^T X v, without ever forming the d x d covariance.
                let mut next = vec![0f32; dim];
                for row in &centred {
                    let p = dot(row, &v);
                    for (acc, x) in next.iter_mut().zip(row) {
                        *acc += p * x;
                    }
                }
                orthogonalise(&mut next, &comps);
                let norm = next.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm <= f32::EPSILON {
                    break;
                }
                for x in &mut next {
                    *x /= norm;
                }
                v = next;
            }
            // A direction that carries no variance is not a direction.
            if centred.iter().map(|r| dot(r, &v).abs()).sum::<f32>() <= f32::EPSILON {
                break;
            }
            comps.push(v);
        }
        if comps.is_empty() {
            return None;
        }
        Some(Self { mean, comps })
    }

    /// Centre, project out, renormalise. The result is a unit vector, so the
    /// scan stays a dot product.
    pub fn apply(&self, v: &[f32]) -> Vec<f32> {
        let mut out: Vec<f32> = v.iter().zip(&self.mean).map(|(x, m)| x - m).collect();
        orthogonalise(&mut out, &self.comps);
        normalise(&mut out);
        out
    }
}

/// Remove the components of `v` along each (orthonormal) direction in `basis`.
fn orthogonalise(v: &mut [f32], basis: &[Vec<f32>]) {
    for c in basis {
        let p = dot(v, c);
        for (x, b) in v.iter_mut().zip(c) {
            *x -= p * b;
        }
    }
}

/// Database reads are copied under the store lock; transforms and ranking
/// happen afterward against an immutable snapshot.
struct IndexUpdate {
    seq: i64,
    deletions: i64,
    reset: bool,
    rows: Vec<(i64, Vec<u8>, i64)>,
}

impl IndexUpdate {
    fn read(index: &VectorIndex, conn: &Connection) -> Result<Self> {
        let (seq, deletions) = mutation_state(conn)?;
        let mut reset = deletions != index.deletions || seq < index.seq;
        if seq == index.seq && !reset {
            return Ok(Self {
                seq,
                deletions,
                reset,
                rows: Vec::new(),
            });
        }
        // Re-fitting must start with raw vectors, never a second whitening of
        // an already transformed matrix. Only growth needs this count.
        if index.whitening.is_some() && !reset {
            let total: i64 = conn.query_row(
                "SELECT COUNT(*) FROM segment_vectors WHERE model_id=?1 AND dim=?2",
                params![index.model_id, index.dim as i64],
                |r| r.get(0),
            )?;
            reset = total as usize >= index.fitted_at + index.fitted_at / 2;
        }
        let mut stmt = conn.prepare(
            "SELECT segment_id,vector,seq FROM segment_vectors
            WHERE model_id=?1 AND dim=?2 AND seq>?3 ORDER BY seq,segment_id",
        )?;
        let rows = stmt
            .query_map(
                params![
                    index.model_id,
                    index.dim as i64,
                    if reset { 0 } else { index.seq }
                ],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?
            .collect::<rusqlite::Result<_>>()?;
        Ok(Self {
            seq,
            deletions,
            reset,
            rows,
        })
    }
}

pub struct SearchSnapshot {
    base: Arc<VectorIndex>,
    update: IndexUpdate,
}

impl SearchSnapshot {
    pub fn sequence(&self) -> i64 {
        self.update.seq
    }
}

/// A correction or deletion during inference must not return a current text
/// ranked by a superseded vector. Runs only for the bounded scored hit list.
pub fn vector_current(store: &Store, id: i64, model_id: &str, sequence: i64) -> Result<bool> {
    Ok(store.conn().query_row(
        "SELECT EXISTS(SELECT 1 FROM segment_vectors v
        JOIN segments g ON g.id=v.segment_id WHERE v.segment_id=?1
        AND v.model_id=?2 AND v.seq<=?3 AND g.deleted_at IS NULL)",
        params![id, model_id, sequence],
        |r| r.get(0),
    )?)
}

/// One scored row from the vector scan.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scored {
    pub segment_id: i64,
    pub score: f32,
}

impl VectorIndex {
    pub fn empty(model_id: impl Into<String>, dim: usize) -> Self {
        Self {
            model_id: model_id.into(),
            dim,
            ids: Vec::new(),
            at: HashMap::new(),
            data: Vec::new(),
            seq: 0,
            deletions: 0,
            whitening: None,
            fitted_at: 0,
        }
    }

    /// Whether the language-bias correction is currently in effect. Reported by
    /// `status`, because it changes what the scores mean.
    pub fn whitened(&self) -> bool {
        self.whitening.is_some()
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// Bytes of resident vector data. `status` reports it, because 147 MB is
    /// the kind of number a user is entitled to see before it appears in their
    /// process.
    pub fn bytes(&self) -> usize {
        self.data.len() * 4
    }

    fn insert(&mut self, id: i64, vector: &[f32]) {
        match self.at.get(&id) {
            Some(&row) => self.data[row * self.dim..(row + 1) * self.dim].copy_from_slice(vector),
            None => {
                let row = self.ids.len();
                self.ids.push(id);
                self.at.insert(id, row);
                self.data.extend_from_slice(vector);
            }
        }
    }

    fn clear(&mut self) {
        self.ids.clear();
        self.at.clear();
        self.data.clear();
        self.seq = 0;
        self.whitening = None;
        self.fitted_at = 0;
    }

    /// A vector as it should sit in the matrix: whitened if there is a
    /// whitening, raw if there is not.
    fn prepared(&self, v: &[f32]) -> Vec<f32> {
        match &self.whitening {
            Some(w) => w.apply(v),
            None => v.to_vec(),
        }
    }

    /// Put a query vector into the same space the matrix is in. Public because
    /// the caller embeds the query and the index owns the transform, and the
    /// two must not be able to disagree.
    pub fn prepare_query(&self, v: &[f32]) -> Vec<f32> {
        self.prepared(v)
    }

    /// Bring the matrix up to date with the database, cheaply when it can be.
    pub fn refresh(&mut self, conn: &Connection) -> Result<()> {
        let update = IndexUpdate::read(self, conn)?;
        self.apply_update(update)?;
        Ok(())
    }

    fn apply_update(&mut self, update: IndexUpdate) -> Result<()> {
        if update.reset {
            self.clear();
        }
        for (id, blob, _) in update.rows {
            let v = Embedding::from_blob(self.model_id.clone(), &blob)?;
            if v.dim() != self.dim {
                continue;
            }
            self.insert(id, &self.prepared(&v.vector));
        }
        self.seq = update.seq;
        self.deletions = update.deletions;
        if self.should_refit() {
            self.fit_whitening();
        }
        Ok(())
    }

    /// Whether the whitening is missing or stale. Growth by half is the
    /// trigger: often enough that a young index converges quickly, rare enough
    /// that a mature one refits a handful of times in its life.
    fn should_refit(&self) -> bool {
        self.ids.len() >= MIN_WHITENING_ROWS
            && (self.fitted_at == 0 || self.ids.len() >= self.fitted_at + self.fitted_at / 2)
    }

    /// Estimate the whitening from what is resident, then apply it in place.
    fn fit_whitening(&mut self) {
        self.fitted_at = self.ids.len();
        let Some(w) = Whitening::fit(&self.data, self.dim, WHITENING_COMPONENTS) else {
            return;
        };
        for row in 0..self.ids.len() {
            let at = row * self.dim;
            let out = w.apply(&self.data[at..at + self.dim]);
            self.data[at..at + self.dim].copy_from_slice(&out);
        }
        debug!(
            rows = self.ids.len(),
            components = WHITENING_COMPONENTS,
            "re-estimated the semantic index's language-bias correction"
        );
        self.whitening = Some(w);
    }

    /// Fold one freshly written vector in without touching the database.
    pub fn note(&mut self, segment_id: i64, v: &Embedding, _seq: i64) {
        if v.model_id != self.model_id || v.dim() != self.dim {
            return;
        }
        let prepared = self.prepared(&v.vector);
        self.insert(segment_id, &prepared);
        // Only apply_update may advance the database scan watermark. A live
        // write can arrive before the first refresh after restart, or after
        // an external backfill wrote other rows. Skipping straight to this
        // write's sequence would permanently hide every intervening vector.
    }

    /// Top `limit` rows by cosine, among the segments `within` admits.
    ///
    /// The facets are applied *inside* the scan, not to its output: filtering
    /// a top-50 afterwards silently returns four hits whenever the speaker
    /// facet is narrow, which looks exactly like "she never said that".
    pub fn search(&self, query: &[f32], limit: usize, within: &Candidates) -> Vec<Scored> {
        if limit == 0 || query.len() != self.dim {
            return Vec::new();
        }
        let mut best: Vec<Scored> = Vec::with_capacity(limit + 1);
        for (row, &id) in self.ids.iter().enumerate() {
            if !within.admits(id) {
                continue;
            }
            let score = dot(query, &self.data[row * self.dim..(row + 1) * self.dim]);
            if best.len() == limit
                && let Some(last) = best.last()
                && score <= last.score
            {
                continue;
            }
            // Insertion sort into a list of at most `limit`: for the limits a
            // search box uses (tens) this beats a binary heap and, unlike one,
            // it is stable on ties — equal scores keep segment-id order, so the
            // same query twice is the same answer twice.
            let at = best.partition_point(|s| s.score >= score);
            best.insert(
                at,
                Scored {
                    segment_id: id,
                    score,
                },
            );
            best.truncate(limit);
        }
        best
    }
}

// ---------------------------------------------------------------------------
// fusion
// ---------------------------------------------------------------------------

/// Which leg found a hit. Carried all the way to the GUI, because "the words
/// are in there" and "this seemed to mean the same thing" are different claims
/// and a user deciding whether to trust a result needs to know which they are
/// looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Keyword,
    Semantic,
    Both,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Keyword => "keyword",
            Via::Semantic => "semantic",
            Via::Both => "both",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fused {
    pub segment_id: i64,
    pub via: Via,
    pub score: f64,
    /// 1-based rank in the keyword list, when it was in it.
    pub keyword_rank: Option<usize>,
    pub semantic_rank: Option<usize>,
}

/// Reciprocal-rank fusion of two ranked id lists.
///
/// RRF rather than a weighted sum of scores, because there is no exchange rate
/// between BM25 and cosine: FTS5's rank is a negative log-likelihood-ish number
/// whose scale depends on the corpus, and e5's cosines all sit between about
/// 0.75 and 0.90. Any constant that mapped one onto the other would be a
/// number nobody could justify. Ranks have no units, so fusing them needs no
/// exchange rate at all.
///
/// Order is fully determined: score descending, then the better of the two
/// ranks, then segment id. Two searches for the same words return the same list
/// in the same order, which matters more here than half a point of quality —
/// a memory tool whose results reshuffle is a memory tool nobody trusts.
pub fn fuse(keyword: &[i64], semantic: &[i64], k: f64) -> Vec<Fused> {
    let mut out: Vec<Fused> = Vec::new();
    let mut at: HashMap<i64, usize> = HashMap::new();

    for (rank, id) in keyword.iter().enumerate() {
        at.entry(*id).or_insert_with(|| {
            out.push(Fused {
                segment_id: *id,
                via: Via::Keyword,
                score: 0.0,
                keyword_rank: None,
                semantic_rank: None,
            });
            out.len() - 1
        });
        let e = &mut out[at[id]];
        if e.keyword_rank.is_none() {
            e.keyword_rank = Some(rank + 1);
            e.score += 1.0 / (k + (rank + 1) as f64);
        }
    }
    for (rank, id) in semantic.iter().enumerate() {
        at.entry(*id).or_insert_with(|| {
            out.push(Fused {
                segment_id: *id,
                via: Via::Semantic,
                score: 0.0,
                keyword_rank: None,
                semantic_rank: None,
            });
            out.len() - 1
        });
        let e = &mut out[at[id]];
        if e.semantic_rank.is_none() {
            e.semantic_rank = Some(rank + 1);
            e.score += 1.0 / (k + (rank + 1) as f64);
            e.via = if e.keyword_rank.is_some() {
                Via::Both
            } else {
                Via::Semantic
            };
        }
    }

    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.best_rank().cmp(&b.best_rank()))
            .then_with(|| a.segment_id.cmp(&b.segment_id))
    });
    out
}

impl Fused {
    fn best_rank(&self) -> usize {
        match (self.keyword_rank, self.semantic_rank) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => usize::MAX,
        }
    }
}

// ---------------------------------------------------------------------------
// the leg, wired to a store
// ---------------------------------------------------------------------------

// Archive repair shares the model, but never waits in line ahead of live work.
struct Foreground<'a>(&'a AtomicUsize);
impl<'a> Foreground<'a> {
    fn new(counter: &'a AtomicUsize) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(counter)
    }
}
impl Drop for Foreground<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct RepairStatus {
    state: &'static str,
    pending: Option<i64>,
    sampled_at: Option<Instant>,
    completed: u64,
    failed_attempts: u64,
    discarded: u64,
    retry_at: Option<Instant>,
}
impl Default for RepairStatus {
    fn default() -> Self {
        Self {
            state: "not_started",
            pending: None,
            sampled_at: None,
            completed: 0,
            failed_attempts: 0,
            discarded: 0,
            retry_at: None,
        }
    }
}

/// Interruptible shutdown, including an idle worker's polling wait. An already
/// running model call completes, but its result is not written after stop.
#[derive(Default)]
pub struct RepairStop {
    stopped: AtomicBool,
    lock: Mutex<()>,
    wake: Condvar,
}
impl RepairStop {
    pub fn stop(&self) {
        let _lock = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_all();
    }
    fn wait(&self, duration: Duration) {
        let lock = self.lock.lock().unwrap_or_else(|p| p.into_inner());
        let _ = self
            .wake
            .wait_timeout_while(lock, duration, |_| !self.stopped.load(Ordering::SeqCst));
    }
}

struct Retry {
    hash: i64,
    attempts: u32,
    due: Instant,
}
#[derive(Default)]
struct RepairSchedule {
    cursor: i64,
    retries: HashMap<i64, Retry>,
}

impl RepairSchedule {
    /// One bounded database batch and at most one forward pass. The closure
    /// makes scheduling/races testable without downloading a model. None means
    /// foreground inference won the model lock; the row stays durable/pending.
    fn step(
        &mut self,
        store: &Mutex<Store>,
        model: &str,
        status: &Mutex<RepairStatus>,
        now: Instant,
        gate: impl Fn() -> Option<&'static str>,
        embed: impl FnOnce(&str) -> Result<Option<Embedding>>,
    ) -> Result<Duration> {
        if let Some(reason) = gate() {
            let mut status = status.lock().unwrap_or_else(|p| p.into_inner());
            status.state = reason;
            status.retry_at = None;
            return Ok(Duration::from_millis(100));
        }
        let rows = {
            let store = match store.try_lock() {
                Ok(store) => store,
                Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    status.lock().unwrap_or_else(|p| p.into_inner()).state = "waiting_store";
                    return Ok(Duration::from_millis(100));
                }
            };
            let pending = coverage(&store, model)?.pending();
            let mut status = status.lock().unwrap_or_else(|p| p.into_inner());
            status.pending = Some(pending);
            status.sampled_at = Some(Instant::now());
            status.retry_at = None;
            if let Some(reason) = gate() {
                status.state = reason;
                return Ok(Duration::from_millis(100));
            }
            if pending == 0 {
                self.cursor = 0;
                self.retries.clear();
                status.state = "idle";
                return Ok(Duration::from_secs(1));
            }
            let mut rows = pending_after(&store, model, 32, self.cursor)?;
            if rows.is_empty() && self.cursor != 0 {
                self.cursor = 0;
                rows = pending_after(&store, model, 32, 0)?;
            }
            rows
        };
        let mut next_retry = None;
        let mut candidate = None;
        for (id, text) in rows {
            self.cursor = id;
            if let Some(retry) = self.retries.get(&id) {
                if retry.hash == text_hash(&text) && retry.due > now {
                    next_retry =
                        Some(next_retry.map_or(retry.due, |at: Instant| at.min(retry.due)));
                    continue;
                }
                if retry.hash != text_hash(&text) {
                    self.retries.remove(&id);
                }
            }
            candidate = Some((id, text));
            break;
        }
        let Some((id, text)) = candidate else {
            let mut status = status.lock().unwrap_or_else(|p| p.into_inner());
            status.state = "backoff";
            status.retry_at = next_retry;
            // Keep scanning larger queues fairly; no unbounded retry map or
            // sleep that would prevent newly corrected text being discovered.
            return Ok(Duration::from_millis(250));
        };
        if let Some(reason) = gate() {
            status.lock().unwrap_or_else(|p| p.into_inner()).state = reason;
            return Ok(Duration::from_millis(100));
        }
        status.lock().unwrap_or_else(|p| p.into_inner()).state = "running";
        let vector = match embed(&text) {
            Ok(Some(vector)) => vector,
            Ok(None) => {
                status.lock().unwrap_or_else(|p| p.into_inner()).state = "waiting_foreground";
                return Ok(Duration::from_millis(100));
            }
            Err(_) => {
                let attempts = self
                    .retries
                    .get(&id)
                    .map_or(1, |r| r.attempts.saturating_add(1));
                let delay =
                    Duration::from_secs((1u64 << attempts.saturating_sub(1).min(6)).min(60));
                // Retain bounded retry metadata, never transcript text. Cursor
                // fairness still holds if an old failure is evicted.
                if self.retries.len() >= 256 && !self.retries.contains_key(&id) {
                    if let Some(oldest) = self
                        .retries
                        .iter()
                        .min_by_key(|(_, r)| r.due)
                        .map(|(&id, _)| id)
                    {
                        self.retries.remove(&oldest);
                    }
                }
                self.retries.insert(
                    id,
                    Retry {
                        hash: text_hash(&text),
                        attempts,
                        due: now + delay,
                    },
                );
                let mut status = status.lock().unwrap_or_else(|p| p.into_inner());
                status.failed_attempts += 1;
                status.state = "backoff";
                status.retry_at = Some(now + delay);
                return Ok(Duration::from_millis(100));
            }
        };
        let store = store.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(reason) = gate() {
            let mut status = status.lock().unwrap_or_else(|p| p.into_inner());
            status.state = reason;
            status.discarded += 1;
            return Ok(Duration::from_millis(100));
        }
        let wrote =
            write_vector(store.conn(), id, &vector, text_hash(&text), Some(&text))?.is_some();
        self.retries.remove(&id);
        let mut status = status.lock().unwrap_or_else(|p| p.into_inner());
        if wrote {
            status.completed += 1;
        } else {
            status.discarded += 1;
        }
        status.pending = Some(coverage(&store, model)?.pending());
        status.sampled_at = Some(Instant::now());
        status.state = if status.pending == Some(0) {
            "idle"
        } else {
            "running"
        };
        Ok(Duration::from_millis(25))
    }
}

/// Repair old, edited, or differently-modelled text automatically. The durable
/// dirty queue is authoritative; stopping/restarting never loses owed work.
/// Model inference runs outside the store mutex and only at idle CPU priority.
pub fn run_repair(
    leg: Arc<SemanticLeg>,
    store: Arc<Mutex<Store>>,
    control: Arc<crate::control::Control>,
    stop: Arc<RepairStop>,
) {
    crate::pipeline::background_current_thread(19, &[]);
    let mut schedule = RepairSchedule::default();
    let gate = || {
        if stop.stopped.load(Ordering::SeqCst) {
            Some("stopped")
        } else if control.is_paused() {
            Some("paused")
        } else if control
            .queue
            .as_ref()
            .is_some_and(|q| q.queued_samples() > 0)
        {
            Some("waiting_capture")
        } else if leg.foreground.load(Ordering::SeqCst) > 0 {
            Some("waiting_foreground")
        } else {
            None
        }
    };
    while !stop.stopped.load(Ordering::SeqCst) {
        let delay = schedule
            .step(
                &store,
                &leg.model_id,
                &leg.repair,
                Instant::now(),
                gate,
                |text| {
                    let Ok(mut embedder) = leg.embedder.try_lock() else {
                        return Ok(None);
                    };
                    if gate().is_some() {
                        return Ok(None);
                    }
                    embedder.embed_passage(text).map(Some)
                },
            )
            .unwrap_or_else(|_| {
                let mut status = leg.repair.lock().unwrap_or_else(|p| p.into_inner());
                status.failed_attempts += 1;
                status.state = "backoff";
                status.retry_at = Some(Instant::now() + Duration::from_secs(1));
                Duration::from_secs(1)
            });
        stop.wait(delay);
    }
    let mut status = leg.repair.lock().unwrap_or_else(|p| p.into_inner());
    status.state = "stopped";
    status.retry_at = None;
}

/// The whole semantic leg, as the daemon holds it: one shared model and one
/// immutable resident index snapshot.
///
/// Model inference and cached index state have separate locks. Search callers
/// snapshot DB changes first, then release the store before inference/refits.
pub struct SemanticLeg {
    model_id: String,
    embedder: Mutex<TextEmbedder>,
    index: Mutex<Arc<VectorIndex>>,
    coverage: Mutex<Option<(i64, Coverage)>>,
    foreground: AtomicUsize,
    repair: Mutex<RepairStatus>,
}

impl SemanticLeg {
    pub fn new(embedder: TextEmbedder) -> Self {
        let model_id = embedder.model_id().to_string();
        Self {
            index: Mutex::new(Arc::new(VectorIndex::empty(model_id.clone(), DIM))),
            model_id,
            embedder: Mutex::new(embedder),
            coverage: Mutex::new(None),
            foreground: AtomicUsize::new(0),
            repair: Mutex::new(RepairStatus::default()),
        }
    }

    pub(crate) fn live_priority(&self) -> impl Drop + '_ {
        Foreground::new(&self.foreground)
    }

    pub fn model_id(&self) -> String {
        self.model_id.clone()
    }

    pub fn snapshot(&self, store: &Store) -> Result<SearchSnapshot> {
        let base = Arc::clone(&self.index.lock().unwrap_or_else(|p| p.into_inner()));
        let update = IndexUpdate::read(&base, store.conn())?;
        Ok(SearchSnapshot { base, update })
    }

    pub fn search_snapshot(
        &self,
        snapshot: SearchSnapshot,
        q: &str,
        limit: usize,
        within: &Candidates,
    ) -> Result<Vec<Scored>> {
        let _priority = Foreground::new(&self.foreground);
        let base = self.prepare_index(snapshot)?;
        let qv = self
            .embedder
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .embed_query(q)?;
        Ok(base.search(&base.prepare_query(&qv.vector), limit, within))
    }

    /// Explicit warm-up for offline evaluation; status never calls this.
    pub fn warm_index(&self, store: &Store) -> Result<()> {
        self.prepare_index(self.snapshot(store)?)?;
        Ok(())
    }

    fn prepare_index(&self, snapshot: SearchSnapshot) -> Result<Arc<VectorIndex>> {
        let SearchSnapshot { mut base, update } = snapshot;
        if update.reset || !update.rows.is_empty() {
            Arc::make_mut(&mut base).apply_update(update)?;
            let mut cached = self.index.lock().unwrap_or_else(|p| p.into_inner());
            if base.seq >= cached.seq {
                *cached = Arc::clone(&base);
            }
        }
        Ok(base)
    }

    /// Embed one segment's transcript and store the vector. Returns `false`
    /// when the row has nothing to embed, which is not an error — a turn with
    /// no words is a real thing.
    pub fn embed_segment(&self, store: &Store, segment_id: i64) -> Result<bool> {
        let _priority = Foreground::new(&self.foreground);
        let text: Option<String> = store
            .conn()
            .query_row(
                "SELECT text FROM segments WHERE id = ?1 AND deleted_at IS NULL",
                params![segment_id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
            return Ok(false);
        };
        let v = self
            .embedder
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .embed_passage(&text)?;
        Ok(write_vector(store.conn(), segment_id, &v, text_hash(&text), Some(&text))?.is_some())
    }

    /// Live capture has priority over archive repair. Never hold the store
    /// mutex while waiting for, or running, model inference.
    pub fn embed_live(
        &self,
        store: &Mutex<Store>,
        control: &crate::control::Control,
        id: i64,
    ) -> Result<bool> {
        let _priority = Foreground::new(&self.foreground);
        if control.is_paused() {
            return Ok(false);
        }
        let text: Option<String> = store
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .conn()
            .query_row(
                "SELECT text FROM segments WHERE id=?1 AND deleted_at IS NULL",
                [id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
            return Ok(false);
        };
        let mut embedder = self.embedder.lock().unwrap_or_else(|p| p.into_inner());
        if control.is_paused() {
            return Ok(false);
        }
        let vector = embedder.embed_passage(&text)?;
        drop(embedder);
        let store = store.lock().unwrap_or_else(|p| p.into_inner());
        if control.is_paused() {
            return Ok(false);
        }
        Ok(write_vector(store.conn(), id, &vector, text_hash(&text), Some(&text))?.is_some())
    }

    /// Cached worker telemetry; no SQL, model lock or transcript text.
    pub fn repair_status(&self) -> serde_json::Value {
        let status = self.repair.lock().unwrap_or_else(|p| p.into_inner());
        serde_json::json!({
            "state": status.state, "pending": status.pending,
            "pending_age_ms": status.sampled_at.map(|at| at.elapsed().as_millis() as u64),
            "completed": status.completed, "failed_attempts": status.failed_attempts,
            "discarded": status.discarded,
            "retry_in_ms": status.retry_at.map(|at| at.saturating_duration_since(Instant::now()).as_millis() as u64).unwrap_or(0),
        })
    }

    /// Rank `limit` segments by meaning, within `within`.
    pub fn search(
        &self,
        store: &Store,
        q: &str,
        limit: usize,
        within: &Candidates,
    ) -> Result<Vec<Scored>> {
        self.search_snapshot(self.snapshot(store)?, q, limit, within)
    }

    /// The same ranking as [`Self::search`], against **raw** vectors read
    /// straight from the database rather than the resident matrix — which is
    /// whitened once the corpus clears [`MIN_WHITENING_ROWS`].
    ///
    /// Exists to answer one question on a real archive rather than the
    /// 48-segment fixture [`Whitening`] was measured on: is the correction
    /// still worth what it costs to maintain, at this corpus's actual size
    /// and actual language mix? (`recalld search eval`'s tuning arms,
    /// FINDINGS §51.) A full scan of `segment_vectors` per call — this is a
    /// measurement tool, not a hot path.
    pub fn search_raw(
        &self,
        store: &Store,
        q: &str,
        limit: usize,
        within: &Candidates,
    ) -> Result<Vec<Scored>> {
        let _priority = Foreground::new(&self.foreground);
        let qv = self
            .embedder
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .embed_query(q)?;
        let model_id = self.model_id.clone();
        let conn = store.conn();
        let mut stmt = conn.prepare(
            "SELECT segment_id, vector FROM segment_vectors WHERE model_id = ?1 AND dim = ?2",
        )?;
        let rows = stmt.query_map(params![model_id, qv.dim() as i64], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
        })?;
        let mut best: Vec<Scored> = Vec::with_capacity(limit + 1);
        for row in rows {
            let (id, blob) = row?;
            if !within.admits(id) {
                continue;
            }
            let v = Embedding::from_blob(model_id.clone(), &blob)?;
            let score = dot(&qv.vector, &v.vector);
            if best.len() == limit
                && let Some(last) = best.last()
                && score <= last.score
            {
                continue;
            }
            let at = best.partition_point(|s| s.score >= score);
            best.insert(
                at,
                Scored {
                    segment_id: id,
                    score,
                },
            );
            best.truncate(limit);
        }
        Ok(best)
    }

    /// What `status` and `semantic status` report.
    pub fn stats(&self, store: &Store) -> Result<(usize, usize, Coverage)> {
        let seq = mutation_state(store.conn())?.0;
        let mut cached = self.coverage.lock().unwrap_or_else(|p| p.into_inner());
        if cached.as_ref().is_none_or(|(at, _)| *at != seq) {
            *cached = Some((seq, coverage(store, &self.model_id)?));
        }
        let coverage = cached.as_ref().unwrap().1;
        let index = self.index.lock().unwrap_or_else(|p| p.into_inner());
        Ok((index.len(), index.bytes(), coverage))
    }
}

/// Which segments the scan is allowed to return, as a membership test.
///
/// Two shapes, because the two situations are not the same size. A narrowed
/// search (speaker, source, date window) yields a set of *survivors*, and it is
/// small. An unnarrowed one would yield every segment ever captured — so it is
/// expressed the other way round, as the tombstones to skip, which is bounded
/// by the undo window and is normally empty.
///
/// The soft-delete case is the reason this is not simply `Option<HashSet>`. A
/// deleted turn stops being searchable the moment it is deleted (DESIGN §0),
/// and its vector is still in the matrix until the undo window closes and
/// retention purges it for real.
#[derive(Debug, Clone)]
pub enum Candidates {
    /// Everything except these — the soft-deleted rows.
    AllBut(std::collections::HashSet<i64>),
    /// Only these.
    Only(std::collections::HashSet<i64>),
}

impl Candidates {
    pub fn admits(&self, id: i64) -> bool {
        match self {
            Candidates::AllBut(no) => !no.contains(&id),
            Candidates::Only(yes) => yes.contains(&id),
        }
    }

    /// Nothing narrowed and nothing deleted — the shape unit tests want.
    pub fn everything() -> Self {
        Candidates::AllBut(std::collections::HashSet::new())
    }
}

/// Resolve a facet set against the store.
pub fn candidates(store: &Store, filter: &SegmentFilter) -> Result<Candidates> {
    if filter.is_everything() {
        let mut stmt = store
            .conn()
            .prepare("SELECT id FROM segments WHERE deleted_at IS NOT NULL")?;
        let dead = stmt
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<_>>()?;
        return Ok(Candidates::AllBut(dead));
    }
    let mut stmt = store.conn().prepare(&format!(
        "SELECT g.id
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
           {}",
        crate::store::world_clause(6)
    ))?;
    let ids = stmt
        .query_map(
            params![
                filter.speaker,
                filter.session,
                filter.source,
                filter.from,
                filter.to,
                filter.worlds_json()
            ],
            |r| r.get::<_, i64>(0),
        )?
        .collect::<rusqlite::Result<std::collections::HashSet<i64>>>()?;
    Ok(Candidates::Only(ids))
}

// ---------------------------------------------------------------------------
// the backfill
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BackfillReport {
    pub embedded: usize,
    /// Rows with a transcript that is only whitespace. Counted rather than
    /// skipped silently: it is the difference between "done" and "stuck".
    pub empty: usize,
    pub batches: usize,
}

/// Embed every segment that needs it, `batch` at a time.
///
/// `progress` is called once per batch with `(done, total)` so the caller owns
/// the reporting; nothing here writes to a terminal.
///
/// Interruptible and resumable: each batch is its own transaction, and the work
/// list comes from [`pending_segments`], which asks the database rather than
/// carrying a cursor. Kill it and run it again and it finishes the job.
pub fn backfill(
    store: &Store,
    embedder: &mut TextEmbedder,
    batch: usize,
    limit: Option<usize>,
    mut progress: impl FnMut(&BackfillReport, i64),
) -> Result<BackfillReport> {
    let model_id = embedder.model_id().to_string();
    let batch = batch.max(1);
    let mut report = BackfillReport::default();

    loop {
        let want = match limit {
            Some(n) if report.embedded + report.empty >= n => break,
            Some(n) => batch.min(n - report.embedded - report.empty),
            None => batch,
        };
        let rows = pending_segments(store, &model_id, want)?;
        if rows.is_empty() {
            break;
        }
        // One transaction per batch: a Ctrl-C between batches costs nothing,
        // and holding the write lock for 128 rows rather than 100k keeps the
        // capturing daemon writing alongside it.
        let tx = store.conn().unchecked_transaction()?;
        for (segment_id, text) in &rows {
            if text.trim().is_empty() {
                report.empty += 1;
                continue;
            }
            let v = embedder.embed_passage(text)?;
            if write_vector(&tx, *segment_id, &v, text_hash(text), Some(text))?.is_some() {
                report.embedded += 1;
            }
        }
        tx.commit()?;
        report.batches += 1;
        progress(&report, coverage(store, &model_id)?.eligible);
    }
    Ok(report)
}

/// `recalld semantic backfill`, end to end: resolve the model, nice the thread,
/// walk the backlog, print progress.
///
/// Lives here rather than in `main.rs` so the command and the leg it drives
/// cannot drift apart.
pub fn backfill_command(
    data_dir: &Path,
    cfg: &crate::config::Config,
    dir: Option<&Path>,
    batch: usize,
    limit: Option<usize>,
) -> Result<()> {
    let root = crate::fetch::target_dir(dir, &cfg.models, data_dir);
    let sem = crate::models::SemanticModel::resolve_at(root, &cfg.models);
    if !sem.present() {
        for e in sem.entries().iter().filter(|e| !e.ok()) {
            eprintln!(
                "  ! {:<15} {}",
                e.role,
                crate::fetch::describe(e.state(), &e.path)
            );
        }
        bail!("{}", crate::models::SemanticModel::how_to_get_it());
    }

    // Idle priority, no CPU pinning: this is a batch job competing with a live
    // capture, and the project's rule is that analysis never wins a timeslice
    // from a frame (`pipeline::deprioritise_current_thread`).
    crate::pipeline::deprioritise_current_thread(19, &[]);

    let store = Store::open(data_dir)?;
    let model_id = sem.model_id();
    let before = coverage(&store, &model_id)?;
    println!("model: {model_id}");
    println!(
        "{} of {} transcribed segment(s) already indexed; {} to do",
        before.embedded,
        before.eligible,
        before.pending()
    );
    if before.complete() {
        return Ok(());
    }

    let mut embedder = TextEmbedder::load(&sem)?;
    let started = std::time::Instant::now();
    let report = backfill(&store, &mut embedder, batch, limit, |r, total| {
        let done = before.embedded + r.embedded as i64;
        let rate = r.embedded as f64 / started.elapsed().as_secs_f64().max(0.001);
        eprint!("\r  {done} / {total}  ({:.0} segments/s)\x1b[K", rate);
    })?;
    eprintln!();

    let after = coverage(&store, &model_id)?;
    println!(
        "embedded {} segment(s) in {:.1}s; {} of {} indexed{}",
        report.embedded,
        started.elapsed().as_secs_f64(),
        after.embedded,
        after.eligible,
        if after.complete() {
            String::new()
        } else {
            format!(", {} still to do — run it again", after.pending())
        }
    );
    if report.empty > 0 {
        println!(
            "  {} segment(s) had no words to embed and were skipped",
            report.empty
        );
    }
    Ok(())
}

/// `recalld semantic status`.
pub fn status_command(data_dir: &Path, cfg: &crate::config::Config) -> Result<()> {
    let root = crate::fetch::target_dir(None, &cfg.models, data_dir);
    let sem = crate::models::SemanticModel::resolve_at(root, &cfg.models);
    println!("models dir: {}", sem.root.display());
    for e in sem.entries() {
        println!(
            "  {:<15} {}",
            e.role,
            crate::fetch::describe(e.state(), &e.path)
        );
    }
    if !sem.present() {
        println!("\n{}", crate::models::SemanticModel::how_to_get_it());
        return Ok(());
    }
    let store = Store::open(data_dir)?;
    let model_id = sem.model_id();
    let c = coverage(&store, &model_id)?;
    println!("\nmodel id:   {model_id}");
    println!("dimensions: {DIM}");
    println!(
        "indexed:    {} of {} transcribed segment(s){}",
        c.embedded,
        c.eligible,
        if c.complete() {
            String::new()
        } else {
            format!(
                " — {} pending, run `recalld semantic backfill`",
                c.pending()
            )
        }
    );
    println!(
        "resident:   {} when loaded",
        crate::fetch::human((c.embedded as usize * DIM * 4) as u64)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prefixes_are_exactly_what_e5_was_trained_with() {
        // Spelled out as a literal, not built from a constant: the whole point
        // is that these two strings match the training recipe character for
        // character, including the space after the colon.
        assert_eq!(Prefix::Query.apply("wale"), "query: wale");
        assert_eq!(Prefix::Passage.apply("wale"), "passage: wale");
    }

    #[test]
    fn normalising_makes_a_unit_vector_and_leaves_zero_alone() {
        let mut v = vec![3.0f32, 4.0];
        normalise(&mut v);
        assert!((v[0] - 0.6).abs() < 1e-6 && (v[1] - 0.8).abs() < 1e-6);
        let mut z = vec![0.0f32, 0.0];
        normalise(&mut z);
        assert_eq!(z, vec![0.0, 0.0]);
    }

    #[test]
    fn the_text_hash_is_stable_and_notices_a_correction() {
        assert_eq!(text_hash("portal world"), text_hash("portal world"));
        assert_ne!(text_hash("portal world"), text_hash("portal worlds"));
        // Pinned: changing this number invalidates every stored vector.
        assert_eq!(text_hash(""), 0xcbf2_9ce4_8422_2325u64 as i64);
    }

    // -- fusion --------------------------------------------------------------

    #[test]
    fn fusion_puts_agreement_first() {
        // 2 tops NEITHER list — it is second on both — and still wins, because
        // two legs agreeing is worth more than one leg being certain. That is
        // the whole reason to fuse rather than to concatenate.
        let f = fuse(&[1, 2], &[3, 2], RRF_K);
        let ids: Vec<i64> = f.iter().map(|x| x.segment_id).collect();
        assert_eq!(ids[0], 2, "the row both legs found should lead: {f:?}");
        assert_eq!(f[0].via, Via::Both);
        // 2/62 = .0323 against 1/61 = .0164 for each of the two firsts.
        assert!(f[0].score > f[1].score * 1.9);
    }

    #[test]
    fn a_row_only_one_leg_found_is_labelled_by_that_leg() {
        let f = fuse(&[10], &[20], RRF_K);
        let by: HashMap<i64, Via> = f.iter().map(|x| (x.segment_id, x.via)).collect();
        assert_eq!(by[&10], Via::Keyword);
        assert_eq!(by[&20], Via::Semantic);
    }

    #[test]
    fn fusion_is_deterministic_on_ties() {
        // Disjoint lists, same rank everywhere: every score is equal, so only
        // the tie-break decides — and it must decide the same way every time.
        let a = fuse(&[5, 4], &[3, 2], RRF_K);
        let b = fuse(&[5, 4], &[3, 2], RRF_K);
        assert_eq!(
            a.iter().map(|x| x.segment_id).collect::<Vec<_>>(),
            b.iter().map(|x| x.segment_id).collect::<Vec<_>>()
        );
        // rank 1 on either list beats rank 2 on either list; within a rank, id.
        assert_eq!(
            a.iter().map(|x| x.segment_id).collect::<Vec<_>>(),
            vec![3, 5, 2, 4]
        );
    }

    #[test]
    fn only_one_leg_at_all_still_fuses() {
        let f = fuse(&[7, 8, 9], &[], RRF_K);
        assert_eq!(f.len(), 3);
        assert!(f.iter().all(|x| x.via == Via::Keyword));
        assert_eq!(f[0].segment_id, 7);
    }

    #[test]
    fn a_duplicate_in_one_list_is_only_counted_once() {
        let single = fuse(&[1, 2], &[], RRF_K);
        let doubled = fuse(&[1, 1, 2], &[], RRF_K);
        assert_eq!(doubled.len(), 2);
        assert!((doubled[0].score - single[0].score).abs() < 1e-12);
    }

    #[test]
    fn ranks_travel_with_the_hit() {
        let f = fuse(&[1, 2], &[2, 1], RRF_K);
        let by: HashMap<i64, Fused> = f.iter().map(|x| (x.segment_id, *x)).collect();
        assert_eq!(by[&1].keyword_rank, Some(1));
        assert_eq!(by[&1].semantic_rank, Some(2));
        assert_eq!(by[&2].keyword_rank, Some(2));
        assert_eq!(by[&2].semantic_rank, Some(1));
    }

    // -- the index -----------------------------------------------------------

    fn unit(seed: u64, dim: usize) -> Vec<f32> {
        let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut v: Vec<f32> = (0..dim)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s as i32 as f32) / (i32::MAX as f32)
            })
            .collect();
        normalise(&mut v);
        v
    }

    #[test]
    fn the_index_ranks_by_cosine_and_respects_the_facet_set() {
        let mut ix = VectorIndex::empty("m@1", 3);
        ix.insert(1, &[1.0, 0.0, 0.0]);
        ix.insert(2, &[0.9, 0.436, 0.0]);
        ix.insert(3, &[0.0, 1.0, 0.0]);
        let hits = ix.search(&[1.0, 0.0, 0.0], 3, &Candidates::everything());
        assert_eq!(
            hits.iter().map(|h| h.segment_id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!((hits[0].score - 1.0).abs() < 1e-6);

        let only = Candidates::Only(std::collections::HashSet::from([2i64, 3]));
        let hits = ix.search(&[1.0, 0.0, 0.0], 3, &only);
        assert_eq!(
            hits.iter().map(|h| h.segment_id).collect::<Vec<_>>(),
            vec![2, 3],
            "a facet must narrow the scan, not trim the answer afterwards"
        );
    }

    #[test]
    fn re_embedding_a_segment_replaces_its_row_rather_than_adding_one() {
        let mut ix = VectorIndex::empty("m@1", 2);
        ix.insert(1, &[1.0, 0.0]);
        ix.insert(1, &[0.0, 1.0]);
        assert_eq!(ix.len(), 1);
        let hits = ix.search(&[0.0, 1.0], 1, &Candidates::everything());
        assert!((hits[0].score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_top_k_of_equal_scores_is_the_same_list_every_time() {
        let mut ix = VectorIndex::empty("m@1", 2);
        for id in [9i64, 3, 7, 1] {
            ix.insert(id, &[1.0, 0.0]);
        }
        let a = ix.search(&[1.0, 0.0], 2, &Candidates::everything());
        let b = ix.search(&[1.0, 0.0], 2, &Candidates::everything());
        assert_eq!(a, b);
        // Insertion order, which is id order out of SQL: stable, and the same
        // for a fresh daemon as for one that has been running for a week.
        assert_eq!(a[0].segment_id, 9);
    }

    // -- whitening -----------------------------------------------------------

    /// The failure this exists for, in miniature: two clusters that differ
    /// along one dominant axis ("which language this is"), and a weaker axis
    /// that carries the actual meaning. Raw cosine sorts by the dominant axis;
    /// the correction removes it and the meaning wins.
    #[test]
    fn removing_the_dominant_direction_lets_the_weaker_signal_win() {
        // dim 3: axis 0 is "language", axes 1..2 are "topic".
        let dim = 3;
        let mut raw: Vec<f32> = Vec::new();
        let mut rows: Vec<Vec<f32>> = Vec::new();
        for i in 0..MIN_WHITENING_ROWS {
            // Half the corpus at language +1, half at -1, with a big amplitude
            // on that axis and a small one on topic.
            let lang = if i % 2 == 0 { 4.0 } else { -4.0 };
            let angle = (i as f32) * 0.7;
            let mut v = vec![lang, angle.cos(), angle.sin()];
            normalise(&mut v);
            raw.extend_from_slice(&v);
            rows.push(v);
        }
        let w = Whitening::fit(&raw, dim, WHITENING_COMPONENTS)
            .expect("a corpus this size must yield an estimate");

        // Two rows on the same topic but opposite languages.
        let mut same_topic_other_language = vec![-4.0f32, rows[0][1], rows[0][2]];
        normalise(&mut same_topic_other_language);
        let raw_cos = dot(&rows[0], &same_topic_other_language);
        let fixed = dot(&w.apply(&rows[0]), &w.apply(&same_topic_other_language));
        assert!(
            fixed > raw_cos + 0.5,
            "the correction must recover the cross-language pair: {raw_cos:.3} -> {fixed:.3}"
        );
    }

    #[test]
    fn the_correction_keeps_vectors_on_the_unit_sphere() {
        let dim = 8;
        let mut raw = Vec::new();
        for i in 0..MIN_WHITENING_ROWS {
            raw.extend_from_slice(&unit(i as u64 + 1, dim));
        }
        let w = Whitening::fit(&raw, dim, WHITENING_COMPONENTS).unwrap();
        let out = w.apply(&unit(999, dim));
        let norm: f32 = out.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "the scan is a dot product, so the transform must renormalise: {norm}"
        );
    }

    #[test]
    fn the_correction_is_the_same_every_time_it_is_estimated() {
        let dim = 6;
        let mut raw = Vec::new();
        for i in 0..MIN_WHITENING_ROWS {
            raw.extend_from_slice(&unit(i as u64 + 1, dim));
        }
        let a = Whitening::fit(&raw, dim, 2).unwrap();
        let b = Whitening::fit(&raw, dim, 2).unwrap();
        assert_eq!(a.mean, b.mean);
        assert_eq!(a.comps, b.comps);
    }

    /// Too little to estimate from is not an error, and must not be guessed at:
    /// a top principal direction fitted to twenty turns is noise, and removing
    /// it would remove signal.
    #[test]
    fn a_small_corpus_is_left_alone() {
        let dim = 4;
        let mut raw = Vec::new();
        for i in 0..(MIN_WHITENING_ROWS - 1) {
            raw.extend_from_slice(&unit(i as u64 + 1, dim));
        }
        assert!(Whitening::fit(&raw, dim, 2).is_none());

        let mut ix = VectorIndex::empty("m@1", dim);
        for (i, id) in (1..=8i64).enumerate() {
            ix.insert(id, &unit(i as u64 + 1, dim));
        }
        assert!(!ix.whitened());
        // ...and a raw index still ranks: absent correction, absent problem.
        let hits = ix.search(&unit(1, dim), 1, &Candidates::everything());
        assert_eq!(hits[0].segment_id, 1);
    }

    // -- the schema ----------------------------------------------------------

    /// v9 has to land on whatever the trunk got to in the meantime. Each of
    /// these stamps a version, drops the table, and reopens — which is exactly
    /// what a user's database does when they update.
    #[test]
    fn the_migration_applies_from_v6_v7_and_v8_alike() {
        for from in [6i64, 7, 8] {
            let dir =
                std::env::temp_dir().join(format!("nxr-sem-mig{from}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();

            {
                let s = Store::open(&dir).unwrap();
                let source = s.upsert_source("VRChat.exe", "VRChat", 1).unwrap();
                let session = s.begin_session(source, 1).unwrap();
                let id = s.insert_segment(session, 1, 2, "a.wav", 1).unwrap();
                s.correct_segment_text(id, "portal world").unwrap();
                // Rewind to a database this build has never written, and take
                // v9's table away with it.
                for trigger in [
                    "semantic_vector_insert",
                    "semantic_vector_update",
                    "semantic_vector_delete",
                    "semantic_text_insert",
                    "semantic_text_update",
                    "semantic_text_delete",
                ] {
                    s.conn()
                        .execute_batch(&format!("DROP TRIGGER {trigger}"))
                        .unwrap();
                }
                s.conn().execute_batch("DROP TABLE semantic_dirty; DROP TABLE semantic_models; DROP TABLE semantic_state;").unwrap();
                s.conn()
                    .execute_batch(&format!(
                        "DROP TABLE segment_vectors;
                         UPDATE schema_version SET version = {from};"
                    ))
                    .unwrap();
                // Whatever v7/v8 turn out to be, they are somebody else's
                // tables. Stand one in, so the reopen has to tolerate a schema
                // it does not know about rather than merely a smaller one.
                if from > 6 {
                    s.conn()
                        .execute_batch(
                            "CREATE TABLE some_other_migration (id INTEGER PRIMARY KEY);
                             INSERT INTO some_other_migration (id) VALUES (1);",
                        )
                        .unwrap();
                }
            }

            let s = Store::open(&dir).unwrap();
            let v: i64 = s
                .conn()
                .query_row("SELECT version FROM schema_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(v, crate::store::SCHEMA_VERSION, "migrating from v{from}");
            // The table is back...
            assert_eq!(
                coverage(&s, "m@1").unwrap(),
                Coverage {
                    eligible: 1,
                    embedded: 0
                },
                "migrating from v{from}"
            );
            // ...the transcript survived...
            assert_eq!(s.search("portal", 10).unwrap().len(), 1);
            // ...and the stranger's table was not touched.
            if from > 6 {
                let n: i64 = s
                    .conn()
                    .query_row("SELECT COUNT(*) FROM some_other_migration", [], |r| {
                        r.get(0)
                    })
                    .unwrap();
                assert_eq!(n, 1);
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Re-running the migration on a database that already has vectors must not
    /// drop them — it is called unconditionally on every open.
    #[test]
    fn the_migration_is_a_no_op_the_second_time() {
        let s = Store::open_in_memory().unwrap();
        let source = s.upsert_source("VRChat.exe", "VRChat", 1).unwrap();
        let session = s.begin_session(source, 1).unwrap();
        let id = s.insert_segment(session, 1, 2, "a.wav", 1).unwrap();
        s.correct_segment_text(id, "portal world").unwrap();
        let v = Embedding::new("m@1", vec![1.0, 0.0, 0.0]);
        put_vector(s.conn(), id, &v, text_hash("portal world")).unwrap();

        migrate_v9(s.conn()).unwrap();
        assert_eq!(coverage(&s, "m@1").unwrap().embedded, 1);
    }

    /// Isolated SQLite candidate selection, not end-to-end inference speed.
    #[test]
    #[ignore = "synthetic repair queue benchmark; run explicitly"]
    fn repair_candidate_selection_benchmark() {
        for n in [10_000i64, 100_000] {
            let store = Store::open_in_memory().unwrap();
            let source = store.upsert_source("synthetic", "synthetic", 1).unwrap();
            let session = store.begin_session(source, 1).unwrap();
            store
                .conn()
                .execute(
                    "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<?1)
                INSERT INTO segments(session_id,t_start_ns,t_end_ns,audio_path,created_at,text)
                SELECT ?2,i,i+1,'',i,'synthetic transcript' FROM n",
                    params![n, session],
                )
                .unwrap();
            let old = "SELECT g.id,g.text FROM segments g JOIN (
                SELECT segment_id FROM semantic_dirty UNION SELECT v.segment_id FROM semantic_models m
                CROSS JOIN segment_vectors v ON v.model_id=m.model_id WHERE m.model_id<>?1 AND m.embedded>0
                ) pending ON pending.segment_id=g.id WHERE g.deleted_at IS NULL AND g.text IS NOT NULL
                AND trim(g.text)<>'' ORDER BY g.id LIMIT 32";
            let start = Instant::now();
            for _ in 0..20 {
                let mut stmt = store.conn().prepare(old).unwrap();
                let rows = stmt
                    .query_map(["m@1"], |r| r.get::<_, i64>(0))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                assert_eq!(rows.len(), 32);
            }
            let before = start.elapsed();
            let start = Instant::now();
            for _ in 0..20 {
                assert_eq!(pending_after(&store, "m@1", 32, 0).unwrap().len(), 32);
            }
            eprintln!(
                "repair candidates {n}: 20 reads old={before:?} bounded={:?}; SQLite metadata only, no inference",
                start.elapsed()
            );
        }
    }

    #[test]
    fn repair_candidates_merge_dirty_and_other_model_in_order_without_skipping() {
        let (store, ids) = seeded();
        put_vector(
            store.conn(),
            ids[0],
            &Embedding::new("other@1", vec![1., 0.]),
            1,
        )
        .unwrap();
        assert_eq!(pending_after(&store, "m@1", 1, 0).unwrap()[0].0, ids[0]);
        assert_eq!(
            pending_after(&store, "m@1", 1, ids[0]).unwrap()[0].0,
            ids[1]
        );
        assert!(pending_after(&store, "m@1", 1, ids[1]).unwrap().is_empty());
    }

    #[test]
    fn repair_embeds_outside_store_lock_and_resumes_from_durable_queue() {
        let (store, ids) = seeded();
        let store = Mutex::new(store);
        let status = Mutex::new(RepairStatus::default());
        let now = Instant::now();
        let mut schedule = RepairSchedule::default();
        schedule
            .step(
                &store,
                "m@1",
                &status,
                now,
                || None,
                |text| {
                    assert!(
                        store.try_lock().is_ok(),
                        "inference must not hold SQLite mutex"
                    );
                    assert_eq!(text, "the portal world");
                    Ok(Some(Embedding::new("m@1", vec![1., 0.])))
                },
            )
            .unwrap();
        assert_eq!(status.lock().unwrap().pending, Some(1));
        // New worker, no remembered cursor: success is durably dequeued.
        let mut restarted = RepairSchedule::default();
        restarted
            .step(
                &store,
                "m@1",
                &status,
                now,
                || None,
                |text| {
                    assert_eq!(text, "the fountain");
                    Ok(Some(Embedding::new("m@1", vec![0., 1.])))
                },
            )
            .unwrap();
        assert_eq!(status.lock().unwrap().completed, 2);
        assert_eq!(status.lock().unwrap().pending, Some(0));
        store
            .lock()
            .unwrap()
            .correct_segment_text(ids[0], "corrected words")
            .unwrap();
        restarted
            .step(
                &store,
                "m@1",
                &status,
                now,
                || None,
                |text| {
                    assert_eq!(text, "corrected words");
                    Ok(Some(Embedding::new("m@1", vec![1., 1.])))
                },
            )
            .unwrap();
        assert_eq!(status.lock().unwrap().pending, Some(0));
    }

    #[test]
    fn repair_gates_pause_capture_foreground_and_stop_before_inference() {
        for reason in ["paused", "waiting_capture", "waiting_foreground", "stopped"] {
            let (store, _) = seeded();
            let store = Mutex::new(store);
            let status = Mutex::new(RepairStatus::default());
            RepairSchedule::default()
                .step(
                    &store,
                    "m@1",
                    &status,
                    Instant::now(),
                    || Some(reason),
                    |_| panic!("gate must prevent inference"),
                )
                .unwrap();
            let status = status.lock().unwrap();
            assert_eq!(status.state, reason);
            assert_eq!(
                status.pending, None,
                "gated worker must not touch the store"
            );
            assert_eq!(status.completed, 0);
        }
    }

    #[test]
    fn repair_pause_or_shutdown_during_inference_discards_result() {
        for reason in ["paused", "stopped"] {
            let (store, _) = seeded();
            let store = Mutex::new(store);
            let status = Mutex::new(RepairStatus::default());
            let gate = std::cell::Cell::new(None);
            RepairSchedule::default()
                .step(
                    &store,
                    "m@1",
                    &status,
                    Instant::now(),
                    || gate.get(),
                    |_| {
                        gate.set(Some(reason));
                        Ok(Some(Embedding::new("m@1", vec![1., 0.])))
                    },
                )
                .unwrap();
            assert_eq!(status.lock().unwrap().discarded, 1);
            assert_eq!(
                coverage(&store.lock().unwrap(), "m@1").unwrap().pending(),
                2
            );
        }
    }

    #[test]
    fn repair_edits_and_deletions_during_inference_never_write_stale_vectors() {
        for delete in [false, true] {
            let (store, ids) = seeded();
            let store = Mutex::new(store);
            let status = Mutex::new(RepairStatus::default());
            RepairSchedule::default()
                .step(
                    &store,
                    "m@1",
                    &status,
                    Instant::now(),
                    || None,
                    |_| {
                        let store = store.lock().unwrap();
                        if delete {
                            store.conn().execute(
                                "UPDATE segments SET deleted_at=1 WHERE id=?1",
                                [ids[0]],
                            )?;
                        } else {
                            store.correct_segment_text(ids[0], "new words")?;
                        }
                        Ok(Some(Embedding::new("m@1", vec![1., 0.])))
                    },
                )
                .unwrap();
            assert_eq!(status.lock().unwrap().discarded, 1);
            assert_eq!(status.lock().unwrap().completed, 0);
            let count: i64 = store
                .lock()
                .unwrap()
                .conn()
                .query_row("SELECT count(*) FROM segment_vectors", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0);
        }
    }

    #[test]
    fn repair_failure_backoff_does_not_starve_other_rows_and_edit_retries_immediately() {
        let (store, ids) = seeded();
        let store = Mutex::new(store);
        let status = Mutex::new(RepairStatus::default());
        let mut schedule = RepairSchedule::default();
        let now = Instant::now();
        schedule
            .step(
                &store,
                "m@1",
                &status,
                now,
                || None,
                |_| bail!("synthetic failure"),
            )
            .unwrap();
        assert_eq!(schedule.retries[&ids[0]].due, now + Duration::from_secs(1));
        schedule
            .step(
                &store,
                "m@1",
                &status,
                now,
                || None,
                |text| {
                    assert_eq!(text, "the fountain", "a poison row cannot starve the queue");
                    Ok(Some(Embedding::new("m@1", vec![1., 0.])))
                },
            )
            .unwrap();
        schedule
            .step(
                &store,
                "m@1",
                &status,
                now,
                || None,
                |_| panic!("retry not due"),
            )
            .unwrap();
        assert_eq!(status.lock().unwrap().state, "backoff");
        schedule
            .step(
                &store,
                "m@1",
                &status,
                now + Duration::from_secs(1),
                || None,
                |_| bail!("still broken"),
            )
            .unwrap();
        assert_eq!(schedule.retries[&ids[0]].due, now + Duration::from_secs(3));
        store
            .lock()
            .unwrap()
            .correct_segment_text(ids[0], "fixed text")
            .unwrap();
        schedule
            .step(
                &store,
                "m@1",
                &status,
                now + Duration::from_secs(1),
                || None,
                |text| {
                    assert_eq!(text, "fixed text");
                    Ok(Some(Embedding::new("m@1", vec![1., 0.])))
                },
            )
            .unwrap();
        assert_eq!(status.lock().unwrap().pending, Some(0));
        assert_eq!(status.lock().unwrap().failed_attempts, 2);
    }

    #[test]
    fn repair_yields_without_waiting_for_busy_store() {
        let (store, _) = seeded();
        let store = Mutex::new(store);
        let held = store.lock().unwrap();
        let status = Mutex::new(RepairStatus::default());
        RepairSchedule::default()
            .step(
                &store,
                "m@1",
                &status,
                Instant::now(),
                || None,
                |_| panic!("busy store cannot start inference"),
            )
            .unwrap();
        assert_eq!(status.lock().unwrap().state, "waiting_store");
        assert_eq!(status.lock().unwrap().pending, None);
        drop(held);
    }

    #[test]
    fn repair_error_memory_and_retry_delay_are_bounded() {
        let (store, ids) = seeded();
        let store = Mutex::new(store);
        let status = Mutex::new(RepairStatus::default());
        let now = Instant::now();
        let mut schedule = RepairSchedule::default();
        for id in 100..356 {
            schedule.retries.insert(
                id,
                Retry {
                    hash: 0,
                    attempts: 1,
                    due: now,
                },
            );
        }
        schedule
            .step(
                &store,
                "m@1",
                &status,
                now,
                || None,
                |_| bail!("synthetic failure"),
            )
            .unwrap();
        assert_eq!(schedule.retries.len(), 256);
        schedule.retries.get_mut(&ids[0]).unwrap().attempts = 100;
        schedule.cursor = 0;
        schedule
            .step(
                &store,
                "m@1",
                &status,
                now + Duration::from_secs(1),
                || None,
                |_| bail!("persistent failure"),
            )
            .unwrap();
        assert_eq!(schedule.retries[&ids[0]].due, now + Duration::from_secs(61));
    }

    #[test]
    fn repair_model_busy_keeps_row_pending_without_counting_failure() {
        let (store, _) = seeded();
        let store = Mutex::new(store);
        let status = Mutex::new(RepairStatus::default());
        RepairSchedule::default()
            .step(
                &store,
                "m@1",
                &status,
                Instant::now(),
                || None,
                |_| Ok(None),
            )
            .unwrap();
        let status = status.lock().unwrap();
        assert_eq!(status.state, "waiting_foreground");
        assert_eq!(status.pending, Some(2));
        assert_eq!(status.failed_attempts, 0);
    }

    #[test]
    fn repair_foreground_guard_unwinds_and_stop_interrupts_wait() {
        let counter = AtomicUsize::new(0);
        {
            let _outer = Foreground::new(&counter);
            {
                let _inner = Foreground::new(&counter);
                assert_eq!(counter.load(Ordering::SeqCst), 2);
            }
            assert_eq!(counter.load(Ordering::SeqCst), 1);
        }
        assert_eq!(counter.load(Ordering::SeqCst), 0);
        let stop = Arc::new(RepairStop::default());
        let worker = Arc::clone(&stop);
        let (send, recv) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
            worker.wait(Duration::from_secs(60));
            send.send(()).unwrap();
        });
        stop.stop();
        recv.recv_timeout(Duration::from_secs(2))
            .expect("stop must wake polling wait");
        join.join().unwrap();
    }

    // -- the store round trip ------------------------------------------------

    fn seeded() -> (Store, Vec<i64>) {
        let s = Store::open_in_memory().unwrap();
        let source = s.upsert_source("VRChat.exe", "VRChat", 1).unwrap();
        let session = s.begin_session(source, 1).unwrap();
        let mut ids = Vec::new();
        for (i, text) in ["the portal world", "the fountain", ""].iter().enumerate() {
            let t = 100 + i as i64;
            let id = s
                .insert_segment(session, t, t + 1, &format!("{i}.wav"), t)
                .unwrap();
            if !text.is_empty() {
                s.correct_segment_text(id, text).unwrap();
            }
            ids.push(id);
        }
        (s, ids)
    }

    #[test]
    fn a_segment_with_no_transcript_is_not_pending_work() {
        let (s, ids) = seeded();
        let c = coverage(&s, "m@1").unwrap();
        assert_eq!(c.eligible, 2, "the wordless turn is not owed a vector");
        assert_eq!(c.embedded, 0);
        let pending: Vec<i64> = pending_segments(&s, "m@1", 10)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(pending, vec![ids[0], ids[1]]);
    }

    #[test]
    fn a_vector_from_another_model_is_pending_work_again() {
        let (s, ids) = seeded();
        let v = Embedding::new("old@1", vec![1.0, 0.0]);
        put_vector(s.conn(), ids[0], &v, text_hash("the portal world")).unwrap();
        assert_eq!(coverage(&s, "old@1").unwrap().embedded, 1);
        assert_eq!(coverage(&s, "new@1").unwrap().embedded, 0);
        let pending: Vec<i64> = pending_segments(&s, "new@1", 10)
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(pending, vec![ids[0], ids[1]]);
    }

    #[test]
    fn re_embedding_bumps_the_watermark_without_adding_a_row() {
        let (s, ids) = seeded();
        let v = Embedding::new("m@1", vec![1.0, 0.0]);
        let first = put_vector(s.conn(), ids[0], &v, 1).unwrap();
        let second = put_vector(s.conn(), ids[0], &v, 2).unwrap();
        assert!(second > first, "the watermark must move on an update");
        assert_eq!(coverage(&s, "m@1").unwrap().embedded, 1);
    }

    /// Deleting a turn has to delete what makes it findable, by meaning as much
    /// as by word (DESIGN §0).
    #[test]
    fn purging_a_segment_takes_its_vector_with_it() {
        let (s, ids) = seeded();
        let v = Embedding::new("m@1", vec![1.0, 0.0]);
        put_vector(s.conn(), ids[0], &v, 1).unwrap();
        assert_eq!(coverage(&s, "m@1").unwrap().embedded, 1);
        s.purge_segments(&[ids[0]]).unwrap();
        assert_eq!(coverage(&s, "m@1").unwrap().embedded, 0);
    }

    /// A soft delete is not a purge — the row and its vector are still there
    /// for the undo window — so the *scan* is what has to hide it.
    #[test]
    fn a_soft_deleted_turn_stops_being_a_candidate_immediately() {
        let (s, ids) = seeded();
        assert!(
            candidates(&s, &SegmentFilter::default())
                .unwrap()
                .admits(ids[0])
        );
        s.soft_delete_segments(&[ids[0]], 999).unwrap();
        let within = candidates(&s, &SegmentFilter::default()).unwrap();
        assert!(!within.admits(ids[0]), "a deleted turn is not searchable");
        assert!(within.admits(ids[1]));
    }

    #[test]
    fn a_facet_narrows_the_candidate_set() {
        let (s, ids) = seeded();
        let wide = candidates(&s, &SegmentFilter::default()).unwrap();
        assert!(matches!(wide, Candidates::AllBut(_)));
        let narrow = candidates(
            &s,
            &SegmentFilter {
                source: Some("Discord.exe".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(narrow, Candidates::Only(_)));
        assert!(!narrow.admits(ids[0]));
    }

    /// The index has to see work another *process* did — the backfill runs
    /// outside the daemon, and a daemon that only trusted its own writes would
    /// answer from a stale matrix for ever.
    #[test]
    fn the_index_picks_up_rows_it_did_not_write() {
        let (s, ids) = seeded();
        let mut ix = VectorIndex::empty("m@1", 2);
        ix.refresh(s.conn()).unwrap();
        assert_eq!(ix.len(), 0);

        put_vector(s.conn(), ids[0], &Embedding::new("m@1", vec![1.0, 0.0]), 1).unwrap();
        ix.refresh(s.conn()).unwrap();
        assert_eq!(ix.len(), 1);

        // An update in place, which does not move the segment id.
        put_vector(s.conn(), ids[0], &Embedding::new("m@1", vec![0.0, 1.0]), 2).unwrap();
        ix.refresh(s.conn()).unwrap();
        assert_eq!(ix.len(), 1);
        let hits = ix.search(&[0.0, 1.0], 1, &Candidates::everything());
        assert!(
            (hits[0].score - 1.0).abs() < 1e-6,
            "the new vector, not the old"
        );

        // ...and a deletion, which a watermark alone cannot see.
        s.purge_segments(&[ids[0]]).unwrap();
        ix.refresh(s.conn()).unwrap();
        assert_eq!(ix.len(), 0);
    }

    #[test]
    fn a_live_vector_before_first_refresh_preserves_the_existing_archive() {
        let (s, ids) = seeded();
        let old = Embedding::new("m@1", vec![1.0, 0.0]);
        let live = Embedding::new("m@1", vec![0.0, 1.0]);
        put_vector(s.conn(), ids[0], &old, 1).unwrap();
        let mut ix = VectorIndex::empty("m@1", 2);
        let seq = put_vector(s.conn(), ids[1], &live, 2).unwrap();
        ix.note(ids[1], &live, seq);

        ix.refresh(s.conn()).unwrap();
        assert_eq!(ix.len(), 2, "the archive survives the first live write");
        assert_eq!(
            ix.search(&old.vector, 1, &Candidates::everything())[0].segment_id,
            ids[0]
        );
    }

    #[test]
    fn a_live_vector_does_not_skip_an_external_update_between_refreshes() {
        let (s, ids) = seeded();
        put_vector(s.conn(), ids[0], &Embedding::new("m@1", vec![1.0, 0.0]), 1).unwrap();
        let mut ix = VectorIndex::empty("m@1", 2);
        ix.refresh(s.conn()).unwrap();

        // A backfill updates an existing vector, then live capture writes its
        // next vector before the daemon searches or reports index statistics.
        put_vector(s.conn(), ids[0], &Embedding::new("m@1", vec![0.0, 1.0]), 2).unwrap();
        let live = Embedding::new("m@1", vec![1.0, 0.0]);
        let seq = put_vector(s.conn(), ids[1], &live, 3).unwrap();
        ix.note(ids[1], &live, seq);
        ix.refresh(s.conn()).unwrap();

        let hits = ix.search(&[0.0, 1.0], 1, &Candidates::everything());
        assert_eq!(hits[0].segment_id, ids[0]);
        assert!((hits[0].score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn vectors_from_another_model_never_enter_the_index() {
        let (s, ids) = seeded();
        put_vector(
            s.conn(),
            ids[0],
            &Embedding::new("other@1", vec![1.0, 0.0]),
            1,
        )
        .unwrap();
        let mut ix = VectorIndex::empty("m@1", 2);
        ix.refresh(s.conn()).unwrap();
        assert_eq!(ix.len(), 0, "cosine across models is a meaningless number");
    }

    #[test]
    fn v21_same_count_replacement_survives_deleted_latest_sequence() {
        let (s, ids) = seeded();
        let v = Embedding::new("m@1", vec![1.0, 0.0]);
        let first = put_vector(s.conn(), ids[0], &v, 1).unwrap();
        let mut index = VectorIndex::empty("m@1", 2);
        index.refresh(s.conn()).unwrap();
        s.purge_segments(&[ids[0]]).unwrap();
        let next = put_vector(s.conn(), ids[1], &v, 2).unwrap();
        assert!(next > first);
        index.refresh(s.conn()).unwrap();
        assert_eq!(index.ids, vec![ids[1]]);
        s.conn().execute("DELETE FROM segment_vectors", []).unwrap();
        assert!(put_vector(s.conn(), ids[1], &v, 3).unwrap() > next);
    }

    #[test]
    fn v21_model_replacement_removes_old_space_even_when_count_stays_equal() {
        let (s, ids) = seeded();
        put_vector(s.conn(), ids[0], &Embedding::new("m@1", vec![1.0, 0.0]), 1).unwrap();
        let mut index = VectorIndex::empty("m@1", 2);
        index.refresh(s.conn()).unwrap();
        put_vector(s.conn(), ids[0], &Embedding::new("m@2", vec![0.0, 1.0]), 1).unwrap();
        put_vector(s.conn(), ids[1], &Embedding::new("m@1", vec![1.0, 0.0]), 1).unwrap();
        index.refresh(s.conn()).unwrap();
        assert_eq!(index.ids, vec![ids[1]]);
        assert_eq!(coverage(&s, "m@1").unwrap().embedded, 1);
        assert_eq!(coverage(&s, "m@2").unwrap().embedded, 1);
    }

    #[test]
    fn v21_dirty_queue_and_coverage_follow_edits_deletes_and_undo() {
        let (s, ids) = seeded();
        let v = Embedding::new("m@1", vec![1.0, 0.0]);
        put_vector(s.conn(), ids[0], &v, text_hash("the portal world")).unwrap();
        assert_eq!(
            coverage(&s, "m@1").unwrap(),
            Coverage {
                eligible: 2,
                embedded: 1
            }
        );
        s.correct_segment_text(ids[0], "corrected fountain")
            .unwrap();
        assert_eq!(
            coverage(&s, "m@1").unwrap(),
            Coverage {
                eligible: 2,
                embedded: 0
            }
        );
        assert_eq!(pending_segments(&s, "m@1", 1).unwrap()[0].0, ids[0]);
        put_vector(s.conn(), ids[0], &v, text_hash("corrected fountain")).unwrap();
        s.soft_delete_segments(&[ids[0]], 999).unwrap();
        assert_eq!(
            coverage(&s, "m@1").unwrap(),
            Coverage {
                eligible: 1,
                embedded: 0
            }
        );
        s.conn()
            .execute(
                "UPDATE segments SET deleted_at=NULL WHERE id=?1",
                params![ids[0]],
            )
            .unwrap();
        assert_eq!(
            coverage(&s, "m@1").unwrap(),
            Coverage {
                eligible: 2,
                embedded: 0
            }
        );
        assert_eq!(pending_segments(&s, "m@1", 1).unwrap()[0].0, ids[0]);
        assert!(pending_segments(&s, "m@1", 0).unwrap().is_empty());
        s.correct_segment_text(ids[0], " \n\t\u{a0}\u{2003}")
            .unwrap();
        assert_eq!(coverage(&s, "m@1").unwrap().eligible, 1);
        assert_eq!(pending_segments(&s, "m@1", 10).unwrap().len(), 1);
    }

    #[test]
    fn v21_metadata_changes_do_not_touch_fts_or_semantic_index() {
        let (s, ids) = seeded();
        let v = Embedding::new("m@1", vec![1.0, 0.0]);
        put_vector(s.conn(), ids[0], &v, text_hash("the portal world")).unwrap();
        let before = mutation_state(s.conn()).unwrap();
        let writes = s.conn().total_changes();
        s.conn()
            .execute(
                "UPDATE segments SET mood='calm' WHERE id=?1",
                params![ids[0]],
            )
            .unwrap();
        assert_eq!(
            s.conn().total_changes() - writes,
            1,
            "only the metadata row changes"
        );
        assert_eq!(mutation_state(s.conn()).unwrap(), before);
        assert_eq!(s.search("portal", 10).unwrap().len(), 1);
        s.correct_segment_text(ids[0], "changed words").unwrap();
        assert!(s.search("portal", 10).unwrap().is_empty());
        assert_eq!(s.search("changed", 10).unwrap().len(), 1);
        s.conn()
            .execute("UPDATE segments SET text=NULL WHERE id=?1", params![ids[0]])
            .unwrap();
        assert!(s.search("changed", 10).unwrap().is_empty());
        s.correct_segment_text(ids[0], "restored words").unwrap();
        assert_eq!(s.search("restored", 10).unwrap().len(), 1);
    }

    #[test]
    fn v21_inflight_vectors_and_results_cannot_outlive_their_text() {
        let (s, ids) = seeded();
        let v = Embedding::new("m@1", vec![1.0, 0.0]);
        let seq = write_vector(
            s.conn(),
            ids[0],
            &v,
            text_hash("the portal world"),
            Some("the portal world"),
        )
        .unwrap()
        .unwrap();
        assert!(vector_current(&s, ids[0], "m@1", seq).unwrap());
        s.correct_segment_text(ids[0], "new words").unwrap();
        assert!(!vector_current(&s, ids[0], "m@1", seq).unwrap());
        assert!(
            write_vector(s.conn(), ids[0], &v, 1, Some("the portal world"))
                .unwrap()
                .is_none()
        );
        let next = write_vector(
            s.conn(),
            ids[0],
            &v,
            text_hash("new words"),
            Some("new words"),
        )
        .unwrap()
        .unwrap();
        assert!(!vector_current(&s, ids[0], "m@1", seq).unwrap());
        assert!(vector_current(&s, ids[0], "m@1", next).unwrap());
        s.soft_delete_segments(&[ids[0]], 999).unwrap();
        assert!(!vector_current(&s, ids[0], "m@1", next).unwrap());
        assert!(
            write_vector(s.conn(), ids[0], &v, 1, Some("new words"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn v21_mutations_rollback_with_the_transcript_statement() {
        let (s, ids) = seeded();
        let before = mutation_state(s.conn()).unwrap();
        {
            let tx = s.conn().unchecked_transaction().unwrap();
            put_vector(&tx, ids[0], &Embedding::new("m@1", vec![1.0, 0.0]), 1).unwrap();
            // Dropping without commit must restore counters and the dirty row.
        }
        assert_eq!(mutation_state(s.conn()).unwrap(), before);
        assert_eq!(coverage(&s, "m@1").unwrap().embedded, 0);
        assert_eq!(pending_segments(&s, "m@1", 10).unwrap().len(), 2);
        migrate_v21(s.conn()).unwrap();
        assert_eq!(mutation_state(s.conn()).unwrap(), before);
    }

    #[test]
    fn v21_reconciles_a_v20_archive_and_rolls_back_a_failed_migration() {
        let dir = std::env::temp_dir().join(format!(
            "nx-semantic-migration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let s = Store::open(&dir).unwrap();
        let source = s.upsert_source("synthetic", "synthetic", 1).unwrap();
        let session = s.begin_session(source, 1).unwrap();
        let ids: Vec<_> = (0..2)
            .map(|i| {
                let id = s.insert_segment(session, i, i + 1, "", i).unwrap();
                s.correct_segment_text(id, "original text").unwrap();
                id
            })
            .collect();
        for trigger in [
            "semantic_vector_insert",
            "semantic_vector_update",
            "semantic_vector_delete",
            "semantic_text_insert",
            "semantic_text_update",
            "semantic_text_delete",
        ] {
            s.conn()
                .execute_batch(&format!("DROP TRIGGER {trigger}"))
                .unwrap();
        }
        s.conn()
            .execute_batch(
                "DROP TABLE semantic_dirty; DROP TABLE semantic_models; DROP TABLE semantic_state;
            DROP INDEX idx_segment_vectors_global_seq;
            DROP TRIGGER segments_fts_update;
            CREATE TRIGGER segments_fts_update AFTER UPDATE ON segments BEGIN
                INSERT INTO segments_fts(segments_fts,rowid,text) VALUES('delete',old.id,old.text);
                INSERT INTO segments_fts(rowid,text) VALUES(new.id,new.text);
            END;
            UPDATE schema_version SET version=20;",
            )
            .unwrap();
        for (i, id) in ids.iter().enumerate() {
            s.conn()
                .execute(
                    "INSERT INTO segment_vectors VALUES(?1,?2,?3,'m@1',2,?4)",
                    params![
                        id,
                        7 + i as i64,
                        Embedding::new("m@1", vec![1.0, 0.0]).to_blob(),
                        text_hash("original text")
                    ],
                )
                .unwrap();
        }
        s.correct_segment_text(ids[1], "corrected text").unwrap();
        s.conn()
            .execute_batch(
                "CREATE TRIGGER reject_migration BEFORE DELETE ON segment_vectors
            BEGIN SELECT RAISE(ABORT,'test migration rollback'); END;",
            )
            .unwrap();
        let path = s.conn().path().unwrap().to_string();
        drop(s);
        assert!(Store::open(&dir).is_err());
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            conn.query_row("SELECT version FROM schema_version", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            20
        );
        assert!(
            !conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='semantic_state')",
                    [],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap()
        );
        let trigger: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='segments_fts_update'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !trigger.contains("UPDATE OF text"),
            "FTS DDL rolls back too"
        );
        conn.execute_batch("DROP TRIGGER reject_migration").unwrap();
        drop(conn);
        let s = Store::open(&dir).unwrap();
        assert_eq!(
            coverage(&s, "m@1").unwrap(),
            Coverage {
                eligible: 2,
                embedded: 1
            }
        );
        assert_eq!(
            pending_segments(&s, "m@1", 10).unwrap(),
            vec![(ids[1], "corrected text".into())]
        );
        assert!(
            put_vector(
                s.conn(),
                ids[1],
                &Embedding::new("m@1", vec![1.0, 0.0]),
                text_hash("corrected text")
            )
            .unwrap()
                > 8
        );
        let writes = s.conn().total_changes();
        s.conn()
            .execute("UPDATE segments SET mood='calm'", [])
            .unwrap();
        assert_eq!(s.conn().total_changes() - writes, 2);
        assert_eq!(s.search("corrected", 10).unwrap().len(), 1);
        drop(s);
        let s = Store::open(&dir).unwrap();
        assert_eq!(coverage(&s, "m@1").unwrap().embedded, 2);
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn v21_sequences_are_unique_across_connections_and_survive_reopen() {
        let dir = std::env::temp_dir().join(format!(
            "nx-semantic-v21-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let s = Store::open(&dir).unwrap();
        let source = s.upsert_source("synthetic", "synthetic", 1).unwrap();
        let session = s.begin_session(source, 1).unwrap();
        let ids: Vec<_> = (0..2)
            .map(|i| {
                let id = s.insert_segment(session, i, i + 1, "", i).unwrap();
                s.correct_segment_text(id, "synthetic").unwrap();
                id
            })
            .collect();
        let path = s.conn().path().unwrap().to_string();
        let handles: Vec<_> = ids
            .iter()
            .map(|&id| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let conn = Connection::open(path).unwrap();
                    conn.busy_timeout(std::time::Duration::from_secs(5))
                        .unwrap();
                    (0..20)
                        .map(|_| {
                            put_vector(
                                &conn,
                                id,
                                &Embedding::new("m@1", vec![1.0, 0.0]),
                                text_hash("synthetic"),
                            )
                            .unwrap()
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let mut sequences: Vec<_> = handles
            .into_iter()
            .flat_map(|h| h.join().unwrap())
            .collect();
        sequences.sort_unstable();
        sequences.dedup();
        assert_eq!(sequences.len(), 40);
        let last = *sequences.last().unwrap();
        s.conn().execute("DELETE FROM segment_vectors", []).unwrap();
        drop(s);
        let s = Store::open(&dir).unwrap();
        assert!(
            put_vector(s.conn(), ids[0], &Embedding::new("m@1", vec![1.0, 0.0]), 1).unwrap() > last
        );
        assert_eq!(coverage(&s, "m@1").unwrap().embedded, 1);
        drop(s);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn v21_index_update_can_be_applied_after_the_database_is_closed() {
        let (s, ids) = seeded();
        put_vector(s.conn(), ids[0], &Embedding::new("m@1", vec![1.0, 0.0]), 1).unwrap();
        let mut index = VectorIndex::empty("m@1", 2);
        let update = IndexUpdate::read(&index, s.conn()).unwrap();
        drop(s);
        index.apply_update(update).unwrap();
        assert_eq!(
            index.search(&[1.0, 0.0], 1, &Candidates::everything())[0].segment_id,
            ids[0]
        );
    }

    /// No recordings or models; measures database work only. Run explicitly:
    /// cargo test -p recalld --lib v21_synthetic_index_benchmark -- --ignored --nocapture
    #[test]
    #[ignore = "synthetic indexing benchmark; run explicitly"]
    fn v21_synthetic_index_benchmark() {
        for n in [10_000usize, 100_000] {
            let s = Store::open_in_memory().unwrap();
            let source = s.upsert_source("synthetic", "synthetic", 1).unwrap();
            let session = s.begin_session(source, 1).unwrap();
            let tx = s.conn().unchecked_transaction().unwrap();
            tx.execute(
                "WITH RECURSIVE n(i) AS (VALUES(1) UNION ALL SELECT i+1 FROM n WHERE i<?1)
                INSERT INTO segments(session_id,t_start_ns,t_end_ns,audio_path,created_at,text)
                SELECT ?2,i,i+1,'',i,'synthetic transcript' FROM n",
                params![n as i64, session],
            )
            .unwrap();
            let v = Embedding::new("m@1", vec![1.0, 0.0]);
            tx.execute(
                "INSERT INTO segment_vectors(segment_id,seq,vector,model_id,dim,text_hash)
                SELECT id,0,?1,'m@1',2,?2 FROM segments",
                params![v.to_blob(), text_hash("synthetic transcript")],
            )
            .unwrap();
            tx.commit().unwrap();
            let started = std::time::Instant::now();
            for _ in 0..1000 {
                put_vector(s.conn(), 1, &v, 1).unwrap();
            }
            let write_us = started.elapsed().as_secs_f64() * 1000.0;
            let started = std::time::Instant::now();
            for _ in 0..1000 {
                assert_eq!(coverage(&s, "m@1").unwrap().embedded, n as i64);
            }
            let coverage_us = started.elapsed().as_secs_f64() * 1000.0;
            s.conn()
                .execute(
                    "UPDATE segments SET text='corrected transcript' WHERE id<=100",
                    [],
                )
                .unwrap();
            let started = std::time::Instant::now();
            assert_eq!(pending_segments(&s, "m@1", 100).unwrap().len(), 100);
            let pending_ms = started.elapsed().as_secs_f64() * 1000.0;
            let started = std::time::Instant::now();
            s.conn()
                .execute("UPDATE segments SET mood='calm'", [])
                .unwrap();
            let metadata_ms = started.elapsed().as_secs_f64() * 1000.0;
            println!(
                "{}",
                serde_json::json!({"rows":n,"vector_write_mean_us":write_us,
                "coverage_mean_us":coverage_us,"pending_100_ms":pending_ms,"metadata_update_ms":metadata_ms,
                "scope":"synthetic SQLite; excludes model inference and real recordings"})
            );
        }
    }

    /// The measurement the brute force rests on. 100k x 384 f32 is 147 MB; if
    /// scanning it were not tens of milliseconds this design would need an
    /// approximate index instead, so the number is asserted, not assumed.
    #[test]
    fn brute_force_at_scale() {
        let n = 100_000usize;
        let mut ix = VectorIndex::empty("m@1", DIM);
        ix.ids = (1..=n as i64).collect();
        ix.at = ix.ids.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        ix.data = Vec::with_capacity(n * DIM);
        let block = unit(7, DIM);
        for _ in 0..n {
            ix.data.extend_from_slice(&block);
        }
        assert_eq!(ix.bytes(), n * DIM * 4);

        let q = unit(11, DIM);
        // Thread CPU time, not wall time. This guard asks whether the ALGORITHM
        // is still cheap; wall time answers a different question — how busy the
        // box is — and under `nice 19` on a machine running six other builds it
        // answered "no" at 620 ms for work costing a fraction of that in CPU. Only the
        // scheduler's contribution is excluded; a slower algorithm still fails.
        fn thread_cpu_ms() -> f64 {
            let mut ts = libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            // SAFETY: a valid, writable timespec and a clock id libc defines.
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
            ts.tv_sec as f64 * 1000.0 + ts.tv_nsec as f64 / 1e6
        }
        // CPU time is still not load-independent: neighbours evicting L3 and a
        // clock that has stopped boosting inflate it by a third or so on a
        // 32-core box at load 40. So take the best of three samples — the least
        // disturbed one — rather than a single draw. A slower ALGORITHM is
        // slower in every sample and gets no help from this.
        let sample = |ix: &VectorIndex| {
            let c0 = thread_cpu_ms();
            let hits = ix.search(&q, 50, &Candidates::everything());
            (hits, thread_cpu_ms() - c0)
        };
        let (hits, first) = sample(&ix);
        let ms = (1..3).fold(first, |best, _| best.min(sample(&ix).1));
        assert_eq!(hits.len(), 50);
        // Measured 2026-09-05 in the unoptimised test profile `cargo test`
        // builds: 430-480 ms on a 32-core desktop with the box at load 13-40.
        // (An optimised build is roughly an order of magnitude faster; the
        // "tens of milliseconds" in the module docs is that build.) The bar is
        // ~5x the measured figure: a busy machine does not reach it, the
        // order-of-magnitude regressions this guard exists for — reading the
        // matrix back out of SQLite per query, say — still do. This is a
        // regression guard on the algorithm, not a benchmark.
        assert!(
            ms < 2500.0,
            "a {n}-vector scan took {ms:.1} ms of CPU (best of 3) — brute force is no longer the right shape"
        );
        eprintln!(
            "brute force: {n} x {DIM} = {} MB, {ms:.1} ms (best of 3)",
            ix.bytes() >> 20
        );
    }
}
