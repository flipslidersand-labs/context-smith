use crate::budget::{allocate, Candidate};
use crate::expand::{expand, mode_from_env};
use crate::index_builder::IndexDb;
use crate::path_kind::path_score_factor;
use crate::search_index::search_bm25;
use crate::GitRepo;
use anyhow::Context;
use rusqlite::OptionalExtension;
use std::path::{Path, PathBuf};

const DEFAULT_SEED_COUNT: usize = 20;
const DEFAULT_BFS_DEPTH: u8 = 2;
const MAX_SEED_COUNT: usize = 1000;

/// Options for [`select_candidates`].
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct SelectOptions {
    /// Token budget for the selection.
    pub budget: usize,
    /// Disable semantic (embedding) seeds; BM25 only. Only has an effect when
    /// built with the `remote-embed` feature.
    pub no_embed: bool,
    /// Number of top BM25 seeds kept, and the `top_n` used when fusing with
    /// semantic seeds (single source so the two never drift). Default 20.
    pub seed_count: usize,
    /// Dependency BFS depth from the seeds. Default 2.
    pub bfs_depth: u8,
}

impl SelectOptions {
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            no_embed: false,
            seed_count: DEFAULT_SEED_COUNT,
            bfs_depth: DEFAULT_BFS_DEPTH,
        }
    }
}

/// How the seed files were chosen.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeedMode {
    /// BM25 only (embeddings disabled, not built in, or no embedder configured).
    Bm25,
    /// BM25 RRF-fused with embedding seeds.
    Fused,
    /// An embedder was configured but no usable embeddings were stored; BM25 seeds used.
    NoUsableEmbeddings,
    /// Embedding failed; fell back to BM25. Holds the failure reason.
    FallbackBm25(String),
}

/// Result of [`select_candidates`]: the candidates plus diagnostics the caller may display.
#[non_exhaustive]
#[derive(Debug)]
pub struct Selection {
    /// Selected candidates in score order.
    pub candidates: Vec<Candidate>,
    /// How the seeds were chosen.
    pub seed_mode: SeedMode,
    /// Files skipped because they could not be read: (absolute path, error).
    pub skipped: Vec<(PathBuf, String)>,
}

/// Error from [`select_candidates`]. Carries the diagnostics gathered before the failure so the
/// caller can still show them. `Display` and the error chain are those of the underlying error.
#[non_exhaustive]
#[derive(Debug)]
pub struct SelectError {
    /// The underlying failure.
    pub error: anyhow::Error,
    /// How the seeds were chosen before the failure (`Bm25` if it failed earlier).
    pub seed_mode: SeedMode,
    /// Files skipped as unreadable before the failure: (absolute path, error).
    pub skipped: Vec<(PathBuf, String)>,
}

impl std::fmt::Display for SelectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}

impl std::error::Error for SelectError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // Skip the wrapped error itself (already shown by `Display`); continue its chain.
        let inner: &(dyn std::error::Error + Send + Sync + 'static) = self.error.as_ref();
        inner.source()
    }
}

/// Compute seed files for a task. With the `remote-embed` feature and a
/// configured embedder, RRF-fuse BM25 with semantic (vector) seeds; otherwise
/// return the BM25 seeds unchanged (backward compatible).
fn build_seeds(
    conn: &rusqlite::Connection,
    task: &str,
    bm25: Vec<(i64, f32)>,
    no_embed: bool,
    seed_count: usize,
) -> (Vec<(i64, f32)>, SeedMode) {
    #[cfg(feature = "remote-embed")]
    {
        if !no_embed {
            let embedder = match crate::embed::RemoteEmbedder::from_env() {
                Ok(e) => e,
                Err(e) => return (bm25, SeedMode::FallbackBm25(e.to_string())),
            };
            if let Some(embedder) = embedder {
                match crate::embed::fuse_seeds_with_status(conn, &embedder, task, &bm25, seed_count)
                {
                    Ok((seeds, fused)) => {
                        let mode = if fused {
                            SeedMode::Fused
                        } else {
                            SeedMode::NoUsableEmbeddings
                        };
                        return (seeds, mode);
                    }
                    Err(e) => return (bm25, SeedMode::FallbackBm25(e.to_string())),
                }
            }
        }
    }
    #[cfg(not(feature = "remote-embed"))]
    let _ = (conn, task, no_embed, seed_count);
    (bm25, SeedMode::Bm25)
}

/// Run the selection pipeline for a task: BM25 (optionally RRF-fused with
/// semantic seeds) → dependency BFS → load content → greedy budget allocation.
/// Returns the selected candidates in score order plus diagnostics (seed mode, skipped files), or an error when no files
/// match (so the caller decides how to exit, preserving RAII drop order).
pub fn select_candidates(
    repo: &GitRepo,
    db: &IndexDb,
    task: &str,
    opts: &SelectOptions,
) -> Result<Selection, SelectError> {
    let mut seed_mode = SeedMode::Bm25;
    let mut skipped = Vec::new();
    match select_inner(repo, db, task, opts, &mut seed_mode, &mut skipped) {
        Ok(candidates) => Ok(Selection {
            candidates,
            seed_mode,
            skipped,
        }),
        Err(error) => Err(SelectError {
            error,
            seed_mode,
            skipped,
        }),
    }
}

/// BM25 top-`seed_count` among files that can actually be read and are non-empty (#536). Unreadable files (e.g. non-UTF-8
/// ones that are indexed by path only, #308) would otherwise occupy seed slots and push real
/// candidates out (#465), so fetch more rows until enough readable ones are found. When none is
/// readable the unfiltered hits are returned, so the caller still reports "could not be read".
fn readable_bm25(
    repo: &GitRepo,
    git_index: &git2::Index,
    db: &IndexDb,
    task: &str,
    seed_count: usize,
    skipped: &mut Vec<(PathBuf, String)>,
) -> anyhow::Result<Vec<(i64, f32)>> {
    let conn = db.connection();
    let first = search_bm25(conn, task, seed_count)?;
    let mut fetch = seed_count;
    let mut hits = first.clone();
    loop {
        let mut readable = Vec::with_capacity(seed_count);
        let mut dropped: Vec<(PathBuf, String)> = Vec::new();
        for &(id, score) in &hits {
            let rel: Option<String> = conn
                .query_row("SELECT path FROM files WHERE id = ?1", [id], |r| r.get(0))
                .optional()?;
            let Some(rel) = rel else { continue };
            match repo.read_indexed(git_index, Path::new(&rel)) {
                // Empty / whitespace-only files do not take a seed slot (#536).
                Ok(content) => {
                    if !content.trim().is_empty() && readable.len() < seed_count {
                        readable.push((id, score));
                    }
                }
                Err(e) => dropped.push((repo.root().join(&rel), e.to_string())),
            }
        }
        // `hits.len() < fetch` means BM25 has no more rows to give.
        if readable.len() >= seed_count || hits.len() < fetch || fetch >= MAX_SEED_COUNT {
            if readable.is_empty() {
                return Ok(first);
            }
            for d in dropped {
                if !skipped.iter().any(|(p, _)| *p == d.0) {
                    skipped.push(d);
                }
            }
            return Ok(readable);
        }
        fetch = (fetch * 2).min(MAX_SEED_COUNT);
        hits = search_bm25(conn, task, fetch)?;
    }
}

fn select_inner(
    repo: &GitRepo,
    db: &IndexDb,
    task: &str,
    opts: &SelectOptions,
    seed_mode_out: &mut SeedMode,
    skipped: &mut Vec<(PathBuf, String)>,
) -> anyhow::Result<Vec<Candidate>> {
    if opts.seed_count == 0 {
        anyhow::bail!("seed_count must be >= 1");
    }
    let seed_count = opts.seed_count.min(MAX_SEED_COUNT);

    // Step 1: BM25 search → top-`seed_count` seeds, optionally RRF-fused with semantic
    // vectors. Fusion can surface files with zero lexical overlap, so the empty
    // check happens after fusion (semantic may rescue an empty BM25).
    let git_index = repo.open_index()?;
    let bm25 = readable_bm25(repo, &git_index, db, task, seed_count, skipped)?;
    let (seeds, seed_mode) = build_seeds(db.connection(), task, bm25, opts.no_embed, seed_count);
    *seed_mode_out = seed_mode;
    if seeds.is_empty() {
        anyhow::bail!("No matching files found for task: {}", task);
    }

    // Step 2: BFS expand from seeds (depth=`bfs_depth`)
    // Every expansion mode already keeps each seed at its max score (never lowered).
    let scored = expand(db.connection(), mode_from_env()?, &seeds, opts.bfs_depth)?;

    // Step 3: Load paths for scored files only, then read content.
    // Filter in the SQL query so we never fetch rows for unscored files,
    // avoiding the O(all-files) read_to_string that wastes RAM (#80).
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    {
        let conn = db.connection();
        // Collect only the file IDs that have a positive score; the DB query
        // is restricted to this set so we read at most len(scored) rows.
        let scored_ids: Vec<i64> = scored
            .iter()
            .filter_map(|(&id, &s)| {
                if s.is_finite() && s > 0.0 {
                    Some(id)
                } else {
                    None
                }
            })
            .collect();

        for file_id in scored_ids {
            let rel_path: String = {
                let mut stmt = conn
                    .prepare_cached("SELECT path FROM files WHERE id = ?1 AND lang != 'unknown'")?;
                // Only "no row" (unscored/unknown-lang file) is expected; any
                // other DB error must propagate rather than drop candidates.
                match stmt
                    .query_row([file_id], |row| row.get(0))
                    .optional()
                    .with_context(|| {
                        format!(
                            "failed to read files row id={file_id} from index \
                             (rebuild with `contextsmith index --force`)"
                        )
                    })? {
                    Some(p) => p,
                    None => continue,
                }
            };
            let score = scored[&file_id] * path_score_factor(&rel_path);
            let abs = repo.root().join(&rel_path);
            let content = match repo.read_indexed(&git_index, Path::new(&rel_path)) {
                Ok(s) => s,
                Err(e) => {
                    if !skipped.iter().any(|(p, _)| *p == abs) {
                        skipped.push((abs, e.to_string()));
                    }
                    unreadable.push(rel_path);
                    continue;
                }
            };
            candidates.push(Candidate {
                file_id,
                path: PathBuf::from(rel_path),
                score,
                content,
                truncated: false,
                original_tokens: 0,
            });
        }
    }

    // Matches existed but none could be read: report it instead of returning
    // an empty success that looks like "no match" (#286).
    if candidates.is_empty() && !unreadable.is_empty() {
        anyhow::bail!(
            "matching files could not be read ({}); re-run `contextsmith index` to refresh the index",
            unreadable.join(", ")
        );
    }

    // Matches existed but every one is empty (e.g. only an empty `__init__.py`): there is nothing
    // to put in a bundle, so fail like the unreadable case rather than return `files: []` (#311).
    // The "No matching files" prefix keeps wrappers that key on it working.
    if !candidates.is_empty() && candidates.iter().all(|c| c.content.is_empty()) {
        let names: Vec<String> = candidates
            .iter()
            .map(|c| c.path.display().to_string())
            .collect();
        anyhow::bail!(
            "No matching files with content: all matches are empty ({})",
            names.join(", ")
        );
    }

    // Several tracked paths can resolve to one file (in-repo symlinks); keep one so the same
    // content does not spend the budget twice (#311).
    let candidates = dedup_by_real_path(repo, candidates);

    // Step 4: Greedy budget allocation
    Ok(allocate(candidates, opts.budget))
}

/// Collapse candidates whose paths resolve to the same file inside the repo. The candidate at the
/// real path wins; otherwise the best-scored one (ties: smaller path). Paths that cannot be
/// canonicalized are kept as they are.
fn dedup_by_real_path(repo: &GitRepo, candidates: Vec<Candidate>) -> Vec<Candidate> {
    use std::collections::HashMap;
    let canon_root = repo.canonical_root();
    // (is_real_path, index) of the winner per real path.
    let mut best: HashMap<PathBuf, usize> = HashMap::new();
    let mut keys: Vec<Option<(PathBuf, bool)>> = Vec::with_capacity(candidates.len());
    for (i, c) in candidates.iter().enumerate() {
        let key = repo.root().join(&c.path).canonicalize().ok().map(|real| {
            let is_real = real == canon_root.join(&c.path);
            (real, is_real)
        });
        if let Some((real, is_real)) = &key {
            let better = match best.get(real) {
                None => true,
                Some(&j) => {
                    let (_, j_real) = keys[j].as_ref().unwrap();
                    let o = &candidates[j];
                    if *is_real != *j_real {
                        *is_real
                    } else {
                        c.score > o.score || (c.score == o.score && c.path < o.path)
                    }
                }
            };
            if better {
                best.insert(real.clone(), i);
            }
        }
        keys.push(key);
    }
    candidates
        .into_iter()
        .enumerate()
        .filter(|(i, _)| match &keys[*i] {
            Some((real, _)) => best[real] == *i,
            None => true,
        })
        .map(|(_, c)| c)
        .collect()
}
