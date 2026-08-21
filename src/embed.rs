//! Semantic search: dense embeddings + Reciprocal Rank Fusion (RRF) with BM25.
//!
//! The DB-facing logic (vector storage, cosine search, RRF fusion) is always
//! compiled and unit-tested with a mock embedder. Only the concrete network
//! client [`RemoteEmbedder`] is gated behind the `remote-embed` feature so the
//! default crates.io build stays lean and works offline (BM25 only).

use anyhow::Result;
use rusqlite::{params, Connection};

/// Embedding intent. e5-style models embed documents and queries differently;
/// the embedding service applies the right prefix based on this mode, so callers
/// pass raw text (no manual `passage:` / `query:` prefixing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedMode {
    /// Documents being indexed.
    Index,
    /// A search query.
    Search,
}

impl EmbedMode {
    pub fn as_str(self) -> &'static str {
        match self {
            EmbedMode::Index => "index",
            EmbedMode::Search => "search",
        }
    }
}

/// Produces dense vectors for a batch of texts. Implementations may be remote
/// (HTTP embedding service) or, in the future, local (ONNX).
pub trait Embedder {
    /// Embed each input text, returning one vector per input in the same order.
    fn embed(&self, texts: &[String], mode: EmbedMode) -> Result<Vec<Vec<f32>>>;
}

/// Serialize a vector as little-endian f32 bytes for BLOB storage.
pub fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Deserialize a little-endian f32 BLOB back into a vector.
pub fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|&c| f32::from_le_bytes(c))
        .collect()
}

/// Cosine similarity in [-1, 1]; returns 0.0 if either vector is zero-length.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// Reciprocal Rank Fusion over several ranked lists.
///
/// Each list is ordered best-first; an item's fused score is the sum of
/// `1 / (k + rank)` (rank is 1-based) across the lists it appears in. `k=60` is
/// the standard constant. Returns the top `top_n` file ids by fused score.
pub fn rrf_fuse(rankings: &[&[(i64, f32)]], k: f64, top_n: usize) -> Vec<(i64, f32)> {
    use std::collections::HashMap;
    let mut acc: HashMap<i64, f64> = HashMap::new();
    for ranking in rankings {
        for (rank, (fid, _)) in ranking.iter().enumerate() {
            *acc.entry(*fid).or_insert(0.0) += 1.0 / (k + (rank as f64) + 1.0);
        }
    }
    let mut fused: Vec<(i64, f32)> = acc.into_iter().map(|(id, s)| (id, s as f32)).collect();
    fused.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    fused.truncate(top_n);
    fused
}

/// True if the `embeddings` table has at least one stored vector.
pub fn has_embeddings(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))?;
    Ok(n > 0)
}

/// Max characters of file content fed to the embedder (e5 context budget).
const EMBED_CHAR_LIMIT: usize = 8000;
/// Batch size cap for the embedding service (matches embedding-svc MAX=256).
const EMBED_BATCH: usize = 256;

/// Embed every indexed source file and store vectors in the `embeddings` table.
/// Files are sent with mode `index` (the service applies the e5 document prefix).
/// Returns the number of files embedded.
pub fn populate_embeddings(
    conn: &Connection,
    repo_root: &std::path::Path,
    embedder: &dyn Embedder,
) -> Result<usize> {
    let mut stmt = conn.prepare("SELECT id, path FROM files WHERE lang != 'unknown'")?;
    let files: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    let mut ids: Vec<i64> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    for (id, rel) in files {
        let content = match std::fs::read_to_string(repo_root.join(&rel)) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let truncated: String = content.chars().take(EMBED_CHAR_LIMIT).collect();
        ids.push(id);
        texts.push(truncated);
    }
    if ids.is_empty() {
        return Ok(0);
    }

    conn.execute_batch("BEGIN")?;
    let result = (|| -> Result<usize> {
        conn.execute_batch("DELETE FROM embeddings")?;
        let mut written = 0;
        for (id_chunk, text_chunk) in ids.chunks(EMBED_BATCH).zip(texts.chunks(EMBED_BATCH)) {
            let vecs = embedder.embed(text_chunk, EmbedMode::Index)?;
            for (id, v) in id_chunk.iter().zip(vecs.iter()) {
                conn.execute(
                    "INSERT OR REPLACE INTO embeddings (file_id, vector) VALUES (?1, ?2)",
                    params![id, vec_to_blob(v)],
                )?;
                written += 1;
            }
        }
        Ok(written)
    })();

    match result {
        Ok(n) => {
            conn.execute_batch("COMMIT")?;
            Ok(n)
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Cosine-rank stored file vectors against a query vector, best-first.
pub fn search_vectors(conn: &Connection, query: &[f32], top_n: usize) -> Result<Vec<(i64, f32)>> {
    let mut stmt = conn.prepare("SELECT file_id, vector FROM embeddings")?;
    let rows: Vec<(i64, Vec<u8>)> = stmt
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut scored: Vec<(i64, f32)> = rows
        .into_iter()
        .map(|(fid, blob)| (fid, cosine(query, &blob_to_vec(&blob))))
        .collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_n);
    Ok(scored)
}

/// Fuse BM25 seeds with vector-search seeds via RRF.
///
/// Embeds the task (prefixed `query: ` for e5), ranks stored vectors, and RRF-
/// fuses that ranking with `bm25`. If no embeddings are stored, returns the
/// BM25 seeds unchanged so the caller stays backward compatible.
pub fn fuse_seeds(
    conn: &Connection,
    embedder: &dyn Embedder,
    task: &str,
    bm25: &[(i64, f32)],
    top_n: usize,
) -> Result<Vec<(i64, f32)>> {
    if !has_embeddings(conn)? {
        return Ok(bm25.to_vec());
    }
    let query_vec = embedder
        .embed(&[task.to_string()], EmbedMode::Search)?
        .into_iter()
        .next()
        .unwrap_or_default();
    let vector = search_vectors(conn, &query_vec, top_n)?;
    Ok(rrf_fuse(&[bm25, &vector], 60.0, top_n))
}

/// HTTP embedding client for the MINIPC embedding-svc (e5, :9092).
///
/// API contract: `POST {url}/embed/batch` with
/// `{"collection": <str>, "texts": [..], "mode": "index"|"search"}` returning
/// `{"vectors": [[..], ..], "dim", "model", ...}`. Auth via `X-API-Key`.
///
/// Configure via env (aligned with the memory-ingest / search-engine ecosystem):
///   - `EMBEDDING_SVC_URL`     (required, e.g. http://192.0.2.10:9092)
///   - `EMBEDDING_API_KEY`     (optional, sent as `X-API-Key`)
///   - `EMBEDDING_COLLECTION`  (optional, default `context-smith`; must be a
///     collection registered on the service)
#[cfg(feature = "remote-embed")]
pub struct RemoteEmbedder {
    url: String,
    api_key: Option<String>,
    collection: String,
}

#[cfg(feature = "remote-embed")]
impl RemoteEmbedder {
    /// Construct from env; returns `None` if `EMBEDDING_SVC_URL` is unset.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("EMBEDDING_SVC_URL").ok()?;
        Some(RemoteEmbedder {
            url: url.trim_end_matches('/').to_string(),
            api_key: std::env::var("EMBEDDING_API_KEY").ok(),
            collection: std::env::var("EMBEDDING_COLLECTION")
                .unwrap_or_else(|_| "context-smith".to_string()),
        })
    }
}

#[cfg(feature = "remote-embed")]
impl Embedder for RemoteEmbedder {
    fn embed(&self, texts: &[String], mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
        #[derive(serde::Deserialize)]
        struct EmbedResponse {
            vectors: Vec<Vec<f32>>,
        }
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(EMBED_BATCH) {
            let mut req = ureq::post(&format!("{}/embed/batch", self.url));
            if let Some(key) = &self.api_key {
                req = req.set("X-API-Key", key);
            }
            let resp: EmbedResponse = req
                .send_json(ureq::json!({
                    "collection": self.collection,
                    "texts": chunk,
                    "mode": mode.as_str(),
                }))
                .map_err(|e| anyhow::anyhow!("embedding request failed: {e}"))?
                .into_json()
                .map_err(|e| anyhow::anyhow!("embedding response decode failed: {e}"))?;
            if resp.vectors.len() != chunk.len() {
                anyhow::bail!(
                    "embedding service returned {} vectors for {} texts",
                    resp.vectors.len(),
                    chunk.len()
                );
            }
            out.extend(resp.vectors);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Deterministic embedder: maps text length parity to two orthogonal-ish
    /// vectors so tests need no network.
    struct MockEmbedder;
    impl Embedder for MockEmbedder {
        fn embed(&self, texts: &[String], _mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
            Ok(texts
                .iter()
                .map(|t| {
                    if t.len() % 2 == 0 {
                        vec![1.0, 0.0, 0.0]
                    } else {
                        vec![0.0, 1.0, 0.0]
                    }
                })
                .collect())
        }
    }

    #[test]
    fn blob_roundtrip_preserves_vector() {
        let v = vec![1.5f32, -2.0, 0.0, 3.25];
        assert_eq!(blob_to_vec(&vec_to_blob(&v)), v);
    }

    #[test]
    fn cosine_identical_is_one_orthogonal_is_zero() {
        assert!((cosine(&[1.0, 2.0], &[1.0, 2.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn rrf_rewards_agreement_across_lists() {
        // File 2 is ranked highly by both lists; file 1 only by the first.
        let bm25 = vec![(1i64, 5.0f32), (2, 4.0)];
        let vec = vec![(2i64, 0.9f32), (3, 0.8)];
        let fused = rrf_fuse(&[&bm25, &vec], 60.0, 10);
        assert_eq!(fused[0].0, 2, "item in both lists should rank first");
        let ids: Vec<i64> = fused.iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&1) && ids.contains(&3));
    }

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT, lang TEXT);
             CREATE TABLE embeddings (file_id INTEGER PRIMARY KEY, vector BLOB NOT NULL);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn search_vectors_ranks_by_cosine() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO embeddings (file_id, vector) VALUES (1, ?1), (2, ?2)",
            rusqlite::params![vec_to_blob(&[1.0, 0.0]), vec_to_blob(&[0.0, 1.0])],
        )
        .unwrap();
        let hits = search_vectors(&conn, &[1.0, 0.1], 10).unwrap();
        assert_eq!(hits[0].0, 1, "closest vector should rank first");
    }

    #[test]
    fn fuse_seeds_returns_bm25_when_no_embeddings() {
        let conn = mem_db();
        let bm25 = vec![(7i64, 1.0f32)];
        let out = fuse_seeds(&conn, &MockEmbedder, "task", &bm25, 10).unwrap();
        assert_eq!(out, bm25, "no embeddings → BM25 seeds unchanged");
    }
}
