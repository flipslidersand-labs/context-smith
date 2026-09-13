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
    (0..b.len() / 4)
        .map(|i| f32::from_le_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]))
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
/// Batch size for incremental streaming — files are read and embedded in
/// chunks of this size so peak RSS stays bounded at `EMBED_BATCH × EMBED_CHAR_LIMIT`
/// (≈ 64 × 8 000 chars ≈ 512 KB of text) rather than growing with repo size.
const EMBED_BATCH: usize = 64;

/// Incrementally embed source files whose content SHA has changed and store
/// vectors in the `embeddings` table.
///
/// Only files where `embeddings.content_sha IS NULL OR content_sha != blob_sha`
/// are re-embedded; unchanged files are left untouched. Files are processed in
/// chunks of [`EMBED_BATCH`] to cap peak RAM usage. Returns the number of files
/// actually embedded (skipped files are not counted).
pub fn populate_embeddings(
    conn: &Connection,
    repo_root: &std::path::Path,
    embedder: &dyn Embedder,
    db: &crate::index_builder::IndexDb,
) -> Result<usize> {
    // Query only the files that actually need (re-)embedding — avoids the
    // full-table DELETE and re-insert of the previous implementation.
    let stale_files = db.files_needing_embed()?;
    if stale_files.is_empty() {
        return Ok(0);
    }

    let mut written = 0usize;

    // Stream through stale files in chunks so we never allocate more than
    // EMBED_BATCH file contents at once.
    for chunk in stale_files.chunks(EMBED_BATCH) {
        // Build (id, sha, text) triples, skipping unreadable files.
        let mut ids: Vec<i64> = Vec::with_capacity(chunk.len());
        let mut shas: Vec<String> = Vec::with_capacity(chunk.len());
        let mut texts: Vec<String> = Vec::with_capacity(chunk.len());

        for (id, rel, sha) in chunk {
            let abs_path = repo_root.join(rel);
            let content = match std::fs::read_to_string(&abs_path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("warn: skip {:?}: {e}", abs_path);
                    continue;
                }
            };
            let truncated: String = content.chars().take(EMBED_CHAR_LIMIT).collect();
            ids.push(*id);
            shas.push(sha.clone());
            texts.push(truncated);
        }

        if ids.is_empty() {
            continue;
        }

        // Embed this batch.
        let vecs = embedder.embed(&texts, EmbedMode::Index)?;

        // Persist each vector with its content_sha in a single transaction.
        conn.execute_batch("BEGIN")?;
        let result = (|| -> Result<()> {
            for ((id, sha), v) in ids.iter().zip(shas.iter()).zip(vecs.iter()) {
                conn.execute(
                    "INSERT INTO embeddings (file_id, vector, content_sha)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(file_id) DO UPDATE
                       SET vector      = excluded.vector,
                           content_sha = excluded.content_sha",
                    params![id, vec_to_blob(v), sha],
                )?;
            }
            Ok(())
        })();

        match result {
            Ok(()) => {
                conn.execute_batch("COMMIT")?;
                written += ids.len();
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
    }

    Ok(written)
}

/// Cosine-rank stored file vectors against a query vector, best-first.
///
/// Streams rows from the DB one at a time and maintains a min-heap of size
/// `top_n` to avoid loading the full `embeddings` table into RAM.
/// At 768 dims (e5) each vector is ~3 KiB; a 100 k-file repo would otherwise
/// consume ~300 MB in a single allocation. The heap keeps peak memory
/// proportional to `top_n`, not to the total number of stored embeddings.
///
/// **Upper bound**: `top_n` is capped internally at `SEARCH_VECTORS_MAX` so
/// callers cannot accidentally request unbounded results. Use
/// `fuse_seeds`, which sets `top_n * 3`, for higher-recall RRF pre-filtering.
pub fn search_vectors(conn: &Connection, query: &[f32], top_n: usize) -> Result<Vec<(i64, f32)>> {
    use std::cmp::Ordering;
    use std::collections::BinaryHeap;

    // Ordered by score ascending so the smallest score is always at the top
    // of the min-heap, making eviction O(log top_n) instead of O(n).
    #[derive(PartialEq)]
    struct MinEntry(f32, i64); // (score, file_id)
    impl Eq for MinEntry {}
    impl PartialOrd for MinEntry {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }
    impl Ord for MinEntry {
        fn cmp(&self, other: &Self) -> Ordering {
            // Reverse: smaller score → higher priority in BinaryHeap (min-heap).
            other
                .0
                .partial_cmp(&self.0)
                .unwrap_or(Ordering::Equal)
                .then(self.1.cmp(&other.1))
        }
    }

    // Cap `top_n` to prevent pathologically large heaps.
    let effective_n = top_n.min(SEARCH_VECTORS_MAX);
    let mut heap: BinaryHeap<MinEntry> = BinaryHeap::with_capacity(effective_n + 1);

    let mut stmt = conn.prepare("SELECT file_id, vector FROM embeddings")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let fid: i64 = row.get(0)?;
        let blob: Vec<u8> = row.get(1)?;
        let score = cosine(query, &blob_to_vec(&blob));
        heap.push(MinEntry(score, fid));
        if heap.len() > effective_n {
            heap.pop(); // evict the lowest-score entry
        }
    }

    // Drain into a vec and sort descending (best first).
    let mut scored: Vec<(i64, f32)> = heap.into_iter().map(|e| (e.1, e.0)).collect();
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Ok(scored)
}

/// Maximum number of results `search_vectors` will return.
///
/// This guards against callers requesting unbounded top-k results; at 768
/// dims each heap slot costs ~3 KiB so 3 000 entries ≈ 9 MB — well within
/// reason while still giving RRF plenty of candidates to fuse.
const SEARCH_VECTORS_MAX: usize = 3_000;

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
    // Fetch top_n * 3 vector candidates before RRF so that BM25-high-ranked
    // files that sit just outside `top_n` in the vector list still participate
    // in fusion. The final result is still truncated to `top_n` by rrf_fuse.
    let vec_candidates = top_n.saturating_mul(3);
    let vector = search_vectors(conn, &query_vec, vec_candidates)?;
    Ok(rrf_fuse(&[bm25, &vector], 60.0, top_n))
}

/// HTTP embedding client for the MINIPC embedding-svc (e5, :9092).
///
/// API contract: `POST {url}/embed/batch` with
/// `{"collection": <str>, "texts": [..], "mode": "index"|"search"}` returning
/// `{"vectors": [[..], ..], "dim", "model", ...}`. Auth via `X-API-Key`.
///
/// Configure via env (aligned with the memory-ingest / search-engine ecosystem):
///   - `EMBEDDING_SVC_URL`     (required, e.g. http://embedding-svc.local:9092)
///   - `EMBEDDING_API_KEY`     (optional, sent as `X-API-Key`)
///   - `EMBEDDING_COLLECTION`  (optional, default `context-smith`; must be a
///     collection registered on the service)
///   - `EMBEDDING_TIMEOUT_SECS` (optional, read timeout in seconds; default 30)
///
/// Requests use a 5-second connect timeout and a configurable read timeout
/// (default 30 s). Transient errors (connection reset, 5xx) are retried up to
/// 3 times with exponential back-off (1 s, 2 s, 4 s).
#[cfg(feature = "remote-embed")]
pub struct RemoteEmbedder {
    url: String,
    api_key: Option<zeroize::Zeroizing<String>>,
    collection: String,
    /// Read timeout applied to every HTTP request.
    timeout: std::time::Duration,
}

/// Connect timeout for the ureq agent (fixed).
#[cfg(feature = "remote-embed")]
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Default read timeout when `EMBEDDING_TIMEOUT_SECS` is not set.
#[cfg(feature = "remote-embed")]
const DEFAULT_READ_TIMEOUT_SECS: u64 = 30;
/// Maximum number of attempts per chunk (1 initial + 2 retries).
#[cfg(feature = "remote-embed")]
const MAX_ATTEMPTS: u32 = 3;

#[cfg(feature = "remote-embed")]
impl RemoteEmbedder {
    /// Construct from env; returns `None` if `EMBEDDING_SVC_URL` is unset.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("EMBEDDING_SVC_URL").ok()?;
        let timeout_secs = std::env::var("EMBEDDING_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_READ_TIMEOUT_SECS);
        Some(RemoteEmbedder {
            url: url.trim_end_matches('/').to_string(),
            api_key: std::env::var("EMBEDDING_API_KEY")
                .ok()
                .map(zeroize::Zeroizing::new),
            collection: std::env::var("EMBEDDING_COLLECTION")
                .unwrap_or_else(|_| "context-smith".to_string()),
            timeout: std::time::Duration::from_secs(timeout_secs),
        })
    }

    /// Build a ureq agent with the configured timeouts.
    fn agent(&self) -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(self.timeout)
            .build()
    }
}

#[cfg(feature = "remote-embed")]
impl Embedder for RemoteEmbedder {
    fn embed(&self, texts: &[String], mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
        #[derive(serde::Deserialize)]
        struct EmbedResponse {
            vectors: Vec<Vec<f32>>,
        }
        let agent = self.agent();
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(EMBED_BATCH) {
            let body = ureq::json!({
                "collection": self.collection,
                "texts": chunk,
                "mode": mode.as_str(),
            });

            // Retry loop with exponential back-off on transient failures.
            let mut last_err = anyhow::anyhow!("embedding request failed: no attempts made");
            let mut attempt = 0u32;
            let resp: EmbedResponse = loop {
                let mut req = agent.post(&format!("{}/embed/batch", self.url));
                if let Some(key) = &self.api_key {
                    req = req.set("X-API-Key", key.as_str());
                }
                match req.send_json(body.clone()) {
                    Ok(r) => match r.into_json::<EmbedResponse>() {
                        Ok(parsed) => break parsed,
                        Err(e) => {
                            last_err = anyhow::anyhow!("embedding response decode failed: {e}");
                        }
                    },
                    Err(e) => {
                        last_err = anyhow::anyhow!("embedding request failed: {e}");
                    }
                }
                attempt += 1;
                if attempt >= MAX_ATTEMPTS {
                    return Err(last_err);
                }
                std::thread::sleep(std::time::Duration::from_secs(1u64 << (attempt - 1)));
            };

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
            "CREATE TABLE files (
                 id INTEGER PRIMARY KEY, path TEXT, lang TEXT,
                 blob_sha TEXT NOT NULL DEFAULT '', indexed_sha TEXT, size INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE embeddings (
                 file_id INTEGER PRIMARY KEY,
                 vector  BLOB NOT NULL,
                 content_sha TEXT
             );",
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

    /// Build an in-memory [`crate::index_builder::IndexDb`] and populate it
    /// with `files` rows so `files_needing_embed` has something to return.
    fn index_db_with_files(file_rows: &[(&str, &str)]) -> crate::index_builder::IndexDb {
        let db = crate::index_builder::IndexDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        for (path, sha) in file_rows {
            db.connection()
                .execute(
                    "INSERT INTO files (path, lang, blob_sha, size) VALUES (?1, 'rust', ?2, 0)",
                    rusqlite::params![path, sha],
                )
                .unwrap();
        }
        db
    }

    #[test]
    fn populate_embeddings_skips_unchanged_files() {
        // Write two temp files so populate_embeddings can read them.
        let dir = tempfile::tempdir().unwrap();
        let path_a = dir.path().join("a.rs");
        let path_b = dir.path().join("b.rs");
        std::fs::write(&path_a, "fn a() {}").unwrap();
        std::fs::write(&path_b, "fn bb() {}").unwrap();

        let db = index_db_with_files(&[("a.rs", "sha-a"), ("b.rs", "sha-b")]);

        // First run: both files need embedding.
        let n = populate_embeddings(db.connection(), dir.path(), &MockEmbedder, &db).unwrap();
        assert_eq!(n, 2, "both files should be embedded on first run");

        // Second run: sha unchanged → nothing re-embedded.
        let n2 = populate_embeddings(db.connection(), dir.path(), &MockEmbedder, &db).unwrap();
        assert_eq!(n2, 0, "no files re-embedded when sha is unchanged");

        // Simulate a content change by updating blob_sha.
        db.connection()
            .execute(
                "UPDATE files SET blob_sha = 'sha-a-v2' WHERE path = 'a.rs'",
                [],
            )
            .unwrap();
        let n3 = populate_embeddings(db.connection(), dir.path(), &MockEmbedder, &db).unwrap();
        assert_eq!(n3, 1, "only the changed file should be re-embedded");
    }
}
