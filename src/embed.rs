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

/// Cosine similarity in [-1, 1]; returns 0.0 if either vector is zero-length
/// or if the lengths differ (a dimension mismatch is never silently truncated).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
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
/// the standard constant. Returns the top `top_n` file ids by fused score; equal
/// scores are ordered by ascending file id so the result is deterministic.
pub fn rrf_fuse(rankings: &[&[(i64, f32)]], k: f64, top_n: usize) -> Vec<(i64, f32)> {
    use std::collections::HashMap;
    let mut acc: HashMap<i64, f64> = HashMap::new();
    for ranking in rankings {
        for (rank, (fid, _)) in ranking.iter().enumerate() {
            *acc.entry(*fid).or_insert(0.0) += 1.0 / (k + (rank as f64) + 1.0);
        }
    }
    // Sort on the f64 sums with the file id as tie-break: HashMap iteration order is
    // random per run, so without it equal scores (common in RRF) would reorder and
    // the `top_n` cut would change between runs.
    let mut fused: Vec<(i64, f64)> = acc.into_iter().collect();
    fused.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    fused.truncate(top_n);
    fused.into_iter().map(|(id, s)| (id, s as f32)).collect()
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
    let canonical_root = repo_root.canonicalize()?;

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
            let content = match crate::read_within_root(&canonical_root, &abs_path) {
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
        if vecs.len() != texts.len() {
            anyhow::bail!(
                "embedder returned {} vectors for {} texts",
                vecs.len(),
                texts.len()
            );
        }

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
                crate::commit_or_rollback(conn)?;
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
        // Skip rows whose dimension differs from the query (stale/malformed
        // vectors) rather than ranking them with a meaningless score.
        if !blob.len().is_multiple_of(4) || blob.len() / 4 != query.len() {
            continue;
        }
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

/// Generate embeddings for the indexed files via the remote embedding service.
///
/// Returns `None` when `EMBEDDING_SVC_URL` is not configured (nothing was
/// attempted), otherwise the number of files embedded or the failure.
#[cfg(feature = "remote-embed")]
pub fn embed_index(
    repo: &crate::GitRepo,
    db: &crate::index_builder::IndexDb,
) -> Option<Result<usize>> {
    let embedder = RemoteEmbedder::from_env()?;
    Some(populate_embeddings(
        db.connection(),
        repo.root(),
        &embedder,
        db,
    ))
}

/// Fuse BM25 seeds with vector-search seeds via RRF.
///
/// Embeds the task (prefixed `query: ` for e5), ranks stored vectors, and RRF-
/// fuses that ranking with `bm25`. If no embeddings are stored, returns the
/// BM25 seeds unchanged so the caller stays backward compatible.
#[cfg(test)]
pub fn fuse_seeds(
    conn: &Connection,
    embedder: &dyn Embedder,
    task: &str,
    bm25: &[(i64, f32)],
    top_n: usize,
) -> Result<Vec<(i64, f32)>> {
    fuse_seeds_with_status(conn, embedder, task, bm25, top_n).map(|(seeds, _)| seeds)
}

/// Like [`fuse_seeds`], but also reports whether vector fusion actually ran
/// (`true`) or the BM25 seeds were returned unchanged (`false`: no stored
/// embeddings, empty query vector, or dimension mismatch / mixed dimensions).
pub fn fuse_seeds_with_status(
    conn: &Connection,
    embedder: &dyn Embedder,
    task: &str,
    bm25: &[(i64, f32)],
    top_n: usize,
) -> Result<(Vec<(i64, f32)>, bool)> {
    if !has_embeddings(conn)? {
        return Ok((bm25.to_vec(), false));
    }
    let query_vec = embedder
        .embed(&[task.to_string()], EmbedMode::Search)?
        .into_iter()
        .next()
        .unwrap_or_default();
    // An empty query vector, or one whose dimension differs from the stored
    // vectors (e.g. the model changed), would yield meaningless scores; fall
    // back to BM25 only. Stored vectors must all share one valid length
    // (multiple of 4 bytes) equal to the query dimension.
    let (distinct_lens, min_len): (i64, i64) = conn.query_row(
        "SELECT COUNT(DISTINCT length(vector)), MIN(length(vector)) FROM embeddings",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let consistent =
        distinct_lens == 1 && min_len % 4 == 0 && min_len / 4 == query_vec.len() as i64;
    if query_vec.is_empty() || !consistent {
        eprintln!("warn: query embedding empty or dimension mismatch; using BM25 seeds only");
        return Ok((bm25.to_vec(), false));
    }
    // Fetch top_n * 3 vector candidates before RRF so that BM25-high-ranked
    // files that sit just outside `top_n` in the vector list still participate
    // in fusion. The final result is still truncated to `top_n` by rrf_fuse.
    let vec_candidates = top_n.saturating_mul(3);
    let vector = search_vectors(conn, &query_vec, vec_candidates)?;
    Ok((rrf_fuse(&[bm25, &vector], 60.0, top_n), true))
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
///   - `EMBEDDING_ALLOW_INSECURE` (optional, `1` silences the warning printed for
///     plain `http://` to a non-loopback host; prefer `https://`)
///
/// Requests use a 5-second connect timeout and a configurable read timeout
/// (default 30 s). Transient errors (connection reset, 5xx, 408/425/429) are
/// retried; each chunk is attempted at most 3 times (2 retries, waiting 1 s then
/// 2 s). Other 4xx responses (e.g. 401/403/404) and 200 responses with an
/// undecodable body are not retried.
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

/// True when `url` is plain `http://` to a host that is not loopback, i.e. the
/// `X-API-Key` and the embedded source text would cross the network unencrypted.
#[cfg(feature = "remote-embed")]
fn is_insecure_remote(url: &str) -> bool {
    use std::net::{Ipv4Addr, Ipv6Addr};
    let url = url.trim();
    let Some(rest) = url
        .get(..7)
        .filter(|s| s.eq_ignore_ascii_case("http://"))
        .map(|_| &url[7..])
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let hostport = authority.rsplit('@').next().unwrap_or("");
    let host = match hostport.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => hostport.split(':').next().unwrap_or(""),
    };
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host.parse::<Ipv4Addr>().is_ok_and(|a| a.is_loopback())
        || host.parse::<Ipv6Addr>().is_ok_and(|a| a.is_loopback());
    !loopback
}

#[cfg(feature = "remote-embed")]
impl RemoteEmbedder {
    /// Construct from env; returns `None` if `EMBEDDING_SVC_URL` is unset.
    ///
    /// Plain `http://` to a non-loopback host prints a warning to stderr, since the
    /// API key and file contents would travel unencrypted. Prefer `https://`; set
    /// `EMBEDDING_ALLOW_INSECURE=1` to silence the warning for a trusted network.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("EMBEDDING_SVC_URL").ok()?;
        if is_insecure_remote(&url)
            && std::env::var("EMBEDDING_ALLOW_INSECURE").as_deref() != Ok("1")
        {
            eprintln!(
                "warning: EMBEDDING_SVC_URL uses plain http:// to a non-loopback host; \
                 the API key and file contents are sent unencrypted. Use https:// or set \
                 EMBEDDING_ALLOW_INSECURE=1 to silence this warning."
            );
        }
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
                        // A malformed 200 body is not transient: fail fast.
                        Err(e) => {
                            return Err(anyhow::anyhow!("embedding response decode failed: {e}"));
                        }
                    },
                    Err(e) => {
                        // Non-retryable client errors (4xx except 408/425/429): fail fast.
                        if let ureq::Error::Status(code, _) = &e {
                            if *code < 500 && !matches!(*code, 408 | 425 | 429) {
                                return Err(anyhow::anyhow!("embedding request failed: {e}"));
                            }
                        }
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
    fn rrf_ties_break_by_file_id_deterministically() {
        // Two opposite orderings make items i and 11-i tie exactly. Ties must come out
        // by ascending file id every time, not in HashMap iteration order.
        let fwd: Vec<(i64, f32)> = (1..=10).map(|i| (i, 0.0)).collect();
        let rev: Vec<(i64, f32)> = (1..=10).rev().map(|i| (i, 0.0)).collect();
        let expect: Vec<i64> = vec![1, 10, 2, 9, 3, 8, 4, 7, 5, 6];
        for _ in 0..50 {
            let fused = rrf_fuse(&[&fwd, &rev], 60.0, 10);
            let ids: Vec<i64> = fused.iter().map(|(id, _)| *id).collect();
            assert_eq!(ids, expect);
        }
        // The cut at top_n must not depend on iteration order either: of the tied
        // pair (2, 9) at the boundary, the lower id is kept.
        for _ in 0..50 {
            let ids: Vec<i64> = rrf_fuse(&[&fwd, &rev], 60.0, 3)
                .iter()
                .map(|(id, _)| *id)
                .collect();
            assert_eq!(ids, vec![1, 10, 2]);
        }
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

    #[test]
    fn search_vectors_excludes_dim_mismatched_rows() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO embeddings (file_id, vector) VALUES (1, ?1), (2, ?2)",
            rusqlite::params![vec_to_blob(&[-1.0, 0.0]), vec_to_blob(&[1.0, 0.0, 0.0])],
        )
        .unwrap();
        let hits = search_vectors(&conn, &[1.0, 0.0], 10).unwrap();
        let ids: Vec<i64> = hits.iter().map(|h| h.0).collect();
        assert_eq!(ids, vec![1], "mismatched row must not enter the ranking");
    }

    #[test]
    fn fuse_seeds_falls_back_on_mixed_stored_dims() {
        let conn = conn_with_two_dim_vectors();
        conn.execute(
            "INSERT INTO embeddings (file_id, vector) VALUES (3, ?1)",
            rusqlite::params![vec_to_blob(&[1.0, 0.0, 0.0])],
        )
        .unwrap();
        let bm25 = vec![(7i64, 1.0f32)];
        let out = fuse_seeds(&conn, &FixedEmbedder(vec![1.0, 0.0]), "task", &bm25, 10).unwrap();
        assert_eq!(out, bm25);
    }

    #[test]
    fn fuse_seeds_falls_back_on_malformed_blob() {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO embeddings (file_id, vector) VALUES (1, ?1)",
            rusqlite::params![vec![0u8; 9]],
        )
        .unwrap();
        let bm25 = vec![(7i64, 1.0f32)];
        let out = fuse_seeds(&conn, &FixedEmbedder(vec![0.0, 0.0]), "task", &bm25, 10).unwrap();
        assert_eq!(out, bm25);
    }

    #[test]
    fn fuse_seeds_with_status_reports_fusion() {
        let conn = conn_with_two_dim_vectors();
        let bm25 = vec![(7i64, 1.0f32)];
        let (_, fused) =
            fuse_seeds_with_status(&conn, &FixedEmbedder(vec![1.0, 0.0]), "t", &bm25, 10).unwrap();
        assert!(fused);
        let (out, fused) =
            fuse_seeds_with_status(&conn, &FixedEmbedder(vec![1.0]), "t", &bm25, 10).unwrap();
        assert!(!fused);
        assert_eq!(out, bm25);
    }

    #[test]
    fn cosine_length_mismatch_is_zero() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
        assert_eq!(cosine(&[], &[1.0]), 0.0);
    }

    /// Embedder returning a fixed vector (possibly empty) for every text.
    struct FixedEmbedder(Vec<f32>);
    impl Embedder for FixedEmbedder {
        fn embed(&self, texts: &[String], _mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| self.0.clone()).collect())
        }
    }

    /// Embedder that returns one vector too few.
    struct ShortEmbedder;
    impl Embedder for ShortEmbedder {
        fn embed(&self, texts: &[String], _mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().skip(1).map(|_| vec![1.0, 0.0]).collect())
        }
    }

    fn conn_with_two_dim_vectors() -> Connection {
        let conn = mem_db();
        conn.execute(
            "INSERT INTO embeddings (file_id, vector) VALUES (1, ?1), (2, ?2)",
            rusqlite::params![vec_to_blob(&[1.0, 0.0]), vec_to_blob(&[0.0, 1.0])],
        )
        .unwrap();
        conn
    }

    #[test]
    fn fuse_seeds_falls_back_to_bm25_on_empty_query_vector() {
        let conn = conn_with_two_dim_vectors();
        let bm25 = vec![(7i64, 1.0f32)];
        let out = fuse_seeds(&conn, &FixedEmbedder(vec![]), "task", &bm25, 10).unwrap();
        assert_eq!(out, bm25);
    }

    #[test]
    fn fuse_seeds_falls_back_to_bm25_on_dim_mismatch() {
        let conn = conn_with_two_dim_vectors();
        let bm25 = vec![(7i64, 1.0f32)];
        let out = fuse_seeds(
            &conn,
            &FixedEmbedder(vec![1.0, 0.0, 0.0]),
            "task",
            &bm25,
            10,
        )
        .unwrap();
        assert_eq!(out, bm25);
    }

    #[test]
    fn populate_embeddings_errors_on_count_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn b() {}").unwrap();
        let db = index_db_with_files(&[("a.rs", "sha-a"), ("b.rs", "sha-b")]);
        let r = populate_embeddings(db.connection(), dir.path(), &ShortEmbedder, &db);
        assert!(r.is_err(), "count mismatch must be an error");
        assert!(!has_embeddings(db.connection()).unwrap());
    }

    /// #231: a failing COMMIT (deferred FK violation raised by a trigger) must
    /// not leave the transaction open.
    #[test]
    fn populate_embeddings_rolls_back_when_commit_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let db = index_db_with_files(&[("a.rs", "sha-a")]);
        db.connection()
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE fk_parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE fk_child (r INTEGER REFERENCES fk_parent(id) DEFERRABLE INITIALLY DEFERRED);
                 CREATE TRIGGER defer_fk AFTER INSERT ON embeddings
                 BEGIN INSERT INTO fk_child VALUES (999); END;",
            )
            .unwrap();
        let r = populate_embeddings(db.connection(), dir.path(), &MockEmbedder, &db);
        assert!(r.is_err(), "COMMIT failure must surface as an error");
        assert!(db.connection().is_autocommit());
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

    #[cfg(feature = "remote-embed")]
    #[test]
    fn is_insecure_remote_flags_plain_http_to_non_loopback_only() {
        // Plain http to a non-loopback host sends the API key and source in clear text.
        for u in [
            "http://embedding-svc.local:9092",
            "http://10.0.0.5:9092/",
            "HTTP://example.com",
            "http://localhost.evil.example:80",
            "http://127.evil.example",
        ] {
            assert!(is_insecure_remote(u), "{u} should be flagged");
        }
        // https and loopback http are fine.
        for u in [
            "https://embedding-svc.example.com",
            "HTTPS://example.com:443",
            "http://localhost:9092",
            "http://127.0.0.1:9092",
            "http://127.1.2.3",
            "http://[::1]:9092",
            "http://user:pw@localhost:9092/x",
        ] {
            assert!(!is_insecure_remote(u), "{u} should not be flagged");
        }
    }

    /// Serve `status` for every request on a local port; returns (url, counter).
    #[cfg(feature = "remote-embed")]
    fn status_server(status: u16) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let count = std::sync::Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                // Read headers, then the body per Content-Length.
                loop {
                    let n = s.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).to_lowercase();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= h + 4 + len {
                            break;
                        }
                    }
                }
                c.fetch_add(1, Ordering::SeqCst);
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        });
        (url, count)
    }

    #[cfg(feature = "remote-embed")]
    fn remote_requests_for_status(status: u16) -> usize {
        let (url, count) = status_server(status);
        let e = RemoteEmbedder {
            url,
            api_key: None,
            collection: "t".into(),
            timeout: std::time::Duration::from_secs(5),
        };
        assert!(e.embed(&["x".to_string()], EmbedMode::Index).is_err());
        count.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn remote_embedder_does_not_retry_401() {
        assert_eq!(remote_requests_for_status(401), 1);
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn remote_embedder_retries_408() {
        assert_eq!(remote_requests_for_status(408), MAX_ATTEMPTS as usize);
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn remote_embedder_retries_503_and_429() {
        assert_eq!(remote_requests_for_status(503), MAX_ATTEMPTS as usize);
        assert_eq!(remote_requests_for_status(429), MAX_ATTEMPTS as usize);
    }

    /// Serve a 200 response with `body` for every request; returns (url, counter).
    #[cfg(feature = "remote-embed")]
    fn body_server(body: &'static str) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let count = std::sync::Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = Vec::new();
                let mut tmp = [0u8; 1024];
                loop {
                    let n = s.read(&mut tmp).unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    let text = String::from_utf8_lossy(&buf).to_lowercase();
                    if let Some(h) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= h + 4 + len {
                            break;
                        }
                    }
                }
                c.fetch_add(1, Ordering::SeqCst);
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
            }
        });
        (url, count)
    }

    #[cfg(feature = "remote-embed")]
    fn remote_for(url: String) -> RemoteEmbedder {
        RemoteEmbedder {
            url,
            api_key: None,
            collection: "t".into(),
            timeout: std::time::Duration::from_secs(5),
        }
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn remote_embedder_returns_vectors_on_valid_response() {
        let (url, count) = body_server(r#"{"vectors": [[1.0, 0.5]]}"#);
        let out = remote_for(url)
            .embed(&["x".to_string()], EmbedMode::Index)
            .unwrap();
        assert_eq!(out, vec![vec![1.0f32, 0.5]]);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn remote_embedder_does_not_retry_invalid_json() {
        let (url, count) = body_server("not json at all");
        let start = std::time::Instant::now();
        let r = remote_for(url).embed(&["x".to_string()], EmbedMode::Index);
        let msg = format!("{:#}", r.unwrap_err());
        assert!(msg.contains("decode failed"), "got: {msg}");
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(start.elapsed() < std::time::Duration::from_millis(900));
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn remote_embedder_errors_when_vector_count_differs_from_texts() {
        let (url, count) = body_server(r#"{"vectors": []}"#);
        let r = remote_for(url).embed(&["x".to_string()], EmbedMode::Index);
        assert!(r.is_err(), "count mismatch must be an error");
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Serializes tests that mutate process-wide `EMBEDDING_*` env vars.
    #[cfg(feature = "remote-embed")]
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(feature = "remote-embed")]
    const EMBED_ENV: [&str; 5] = [
        "EMBEDDING_SVC_URL",
        "EMBEDDING_API_KEY",
        "EMBEDDING_COLLECTION",
        "EMBEDDING_TIMEOUT_SECS",
        "EMBEDDING_ALLOW_INSECURE",
    ];

    #[cfg(feature = "remote-embed")]
    #[test]
    fn from_env_returns_none_when_url_unset() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for k in EMBED_ENV {
            std::env::remove_var(k);
        }
        assert!(RemoteEmbedder::from_env().is_none());
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn from_env_applies_defaults_and_trims_trailing_slash() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for k in EMBED_ENV {
            std::env::remove_var(k);
        }
        std::env::set_var("EMBEDDING_SVC_URL", "http://127.0.0.1:9092/");
        let e = RemoteEmbedder::from_env().expect("url set");
        std::env::remove_var("EMBEDDING_SVC_URL");
        assert_eq!(e.url, "http://127.0.0.1:9092");
        assert_eq!(e.collection, "context-smith");
        assert!(e.api_key.is_none());
        assert_eq!(
            e.timeout,
            std::time::Duration::from_secs(DEFAULT_READ_TIMEOUT_SECS)
        );
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn from_env_invalid_timeout_falls_back_to_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for k in EMBED_ENV {
            std::env::remove_var(k);
        }
        std::env::set_var("EMBEDDING_SVC_URL", "http://127.0.0.1:9092");
        std::env::set_var("EMBEDDING_TIMEOUT_SECS", "abc");
        let e = RemoteEmbedder::from_env().expect("url set");
        std::env::remove_var("EMBEDDING_SVC_URL");
        std::env::remove_var("EMBEDDING_TIMEOUT_SECS");
        assert_eq!(
            e.timeout,
            std::time::Duration::from_secs(DEFAULT_READ_TIMEOUT_SECS)
        );
    }

    // ---- rrf_fuse boundaries ----

    #[test]
    fn rrf_fuse_no_rankings_returns_empty() {
        assert!(rrf_fuse(&[], 60.0, 10).is_empty());
    }

    #[test]
    fn rrf_fuse_empty_lists_return_empty() {
        let empty: Vec<(i64, f32)> = vec![];
        assert!(rrf_fuse(&[&empty, &empty], 60.0, 10).is_empty());
    }

    #[test]
    fn rrf_fuse_top_n_zero_returns_empty() {
        let a = vec![(1i64, 1.0f32), (2, 0.5)];
        assert!(rrf_fuse(&[&a], 60.0, 0).is_empty());
    }

    #[test]
    fn rrf_fuse_k_zero_scores_by_reciprocal_rank() {
        let a = vec![(1i64, 0.0f32), (2, 0.0)];
        let out = rrf_fuse(&[&a], 0.0, 10);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, 1);
        assert!((out[0].1 - 1.0).abs() < 1e-6, "1/(0+1) = 1");
        assert!((out[1].1 - 0.5).abs() < 1e-6, "1/(0+2) = 0.5");
    }

    // ---- search_vectors boundaries ----

    #[test]
    fn search_vectors_empty_db_returns_empty() {
        let conn = mem_db();
        assert!(search_vectors(&conn, &[1.0, 0.0], 10).unwrap().is_empty());
    }

    #[test]
    fn search_vectors_top_n_zero_returns_empty() {
        let conn = conn_with_two_dim_vectors();
        assert!(search_vectors(&conn, &[1.0, 0.0], 0).unwrap().is_empty());
    }

    // ---- fuse_seeds with embeddings / embedder failure ----

    /// Embedder that always fails (service down).
    struct FailingEmbedder;
    impl Embedder for FailingEmbedder {
        fn embed(&self, _texts: &[String], _mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
            anyhow::bail!("embedding service unavailable")
        }
    }

    #[test]
    fn fuse_seeds_returns_err_when_embedder_fails_and_embeddings_exist() {
        let conn = conn_with_two_dim_vectors();
        let bm25 = vec![(7i64, 1.0f32)];
        let r = fuse_seeds(&conn, &FailingEmbedder, "task", &bm25, 10);
        assert!(r.is_err(), "caller relies on Err to fall back to BM25");
    }

    #[test]
    fn fuse_seeds_does_not_call_embedder_when_no_embeddings() {
        let conn = mem_db();
        let bm25 = vec![(7i64, 1.0f32)];
        let out = fuse_seeds(&conn, &FailingEmbedder, "task", &bm25, 10).unwrap();
        assert_eq!(out, bm25);
    }

    #[test]
    fn fuse_seeds_fuses_vector_hits_with_bm25_via_rrf() {
        let conn = conn_with_two_dim_vectors();
        // File 2 is ranked first by BM25 and 2nd by vectors; file 1 is first by vectors.
        let bm25 = vec![(2i64, 1.0f32), (7, 0.5)];
        let out = fuse_seeds(&conn, &FixedEmbedder(vec![1.0, 0.0]), "t", &bm25, 10).unwrap();
        let ids: Vec<i64> = out.iter().map(|(id, _)| *id).collect();
        // 2: 1/61 + 1/62, 1: 1/61, 7: 1/62.
        assert_eq!(ids, vec![2, 1, 7]);
    }

    #[test]
    fn fuse_seeds_truncates_fused_result_to_top_n() {
        let conn = conn_with_two_dim_vectors();
        let bm25 = vec![(2i64, 1.0f32), (7, 0.5)];
        let out = fuse_seeds(&conn, &FixedEmbedder(vec![1.0, 0.0]), "t", &bm25, 1).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, 2);
    }

    // ---- populate_embeddings failures ----

    #[test]
    fn populate_embeddings_propagates_embedder_error_and_stores_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let db = index_db_with_files(&[("a.rs", "sha-a")]);
        let r = populate_embeddings(db.connection(), dir.path(), &FailingEmbedder, &db);
        assert!(r.is_err());
        assert!(!has_embeddings(db.connection()).unwrap());
    }

    /// Embedder that succeeds for the first batch and fails afterwards.
    struct FailOnSecondCall(std::sync::atomic::AtomicUsize);
    impl Embedder for FailOnSecondCall {
        fn embed(&self, texts: &[String], _mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
            if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1 {
                anyhow::bail!("embedding service unavailable");
            }
            Ok(texts.iter().map(|_| vec![1.0, 0.0]).collect())
        }
    }

    #[test]
    fn populate_embeddings_second_batch_failure_returns_err_keeps_first_batch() {
        let dir = tempfile::tempdir().unwrap();
        let names: Vec<String> = (0..EMBED_BATCH + 1).map(|i| format!("f{i}.rs")).collect();
        for n in &names {
            std::fs::write(dir.path().join(n), "fn f() {}").unwrap();
        }
        let rows: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "sha")).collect();
        let db = index_db_with_files(&rows);
        let embedder = FailOnSecondCall(std::sync::atomic::AtomicUsize::new(0));
        let r = populate_embeddings(db.connection(), dir.path(), &embedder, &db);
        assert!(r.is_err(), "later batch failure must surface");
        let stored: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, EMBED_BATCH as i64, "first batch stays committed");
        // A retry only needs to embed the remaining file.
        let n = populate_embeddings(db.connection(), dir.path(), &MockEmbedder, &db).unwrap();
        assert_eq!(n, 1);
    }
}
