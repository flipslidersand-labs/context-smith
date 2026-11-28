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

    /// Identifier of the model behind this embedder. Stored in the index `meta`
    /// table; when it changes, every stored vector is recomputed.
    fn model_id(&self) -> String {
        "unknown".to_string()
    }
}

const EMBED_MODEL_KEY: &str = "embed_model";
const EMBED_DIM_KEY: &str = "embed_dim";

fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    crate::index_builder::IndexDb::get_meta_on(conn, key)
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
    // NaN sums (e.g. a NaN `k`) have no rank, so drop them before the total-order sort.
    let mut fused: Vec<(i64, f64)> = acc.into_iter().filter(|(_, s)| !s.is_nan()).collect();
    fused.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
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
/// are re-embedded; unchanged files are left untouched. The exception is a
/// change of embedder model or vector dimension (recorded in `meta` as
/// `embed_model` / `embed_dim`): all stored vectors are then recomputed. Files are processed in
/// chunks of [`EMBED_BATCH`] to cap peak RAM usage. Returns the number of files
/// actually embedded (skipped files are not counted).
pub fn populate_embeddings(
    repo_root: &std::path::Path,
    embedder: &dyn Embedder,
    db: &crate::index_builder::IndexDb,
) -> Result<usize> {
    let conn = db.connection();
    // BEGIN / INSERT / COMMIT can all hit SQLITE_BUSY when another process
    // holds the write lock; present the same friendly message as `build_index`.
    populate_embeddings_inner(repo_root, embedder, db)
        .map_err(|e| crate::index_builder::friendly_lock_error(conn, e))
}

fn populate_embeddings_inner(
    repo_root: &std::path::Path,
    embedder: &dyn Embedder,
    db: &crate::index_builder::IndexDb,
) -> Result<usize> {
    let conn = db.connection();
    // Query only the files that actually need (re-)embedding — avoids the
    // full-table DELETE and re-insert of the previous implementation.
    //
    // A different embedding model than the one that produced the stored vectors
    // invalidates all of them, even for files whose content is unchanged.
    let model = embedder.model_id();
    if has_embeddings(conn)? && get_meta(conn, EMBED_MODEL_KEY)?.as_deref() != Some(model.as_str())
    {
        conn.execute("DELETE FROM embeddings", [])?;
    }
    db.upsert_meta(EMBED_MODEL_KEY, &model)?;
    let stale_files = db.files_needing_embed()?;
    if stale_files.is_empty() {
        return Ok(0);
    }
    // Paths come from index.db, which may be untrusted (e.g. shipped in the
    // repo): only tracked files may be read and sent out (#447, #395).
    let git_repo = crate::GitRepo::new(repo_root)?;
    let git_index = git_repo.open_index()?;

    let mut written = 0usize;
    let mut run_dim: Option<usize> = None;

    // Stream through stale files in chunks so we never allocate more than
    // EMBED_BATCH file contents at once.
    for chunk in stale_files.chunks(EMBED_BATCH) {
        // Build (id, sha, text) triples, skipping unreadable files.
        let mut ids: Vec<i64> = Vec::with_capacity(chunk.len());
        let mut shas: Vec<String> = Vec::with_capacity(chunk.len());
        let mut texts: Vec<String> = Vec::with_capacity(chunk.len());

        for (id, rel, sha) in chunk {
            let abs_path = repo_root.join(rel);
            let content = match git_repo.read_indexed(&git_index, std::path::Path::new(rel)) {
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

        // A different vector dimension than the stored one means the existing
        // vectors are unusable: drop them and start over with the new dimension.
        let dim = vecs.first().map_or(0, |v| v.len());
        if vecs.iter().any(|v| v.len() != dim) {
            anyhow::bail!("embedder returned vectors of differing dimensions within one batch");
        }
        match run_dim {
            Some(d) if d != dim => anyhow::bail!(
                "embedding dimension changed mid-run ({d} -> {dim}); \
                 re-run to rebuild embeddings"
            ),
            _ => run_dim = Some(dim),
        }
        let stored_dim = get_meta(conn, EMBED_DIM_KEY)?;
        if written == 0
            && stored_dim.as_deref() != Some(dim.to_string().as_str())
            && has_embeddings(conn)?
        {
            conn.execute("DELETE FROM embeddings", [])?;
            db.upsert_meta(EMBED_DIM_KEY, &dim.to_string())?;
            return populate_embeddings_inner(repo_root, embedder, db);
        }

        // Persist each vector with its content_sha in a single transaction.
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Deferred)?;
        // An early `?` return drops `tx`, which rolls back.
        crate::index_builder::IndexDb::upsert_meta_on(conn, EMBED_DIM_KEY, &dim.to_string())?;
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
        tx.commit()?;
        written += ids.len();
    }

    Ok(written)
}

/// Cosine-rank stored file vectors against a query vector, best-first.
///
/// Streams rows from the DB one at a time and keeps only a `(file_id, score)` pair per row,
/// so the full `embeddings` table (~3 KiB per vector at 768 dims) is never loaded into RAM.
///
/// Vectors made from an older version of the file (`content_sha` differs from
/// `files.blob_sha`) are not ranked.
///
/// **Upper bound**: `top_n` is capped internally at `SEARCH_VECTORS_MAX` so
/// callers cannot accidentally request unbounded results. Use
/// `fuse_seeds`, which sets `top_n * 3`, for higher-recall RRF pre-filtering.
pub fn search_vectors(conn: &Connection, query: &[f32], top_n: usize) -> Result<Vec<(i64, f32)>> {
    // Cap `top_n` to prevent pathologically large results.
    let effective_n = top_n.min(SEARCH_VECTORS_MAX);
    let mut scored: Vec<(i64, f32)> = Vec::new();

    // A vector whose `content_sha` differs from the file's current `blob_sha`
    // was made from older text: leave it out until the next embed replaces it
    // (#617). A NULL `content_sha` (`--force` marks vectors stale but usable) or
    // a missing `files` row is not judged.
    let mut stmt = conn.prepare(
        "SELECT e.file_id, e.vector
         FROM embeddings e LEFT JOIN files f ON f.id = e.file_id
         WHERE e.content_sha IS NULL OR f.id IS NULL OR e.content_sha = f.blob_sha",
    )?;
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
        // A NaN score has no rank and would break the heap's total order; skip it.
        if score.is_nan() {
            continue;
        }
        scored.push((fid, score));
    }

    // Best first; ties go to the lower file_id.
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    scored.truncate(effective_n);
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
/// attempted), `Some(Err)` when the configuration is refused (plain `http://` to
/// a non-loopback host without `EMBEDDING_ALLOW_INSECURE=1`), otherwise the number of files embedded or the failure.
#[cfg(feature = "remote-embed")]
pub fn embed_index(
    repo: &crate::GitRepo,
    db: &crate::index_builder::IndexDb,
) -> Option<Result<usize>> {
    let embedder = match RemoteEmbedder::from_env() {
        Ok(Some(e)) => e,
        Ok(None) => return None,
        Err(e) => return Some(Err(e)),
    };
    Some(populate_embeddings(repo.root(), &embedder, db))
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
    fuse_seeds_with_status(conn, embedder, task, bm25, top_n, &|_| true).map(|(seeds, _)| seeds)
}

/// True if any component is NaN or infinite.
fn has_non_finite(v: &[f32]) -> bool {
    v.iter().any(|x| !x.is_finite())
}

/// Like [`fuse_seeds`], but also reports whether vector fusion actually ran
/// (`true`) or the BM25 seeds were returned unchanged (`false`: no stored
/// embeddings, empty query vector, or dimension mismatch / mixed dimensions).
///
/// `usable` says whether a file id may become a vector seed (readable and non-blank, #564);
/// ids it rejects are dropped from the vector ranking before fusion. `bm25` is taken as is.
pub fn fuse_seeds_with_status(
    conn: &Connection,
    embedder: &dyn Embedder,
    task: &str,
    bm25: &[(i64, f32)],
    top_n: usize,
    usable: &dyn Fn(i64) -> bool,
) -> Result<(Vec<(i64, f32)>, bool)> {
    if !has_embeddings(conn)? {
        return Ok((bm25.to_vec(), false));
    }
    // Same dimension but a different model means a different vector space; the
    // stored id is absent in older indexes, which keeps the previous behaviour.
    if let Some(stored) = get_meta(conn, EMBED_MODEL_KEY)? {
        let current = embedder.model_id();
        if stored != current {
            eprintln!(
                "warn: embeddings were built with model {stored:?} but the current model is \
                 {current:?}; using BM25 seeds only (re-run `contextsmith index`)"
            );
            return Ok((bm25.to_vec(), false));
        }
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
    // A NaN/inf query makes every cosine score NaN, and `search_vectors` drops
    // those rows, so fusion would be BM25 only anyway. Say so, and report the
    // mode as BM25 instead of Fused.
    if has_non_finite(&query_vec) {
        eprintln!("warn: query embedding contains non-finite values; using BM25 seeds only");
        return Ok((bm25.to_vec(), false));
    }
    // Fetch top_n * 3 vector candidates before RRF so that BM25-high-ranked
    // files that sit just outside `top_n` in the vector list still participate
    // in fusion. The final result is still truncated to `top_n` by rrf_fuse.
    let vec_candidates = top_n.saturating_mul(3);
    let mut vector = search_vectors(conn, &query_vec, vec_candidates)?;
    vector.retain(|&(id, _)| usable(id));
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
///   - `EMBEDDING_ALLOW_INSECURE` (optional; plain `http://` to a non-loopback
///     host is refused unless this is `1`, because the API key and file contents
///     would be sent unencrypted; prefer `https://`)
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
/// Upper bound on an embedding response body (same as ureq's `into_string`).
#[cfg(feature = "remote-embed")]
const MAX_RESPONSE_BYTES: u64 = 10 * 1024 * 1024;

/// Read the response body with a hard size cap, then parse it as JSON.
#[cfg(feature = "remote-embed")]
fn read_json_limited<T: serde::de::DeserializeOwned>(
    resp: ureq::http::Response<ureq::Body>,
) -> Result<T> {
    use std::io::Read;
    let mut buf = Vec::new();
    resp.into_body()
        .into_reader()
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > MAX_RESPONSE_BYTES {
        anyhow::bail!("response body too large (limit {MAX_RESPONSE_BYTES} bytes)");
    }
    Ok(serde_json::from_slice(&buf)?)
}

/// Replace the `user:pass@` part of every `scheme://user:pass@host` URL in `s`
/// with `***@`, so credentials never reach stderr or CI logs.
#[cfg(feature = "remote-embed")]
fn redact_url_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        let (head, tail) = rest.split_at(i + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
            .unwrap_or(tail.len());
        let (authority, after) = tail.split_at(end);
        match authority.rfind('@') {
            Some(at) => {
                out.push_str("***");
                out.push_str(&authority[at..]);
            }
            None => out.push_str(authority),
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

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
    /// Construct from env; returns `Ok(None)` if `EMBEDDING_SVC_URL` is unset, empty, or whitespace-only.
    ///
    /// Fails closed: plain `http://` to a non-loopback host returns an error (nothing
    /// is sent) because the API key and file contents would travel unencrypted.
    /// Prefer `https://`; set `EMBEDDING_ALLOW_INSECURE=1` to allow it on a trusted
    /// network (a warning is still printed to stderr).
    pub fn from_env() -> Result<Option<Self>> {
        let Ok(url) = std::env::var("EMBEDDING_SVC_URL") else {
            return Ok(None);
        };
        let url = url.trim().to_string();
        if url.is_empty() {
            return Ok(None);
        }
        if is_insecure_remote(&url) {
            if std::env::var("EMBEDDING_ALLOW_INSECURE").as_deref() != Ok("1") {
                anyhow::bail!(
                    "EMBEDDING_SVC_URL uses plain http:// to a non-loopback host; the API key \
                     and file contents would be sent unencrypted, so embeddings are disabled. \
                     Use https:// or set EMBEDDING_ALLOW_INSECURE=1 to allow it."
                );
            }
            eprintln!(
                "warning: EMBEDDING_SVC_URL uses plain http:// to a non-loopback host; \
                 the API key and file contents are sent unencrypted \
                 (allowed by EMBEDDING_ALLOW_INSECURE=1)."
            );
        }
        let timeout_secs = std::env::var("EMBEDDING_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_READ_TIMEOUT_SECS);
        Ok(Some(RemoteEmbedder {
            url: url.trim_end_matches('/').to_string(),
            api_key: std::env::var("EMBEDDING_API_KEY")
                .ok()
                .map(zeroize::Zeroizing::new),
            collection: std::env::var("EMBEDDING_COLLECTION")
                .unwrap_or_else(|_| "context-smith".to_string()),
            timeout: std::time::Duration::from_secs(timeout_secs),
        }))
    }

    /// Build a ureq agent with the configured timeouts.
    fn agent(&self) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(self.timeout))
            .timeout_recv_body(Some(self.timeout))
            // Never follow redirects: a 3xx would replay the request (and X-API-Key)
            // to an arbitrary host, possibly over plain http.
            .max_redirects(0)
            // ureq 3 reads ALL_PROXY/HTTPS_PROXY by default; ureq 2 did not. Keep the old
            // behavior so X-API-Key is not routed through an environment-chosen proxy.
            .proxy(None)
            .build()
            .into()
    }
}

#[cfg(feature = "remote-embed")]
impl Embedder for RemoteEmbedder {
    fn model_id(&self) -> String {
        // The service picks the model per collection.
        self.collection.clone()
    }

    fn embed(&self, texts: &[String], mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
        #[derive(serde::Deserialize)]
        struct EmbedResponse {
            vectors: Vec<Vec<f32>>,
        }
        let agent = self.agent();
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(EMBED_BATCH) {
            let body = serde_json::json!({
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
                    req = req.header("X-API-Key", key.as_str());
                }
                match req.send_json(body.clone()) {
                    // `.max_redirects(0)` makes ureq return 3xx as Ok: fail fast with a clear cause.
                    Ok(r) if r.status().is_redirection() => {
                        let loc = r
                            .headers()
                            .get("Location")
                            .and_then(|l| l.to_str().ok())
                            .map(|l| format!(" (Location: {})", redact_url_userinfo(l)))
                            .unwrap_or_default();
                        return Err(anyhow::anyhow!(
                            "embedding request failed: HTTP {} redirect not followed{loc}; \
                             point EMBEDDING_SVC_URL at the final URL",
                            r.status().as_u16()
                        ));
                    }
                    Ok(r) => match read_json_limited::<EmbedResponse>(r) {
                        Ok(parsed) => break parsed,
                        // A malformed 200 body is not transient: fail fast.
                        Err(e) => {
                            return Err(anyhow::anyhow!(
                                "embedding response decode failed: {}",
                                redact_url_userinfo(&e.to_string())
                            ));
                        }
                    },
                    Err(e) => {
                        // Non-retryable client errors (4xx except 408/425/429): fail fast.
                        if let ureq::Error::StatusCode(code) = &e {
                            if *code < 500 && !matches!(*code, 408 | 425 | 429) {
                                return Err(anyhow::anyhow!(
                                    "embedding request failed: {}",
                                    redact_url_userinfo(&e.to_string())
                                ));
                            }
                        }
                        last_err = anyhow::anyhow!(
                            "embedding request failed: {}",
                            redact_url_userinfo(&e.to_string())
                        );
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
             );
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
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

    /// #617: a vector made from an older version of the file (its `content_sha`
    /// differs from the file's current `blob_sha`) must not be ranked. Rows with
    /// no `content_sha` (marked stale by `--force`, still usable until replaced)
    /// and rows without a `files` row are not judged here.
    #[test]
    fn search_vectors_skips_vector_of_edited_file() {
        let conn = mem_db();
        conn.execute_batch(
            "INSERT INTO files (id, path, lang, blob_sha) VALUES
                 (1, 'edited.rs', 'rust', 'new'),
                 (2, 'same.rs', 'rust', 'same'),
                 (3, 'forced.rs', 'rust', 'x');",
        )
        .unwrap();
        // The stale vector of file 1 is the closest to the query.
        for (id, v, sha) in [
            (1i64, [1.0f32, 0.0], Some("old")),
            (2, [0.8, 0.6], Some("same")),
            (3, [0.6, 0.8], None),
            (4, [0.0, 1.0], Some("no-files-row")),
        ] {
            conn.execute(
                "INSERT INTO embeddings (file_id, vector, content_sha) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, vec_to_blob(&v), sha],
            )
            .unwrap();
        }
        let ids: Vec<i64> = search_vectors(&conn, &[1.0, 0.0], 10)
            .unwrap()
            .iter()
            .map(|h| h.0)
            .collect();
        assert_eq!(
            ids,
            vec![2, 3, 4],
            "file 1's vector is from before its edit"
        );
    }

    #[test]
    fn search_vectors_nan_vector_does_not_panic_or_lead() {
        let conn = mem_db();
        // Many NaN rows plus finite rows: NaN scores must be dropped, not ranked.
        for id in 1..=40i64 {
            let v = if id % 2 == 0 {
                [f32::NAN, f32::NAN]
            } else {
                [1.0, id as f32 / 100.0]
            };
            conn.execute(
                "INSERT INTO embeddings (file_id, vector) VALUES (?1, ?2)",
                rusqlite::params![id, vec_to_blob(&v)],
            )
            .unwrap();
        }
        let hits = search_vectors(&conn, &[1.0, 0.0], 5).unwrap();
        // Finite rows are the odd ids; cosine falls as id grows, so the exact
        // top 5 are 1,3,5,7,9 and no NaN row (even id) may appear or lead.
        let ids: Vec<i64> = hits.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![1, 3, 5, 7, 9]);
        assert!(hits.iter().all(|(_, s)| !s.is_nan()));
    }

    #[test]
    fn search_vectors_ties_break_by_file_id() {
        let conn = mem_db();
        // Identical vectors => identical scores; insert in scrambled order.
        for id in [7i64, 3, 9, 1, 5, 8, 2, 6, 4, 10] {
            conn.execute(
                "INSERT INTO embeddings (file_id, vector) VALUES (?1, ?2)",
                rusqlite::params![id, vec_to_blob(&[1.0, 0.0])],
            )
            .unwrap();
        }
        for _ in 0..20 {
            let all: Vec<i64> = search_vectors(&conn, &[1.0, 0.0], 10)
                .unwrap()
                .iter()
                .map(|(id, _)| *id)
                .collect();
            assert_eq!(all, (1..=10).collect::<Vec<i64>>());
            // The cut at top_n keeps the lowest ids among ties.
            let cut: Vec<i64> = search_vectors(&conn, &[1.0, 0.0], 4)
                .unwrap()
                .iter()
                .map(|(id, _)| *id)
                .collect();
            assert_eq!(cut, vec![1, 2, 3, 4]);
        }
    }

    /// Ties, NaN rows and the `SEARCH_VECTORS_MAX` cap together: the result is the first
    /// `top_n` of the non-NaN rows ordered by (score desc, file_id asc).
    #[test]
    fn search_vectors_orders_ties_nan_and_cap_like_a_full_sort() {
        let conn = mem_db();
        let total = 5_000i64;
        let tx = conn.unchecked_transaction().unwrap();
        for id in 1..=total {
            // Score classes by `id % 4` (class 0 is the best); every third row is NaN.
            let v = if id % 3 == 0 {
                [f32::NAN, f32::NAN]
            } else {
                [1.0, (id % 4) as f32]
            };
            conn.execute(
                "INSERT INTO embeddings (file_id, vector) VALUES (?1, ?2)",
                rusqlite::params![id, vec_to_blob(&v)],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        let mut want: Vec<i64> = (1..=total).filter(|id| id % 3 != 0).collect();
        want.sort_by_key(|&id| (id % 4, id));
        for n in [0usize, 1, 7, 100, 1_000, 3_000, 3_001, usize::MAX] {
            let got = search_vectors(&conn, &[1.0, 0.0], n).unwrap();
            let ids: Vec<i64> = got.iter().map(|(id, _)| *id).collect();
            let take = n.min(SEARCH_VECTORS_MAX).min(want.len());
            assert_eq!(ids, want[..take], "top_n={n}");
            assert!(got.windows(2).all(|w| w[0].1 >= w[1].1), "top_n={n}");
        }
    }

    #[test]
    fn rrf_fuse_nan_k_does_not_panic() {
        let r: Vec<(i64, f32)> = (1..=30).map(|i| (i, 1.0)).collect();
        let out = rrf_fuse(&[&r], f64::NAN, 10);
        assert!(out.is_empty(), "NaN-scored entries are dropped");
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
        let (_, fused) = fuse_seeds_with_status(
            &conn,
            &FixedEmbedder(vec![1.0, 0.0]),
            "t",
            &bm25,
            10,
            &|_| true,
        )
        .unwrap();
        assert!(fused);
        let (out, fused) =
            fuse_seeds_with_status(&conn, &FixedEmbedder(vec![1.0]), "t", &bm25, 10, &|_| true)
                .unwrap();
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
    fn fuse_seeds_drops_vector_seeds_rejected_by_usable() {
        // #564: id 1 is the best vector match but is blank/unreadable, so it must not be a seed.
        let conn = conn_with_two_dim_vectors();
        let bm25 = vec![(7i64, 1.0f32)];
        let (out, fused) = fuse_seeds_with_status(
            &conn,
            &FixedEmbedder(vec![1.0, 0.0]),
            "t",
            &bm25,
            10,
            &|id| id != 1,
        )
        .unwrap();
        assert!(fused);
        let ids: Vec<i64> = out.iter().map(|&(id, _)| id).collect();
        assert!(!ids.contains(&1), "{ids:?}");
        assert!(ids.contains(&2) && ids.contains(&7), "{ids:?}");
    }

    #[test]
    fn has_non_finite_detects_nan_and_inf() {
        assert!(has_non_finite(&[1.0, f32::NAN]));
        assert!(has_non_finite(&[f32::INFINITY, 0.0]));
        assert!(has_non_finite(&[f32::NEG_INFINITY]));
        assert!(!has_non_finite(&[1.0, -2.0, 0.0]));
        assert!(!has_non_finite(&[]));
    }

    #[test]
    fn fuse_seeds_nan_query_keeps_bm25_only_ranking() {
        let conn = conn_with_two_dim_vectors();
        let bm25 = vec![(7i64, 2.0f32), (8, 1.0)];
        let (out, fused) = fuse_seeds_with_status(
            &conn,
            &FixedEmbedder(vec![f32::NAN, 0.0]),
            "task",
            &bm25,
            10,
            &|_| true,
        )
        .unwrap();
        assert_eq!(out, bm25);
        assert!(
            !fused,
            "a non-finite query must report BM25 mode, not Fused"
        );
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
        track_all(dir.path());
        let db = index_db_with_files(&[("a.rs", "sha-a"), ("b.rs", "sha-b")]);
        let r = populate_embeddings(dir.path(), &ShortEmbedder, &db);
        assert!(r.is_err(), "count mismatch must be an error");
        assert!(!has_embeddings(db.connection()).unwrap());
    }

    /// #231: a failing COMMIT (deferred FK violation raised by a trigger) must
    /// not leave the transaction open.
    #[test]
    fn populate_embeddings_rolls_back_when_commit_fails() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        track_all(dir.path());
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
        let r = populate_embeddings(dir.path(), &MockEmbedder, &db);
        assert!(r.is_err(), "COMMIT failure must surface as an error");
        assert!(db.connection().is_autocommit());
    }

    /// #303: a lock held by another connection must produce the friendly
    /// "locked by another process" message, not the raw SQLite text.
    #[test]
    fn populate_embeddings_reports_lock_when_another_connection_writes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        track_all(dir.path());
        let dbdir = tempfile::tempdir().unwrap();
        let path = dbdir.path().join("idx.db");
        {
            let db = crate::index_builder::IndexDb::open(&path).unwrap();
            db.init_schema().unwrap();
            db.connection()
                .execute(
                    "INSERT INTO files (path, lang, blob_sha, size) VALUES ('a.rs', 'rust', 'sha-a', 0)",
                    [],
                )
                .unwrap();
        }
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let db = crate::index_builder::IndexDb::open_unchecked_for_test(&path).unwrap();
        db.connection()
            .busy_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        let err = populate_embeddings(dir.path(), &MockEmbedder, &db)
            .unwrap_err()
            .to_string();
        assert!(err.contains("locked by another process"), "{err}");
        assert!(db.connection().is_autocommit());
        drop(writer);
    }

    /// `git init` `dir` and stage every file currently in it (no commit needed:
    /// embedding only requires a stage-0 index entry).
    fn track_all(dir: &std::path::Path) {
        let repo = git2::Repository::init(dir).unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_all(["*"], git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        idx.write().unwrap();
    }

    /// Records every text it is asked to embed.
    struct RecordingEmbedder(std::sync::Mutex<Vec<String>>);
    impl Embedder for RecordingEmbedder {
        fn embed(&self, texts: &[String], _mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
            self.0.lock().unwrap().extend(texts.iter().cloned());
            Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
        }
    }

    /// #447: a path listed in index.db but not tracked by git (e.g. `.env`) must
    /// never be read and sent to the embedding service.
    #[test]
    fn populate_embeddings_skips_untracked_files_listed_in_db() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        track_all(dir.path());
        std::fs::write(dir.path().join(".env"), "TOKEN=fake-secret-value").unwrap();
        let db = index_db_with_files(&[("a.rs", "sha-a"), (".env", "sha-env")]);
        let rec = RecordingEmbedder(Default::default());
        let n = populate_embeddings(dir.path(), &rec, &db).unwrap();
        assert_eq!(n, 1);
        let sent = rec.0.lock().unwrap().join("\n");
        assert!(sent.contains("fn a()"), "{sent}");
        assert!(!sent.contains("fake-secret-value"), "{sent}");
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
        track_all(dir.path());

        let db = index_db_with_files(&[("a.rs", "sha-a"), ("b.rs", "sha-b")]);

        // First run: both files need embedding.
        let n = populate_embeddings(dir.path(), &MockEmbedder, &db).unwrap();
        assert_eq!(n, 2, "both files should be embedded on first run");

        // Second run: sha unchanged → nothing re-embedded.
        let n2 = populate_embeddings(dir.path(), &MockEmbedder, &db).unwrap();
        assert_eq!(n2, 0, "no files re-embedded when sha is unchanged");

        // Simulate a content change by updating blob_sha.
        db.connection()
            .execute(
                "UPDATE files SET blob_sha = 'sha-a-v2' WHERE path = 'a.rs'",
                [],
            )
            .unwrap();
        let n3 = populate_embeddings(dir.path(), &MockEmbedder, &db).unwrap();
        assert_eq!(n3, 1, "only the changed file should be re-embedded");
    }

    struct IdEmbedder(&'static str, usize);
    impl Embedder for IdEmbedder {
        fn embed(&self, texts: &[String], _mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![1.0; self.1]).collect())
        }
        fn model_id(&self) -> String {
            self.0.to_string()
        }
    }

    fn two_file_db() -> (tempfile::TempDir, crate::index_builder::IndexDb) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn bb() {}").unwrap();
        track_all(dir.path());
        let db = index_db_with_files(&[("a.rs", "sha-a"), ("b.rs", "sha-b")]);
        (dir, db)
    }

    #[test]
    fn populate_embeddings_reembeds_all_when_model_changes() {
        let (dir, db) = two_file_db();
        let c = db.connection();
        assert_eq!(
            populate_embeddings(dir.path(), &IdEmbedder("m1", 4), &db).unwrap(),
            2
        );
        assert_eq!(
            populate_embeddings(dir.path(), &IdEmbedder("m1", 4), &db).unwrap(),
            0
        );
        assert_eq!(
            populate_embeddings(dir.path(), &IdEmbedder("m2", 4), &db).unwrap(),
            2
        );
        assert_eq!(get_meta(c, EMBED_MODEL_KEY).unwrap().as_deref(), Some("m2"));
        assert_eq!(
            populate_embeddings(dir.path(), &IdEmbedder("m2", 4), &db).unwrap(),
            0
        );
    }

    #[test]
    fn populate_embeddings_reembeds_all_when_dim_changes() {
        let (dir, db) = two_file_db();
        let c = db.connection();
        assert_eq!(
            populate_embeddings(dir.path(), &IdEmbedder("m1", 4), &db).unwrap(),
            2
        );
        assert_eq!(get_meta(c, EMBED_DIM_KEY).unwrap().as_deref(), Some("4"));
        // Same model name, new dimension. The dimension is only observable when
        // the embedder is called, so one file changes to trigger a call.
        c.execute(
            "UPDATE files SET blob_sha = 'sha-a2' WHERE path = 'a.rs'",
            [],
        )
        .unwrap();
        assert_eq!(
            populate_embeddings(dir.path(), &IdEmbedder("m1", 8), &db).unwrap(),
            2
        );
        assert_eq!(get_meta(c, EMBED_DIM_KEY).unwrap().as_deref(), Some("8"));
        let lens: Vec<i64> = c
            .prepare("SELECT length(vector) FROM embeddings")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(lens, vec![32, 32]);
        assert_eq!(
            populate_embeddings(dir.path(), &IdEmbedder("m1", 8), &db).unwrap(),
            0
        );
    }

    /// Returns `dims[i]`-long vectors for call `i` (last entry repeats); with `mixed`, the
    /// first vector of every batch gets the first dim and the rest the second.
    struct DimsEmbedder {
        dims: Vec<usize>,
        mixed: bool,
        calls: std::sync::atomic::AtomicUsize,
    }
    impl Embedder for DimsEmbedder {
        fn embed(&self, texts: &[String], _mode: EmbedMode) -> Result<Vec<Vec<f32>>> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(n < 10, "embedder called {n} times: unbounded recursion?");
            let d = self.dims[n.min(self.dims.len() - 1)];
            Ok(texts
                .iter()
                .enumerate()
                .map(|(i, _)| vec![1.0; if self.mixed && i > 0 { d + 4 } else { d }])
                .collect())
        }
    }

    #[test]
    fn populate_embeddings_rejects_mixed_dims_within_batch() {
        let (dir, db) = two_file_db();
        let e = DimsEmbedder {
            dims: vec![4],
            mixed: true,
            calls: Default::default(),
        };
        let err = populate_embeddings(dir.path(), &e, &db).unwrap_err();
        assert!(err.to_string().contains("dimension"), "{err}");
        assert!(!has_embeddings(db.connection()).unwrap());
    }

    #[test]
    fn populate_embeddings_rejects_dim_change_between_batches() {
        let dir = tempfile::tempdir().unwrap();
        let names: Vec<String> = (0..EMBED_BATCH + 1)
            .map(|i| format!("f{i:03}.rs"))
            .collect();
        for n in &names {
            std::fs::write(dir.path().join(n), "fn a() {}").unwrap();
        }
        track_all(dir.path());
        let rows: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "sha")).collect();
        let db = index_db_with_files(&rows);
        let e = DimsEmbedder {
            dims: vec![4, 8],
            mixed: false,
            calls: Default::default(),
        };
        let err = populate_embeddings(dir.path(), &e, &db).unwrap_err();
        assert!(err.to_string().contains("dimension"), "{err}");
        // The committed first batch is kept, not deleted.
        let n: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n as usize, EMBED_BATCH);
    }

    #[test]
    fn fuse_seeds_falls_back_when_stored_model_differs() {
        let conn = conn_with_two_dim_vectors();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, 'old-model')",
            [EMBED_MODEL_KEY],
        )
        .unwrap();
        let bm25 = vec![(7i64, 1.0f32)];
        let other = IdEmbedder("new-model", 2);
        let (out, fused) =
            fuse_seeds_with_status(&conn, &other, "task", &bm25, 10, &|_| true).unwrap();
        assert_eq!(out, bm25);
        assert!(!fused);
        // Same model id (and no stored id) keep fusing.
        let same = IdEmbedder("old-model", 2);
        assert!(
            fuse_seeds_with_status(&conn, &same, "task", &bm25, 10, &|_| true)
                .unwrap()
                .1
        );
        conn.execute("DELETE FROM meta WHERE key = ?1", [EMBED_MODEL_KEY])
            .unwrap();
        assert!(
            fuse_seeds_with_status(&conn, &other, "task", &bm25, 10, &|_| true)
                .unwrap()
                .1
        );
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
    fn redact_url_userinfo_hides_credentials() {
        assert_eq!(
            redact_url_userinfo("http://user:pw@host:80/embed/batch: boom"),
            "http://***@host:80/embed/batch: boom"
        );
        assert_eq!(
            redact_url_userinfo("see https://u@h/x?a=b@c"),
            "see https://***@h/x?a=b@c"
        );
        assert_eq!(redact_url_userinfo("http://host/a@b"), "http://host/a@b");
        assert_eq!(redact_url_userinfo("no url here"), "no url here");
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn redact_url_userinfo_handles_delimiter_chars_in_password() {
        for pw in ["p)w", "p'w", "p\"w", "p>w", "p@w", "p(w"] {
            let out = redact_url_userinfo(&format!("http://u:{pw}@host/x"));
            assert_eq!(out, "http://***@host/x", "pw: {pw}");
        }
        // URL wrapped in brackets/quotes: only the userinfo is replaced.
        assert_eq!(
            redact_url_userinfo("failed (http://user:pw@host)"),
            "failed (http://***@host)"
        );
        assert_eq!(
            redact_url_userinfo("failed 'http://user:pw@host:9/embed/batch'"),
            "failed 'http://***@host:9/embed/batch'"
        );
        // No userinfo, IPv6, no scheme: unchanged.
        assert_eq!(redact_url_userinfo("(http://host)"), "(http://host)");
        assert_eq!(
            redact_url_userinfo("http://[::1]:80/x"),
            "http://[::1]:80/x"
        );
        assert_eq!(
            redact_url_userinfo("http://u:pw@[::1]:80/x"),
            "http://***@[::1]:80/x"
        );
        assert_eq!(redact_url_userinfo("user:pw@host"), "user:pw@host");
        // Two URLs in one message.
        assert_eq!(
            redact_url_userinfo("http://a:b@h1/x -> http://c:d@h2/y"),
            "http://***@h1/x -> http://***@h2/y"
        );
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn remote_embedder_redirect_error_hides_location_userinfo() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = write!(
                    s,
                    "HTTP/1.1 302 Found\r\nLocation: http://u:p)w@other/embed\r\n\
                     Content-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        });
        let r = remote_for(url).embed(&["x".to_string()], EmbedMode::Index);
        let msg = format!("{:#}", r.unwrap_err());
        assert!(msg.contains("redirect not followed"), "got: {msg}");
        assert!(!msg.contains("p)w"), "got: {msg}");
        assert!(msg.contains("http://***@other/embed"), "got: {msg}");
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn remote_embedder_error_hides_url_userinfo() {
        let (url, _count) = status_server(404);
        let url = url.replacen("http://", "http://user:pw@", 1);
        let r = remote_for(url).embed(&["x".to_string()], EmbedMode::Index);
        let msg = format!("{:#}", r.unwrap_err());
        assert!(!msg.contains("user:pw"), "got: {msg}");
        assert!(!msg.contains("pw@"), "got: {msg}");
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

    /// Server that records the raw (lowercased) text of every request it receives.
    #[cfg(feature = "remote-embed")]
    fn recording_server() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let c = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap_or(0);
                c.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_lowercase());
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{{\"vectors\":[]}}"
                );
            }
        });
        (url, seen)
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn remote_embedder_does_not_follow_redirects_or_leak_api_key() {
        use std::io::{Read, Write};
        let (target_url, seen) = recording_server();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let _ = write!(
                    s,
                    "HTTP/1.1 302 Found\r\nLocation: {target_url}/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
            }
        });
        let mut e = remote_for(url);
        e.api_key = Some(zeroize::Zeroizing::new("sekret-test-key".to_string()));
        let r = e.embed(&["x".to_string()], EmbedMode::Index);
        let msg = format!("{:#}", r.expect_err("a redirect response must be an error"));
        assert!(
            msg.contains("redirect") && msg.contains("Location: "),
            "got: {msg}"
        );
        assert!(msg.contains("/steal"), "got: {msg}");
        assert!(!msg.contains("sekret-test-key"), "got: {msg}");
        assert!(
            seen.lock().unwrap().is_empty(),
            "redirect target must receive no request: {:?}",
            seen.lock().unwrap()
        );
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn remote_embedder_rejects_oversized_response_body() {
        let pad = "a".repeat(MAX_RESPONSE_BYTES as usize + 1024);
        let body: &'static str =
            Box::leak(format!(r#"{{"vectors": [[1.0]], "pad": "{pad}"}}"#).into_boxed_str());
        let (url, count) = body_server(body);
        let r = remote_for(url).embed(&["x".to_string()], EmbedMode::Index);
        let msg = format!("{:#}", r.expect_err("oversized body must be rejected"));
        assert!(msg.contains("too large"), "got: {msg}");
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
        assert!(RemoteEmbedder::from_env().unwrap().is_none());
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn from_env_returns_none_when_url_empty_or_blank() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for k in EMBED_ENV {
            std::env::remove_var(k);
        }
        for v in ["", "   ", "\t\n"] {
            std::env::set_var("EMBEDDING_SVC_URL", v);
            let got = RemoteEmbedder::from_env().unwrap();
            std::env::remove_var("EMBEDDING_SVC_URL");
            assert!(got.is_none(), "{v:?} must be treated as unset");
        }
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn from_env_trims_surrounding_whitespace_in_url() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for k in EMBED_ENV {
            std::env::remove_var(k);
        }
        std::env::set_var("EMBEDDING_SVC_URL", "  http://127.0.0.1:9092/ ");
        let e = RemoteEmbedder::from_env().unwrap().expect("url set");
        std::env::remove_var("EMBEDDING_SVC_URL");
        assert_eq!(e.url, "http://127.0.0.1:9092");
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn from_env_applies_defaults_and_trims_trailing_slash() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for k in EMBED_ENV {
            std::env::remove_var(k);
        }
        std::env::set_var("EMBEDDING_SVC_URL", "http://127.0.0.1:9092/");
        let e = RemoteEmbedder::from_env().unwrap().expect("url set");
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
        let e = RemoteEmbedder::from_env().unwrap().expect("url set");
        std::env::remove_var("EMBEDDING_SVC_URL");
        std::env::remove_var("EMBEDDING_TIMEOUT_SECS");
        assert_eq!(
            e.timeout,
            std::time::Duration::from_secs(DEFAULT_READ_TIMEOUT_SECS)
        );
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn from_env_refuses_insecure_http_unless_allowed() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for k in EMBED_ENV {
            std::env::remove_var(k);
        }
        std::env::set_var("EMBEDDING_SVC_URL", "http://embedding-svc.example:9092");
        std::env::set_var("EMBEDDING_API_KEY", "secret");
        // Unset, and any value other than "1": refused, nothing can be sent.
        let unset = RemoteEmbedder::from_env();
        std::env::set_var("EMBEDDING_ALLOW_INSECURE", "0");
        let zero = RemoteEmbedder::from_env();
        // "1": allowed.
        std::env::set_var("EMBEDDING_ALLOW_INSECURE", "1");
        let allowed = RemoteEmbedder::from_env();
        for k in EMBED_ENV {
            std::env::remove_var(k);
        }
        for r in [unset, zero] {
            let msg = format!("{:#}", r.err().expect("must fail closed"));
            assert!(msg.contains("EMBEDDING_ALLOW_INSECURE=1"), "got: {msg}");
            assert!(!msg.contains("secret"), "key must not leak: {msg}");
        }
        let e = allowed.unwrap().expect("allowed with =1");
        assert_eq!(e.url, "http://embedding-svc.example:9092");
    }

    #[cfg(feature = "remote-embed")]
    #[test]
    fn from_env_allows_https_and_loopback_http_without_flag() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for k in EMBED_ENV {
            std::env::remove_var(k);
        }
        for u in [
            "https://embedding-svc.example.com",
            "http://localhost:9092",
            "http://127.0.0.1:9092",
            "http://[::1]:9092",
        ] {
            std::env::set_var("EMBEDDING_SVC_URL", u);
            let got = RemoteEmbedder::from_env();
            std::env::remove_var("EMBEDDING_SVC_URL");
            assert!(got.unwrap().is_some(), "{u} must be accepted");
        }
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
        track_all(dir.path());
        let db = index_db_with_files(&[("a.rs", "sha-a")]);
        let r = populate_embeddings(dir.path(), &FailingEmbedder, &db);
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
            Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
        }
    }

    #[test]
    fn populate_embeddings_second_batch_failure_returns_err_keeps_first_batch() {
        let dir = tempfile::tempdir().unwrap();
        let names: Vec<String> = (0..EMBED_BATCH + 1).map(|i| format!("f{i}.rs")).collect();
        for n in &names {
            std::fs::write(dir.path().join(n), "fn f() {}").unwrap();
        }
        track_all(dir.path());
        let rows: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "sha")).collect();
        let db = index_db_with_files(&rows);
        let embedder = FailOnSecondCall(std::sync::atomic::AtomicUsize::new(0));
        let r = populate_embeddings(dir.path(), &embedder, &db);
        assert!(r.is_err(), "later batch failure must surface");
        let stored: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM embeddings", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, EMBED_BATCH as i64, "first batch stays committed");
        // A retry only needs to embed the remaining file.
        let n = populate_embeddings(dir.path(), &MockEmbedder, &db).unwrap();
        assert_eq!(n, 1);
    }
}
