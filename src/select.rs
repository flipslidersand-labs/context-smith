use crate::budget::{allocate, Candidate};
use crate::expand::{expand, mode_from_env};
use crate::index_builder::IndexDb;
use crate::path_kind::{escape_control, path_score_factor};
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
    repo: &GitRepo,
    git_index: &git2::Index,
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
                match fuse_filtered(repo, git_index, conn, &embedder, task, &bm25, seed_count) {
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
    let _ = (repo, git_index, conn, task, no_embed, seed_count);
    (bm25, SeedMode::Bm25)
}

/// RRF-fuse BM25 seeds with vector seeds. Vector seeds go through the same readable / non-blank
/// filter as BM25 (#564), and whitespace-only BM25 hits (which `readable_bm25` passes through
/// when nothing readable exists) are kept out of the fusion so they cannot ride along with a
/// real vector hit. When fusion does not run or leaves nothing, `bm25` is returned unchanged
/// so the caller still reports "all matches are empty".
#[cfg(feature = "remote-embed")]
fn fuse_filtered(
    repo: &GitRepo,
    git_index: &git2::Index,
    conn: &rusqlite::Connection,
    embedder: &dyn crate::embed::Embedder,
    task: &str,
    bm25: &[(i64, f32)],
    seed_count: usize,
) -> anyhow::Result<(Vec<(i64, f32)>, bool)> {
    let class = |id: i64| classify_seed(repo, git_index, conn, id);
    let usable = |id: i64| matches!(class(id), Ok(Seed::Usable));
    let lexical: Vec<(i64, f32)> = bm25
        .iter()
        .copied()
        .filter(|&(id, _)| !matches!(class(id), Ok(Seed::Blank)))
        .collect();
    let (seeds, fused) =
        crate::embed::fuse_seeds_with_status(conn, embedder, task, &lexical, seed_count, &usable)?;
    if !fused || seeds.is_empty() {
        return Ok((bm25.to_vec(), fused));
    }
    Ok((seeds, fused))
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

/// How a seed candidate's file looks on disk (shared by the BM25 and vector seed filters, #564).
enum Seed {
    /// No `files` row for the id.
    Missing,
    /// Readable and has non-whitespace content.
    Usable,
    /// Readable but empty or whitespace-only: it must not take a seed slot (#536).
    Blank,
    /// Could not be read (absolute path, error text).
    Unreadable(PathBuf, String),
}

fn classify_seed(
    repo: &GitRepo,
    git_index: &git2::Index,
    conn: &rusqlite::Connection,
    id: i64,
) -> anyhow::Result<Seed> {
    let rel: Option<String> = conn
        .query_row("SELECT path FROM files WHERE id = ?1", [id], |r| r.get(0))
        .optional()?;
    let Some(rel) = rel else {
        return Ok(Seed::Missing);
    };
    Ok(match repo.read_indexed(git_index, Path::new(&rel)) {
        Ok(content) if content.trim().is_empty() => Seed::Blank,
        Ok(_) => Seed::Usable,
        Err(e) => Seed::Unreadable(repo.root().join(&rel), e.to_string()),
    })
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
            match classify_seed(repo, git_index, conn, id)? {
                Seed::Missing | Seed::Blank => {}
                Seed::Usable => {
                    if readable.len() < seed_count {
                        readable.push((id, score));
                    }
                }
                Seed::Unreadable(path, err) => dropped.push((path, err)),
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
    // Read the expansion mode first so a bad CONTEXTSMITH_EXPAND is reported even when the
    // search finds nothing (#531).
    let mode = mode_from_env()?;
    if opts.seed_count == 0 {
        anyhow::bail!("seed_count must be >= 1");
    }
    let seed_count = opts.seed_count.min(MAX_SEED_COUNT);

    // Step 1: BM25 search → top-`seed_count` seeds, optionally RRF-fused with semantic
    // vectors. Fusion can surface files with zero lexical overlap, so the empty
    // check happens after fusion (semantic may rescue an empty BM25).
    let git_index = repo.open_index()?;
    let bm25 = readable_bm25(repo, &git_index, db, task, seed_count, skipped)?;
    let (seeds, seed_mode) = build_seeds(
        repo,
        &git_index,
        db.connection(),
        task,
        bm25,
        opts.no_embed,
        seed_count,
    );
    *seed_mode_out = seed_mode;
    if seeds.is_empty() {
        anyhow::bail!("No matching files found for task: {}", task);
    }

    // Step 2: BFS expand from seeds (depth=`bfs_depth`)
    // Every expansion mode already keeps each seed at its max score (never lowered).
    let scored = expand(db.connection(), mode, &seeds, opts.bfs_depth)?;

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
            "matching files could not be read ({}); see the \"warn: skip\" lines above. Re-running `contextsmith index` helps only if these files changed since indexing.",
            unreadable
                .iter()
                .map(|p| escape_control(p))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Matches existed but every one is empty (e.g. only an empty `__init__.py`): there is nothing
    // to put in a bundle, so fail like the unreadable case rather than return `files: []` (#311).
    // The "No matching files" prefix keeps wrappers that key on it working.
    if !candidates.is_empty() && candidates.iter().all(|c| c.content.trim().is_empty()) {
        let names: Vec<String> = candidates
            .iter()
            .map(|c| escape_control(&c.path.to_string_lossy()))
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

#[cfg(all(test, feature = "remote-embed"))]
mod fused_tests {
    use super::*;
    use crate::embed::{vec_to_blob, EmbedMode, Embedder};
    use crate::index_builder::build_index;
    use std::path::Path;

    struct Fixed(Vec<f32>);
    impl Embedder for Fixed {
        fn embed(&self, texts: &[String], _mode: EmbedMode) -> anyhow::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| self.0.clone()).collect())
        }
    }

    fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, GitRepo, IndexDb) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        let git = git2::Repository::init(&dir).unwrap();
        let sig = git2::Signature::now("t", "t@t.com").unwrap();
        let mut index = git.index().unwrap();
        for (name, content) in files {
            std::fs::write(dir.join(name), content).unwrap();
            index.add_path(Path::new(name)).unwrap();
        }
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        let repo = GitRepo::new(&dir).unwrap();
        let db = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        build_index(&repo, &db, false).unwrap();
        (tmp, repo, db)
    }

    fn id_of(db: &IndexDb, path: &str) -> i64 {
        db.connection()
            .query_row("SELECT id FROM files WHERE path = ?1", [path], |r| r.get(0))
            .unwrap()
    }

    fn store_vector(db: &IndexDb, path: &str, v: &[f32]) {
        db.connection()
            .execute(
                "INSERT INTO embeddings (file_id, vector) VALUES (?1, ?2)",
                rusqlite::params![id_of(db, path), vec_to_blob(v)],
            )
            .unwrap();
    }

    /// #564: BM25 only hits a whitespace-only file while the vector side has a real file; the
    /// blank file must not ride along into the fused seeds.
    #[test]
    fn fused_seeds_exclude_whitespace_only_bm25_hit() {
        let (_t, repo, db) = setup(&[
            ("frobnicator.py", "\n   \n\t\n"),
            ("real.py", "def unrelated(): pass\n"),
        ]);
        store_vector(&db, "frobnicator.py", &[0.0, 1.0]);
        store_vector(&db, "real.py", &[1.0, 0.0]);
        let git_index = repo.open_index().unwrap();
        let mut skipped = Vec::new();
        let bm25 = readable_bm25(&repo, &git_index, &db, "frobnicator", 5, &mut skipped).unwrap();
        assert_eq!(bm25.len(), 1, "BM25 hits only the blank file");

        let (seeds, fused) = fuse_filtered(
            &repo,
            &git_index,
            db.connection(),
            &Fixed(vec![1.0, 0.0]),
            "frobnicator",
            &bm25,
            5,
        )
        .unwrap();
        assert!(fused);
        let ids: Vec<i64> = seeds.iter().map(|&(id, _)| id).collect();
        assert_eq!(ids, vec![id_of(&db, "real.py")]);
    }

    /// With nothing usable on either side the BM25 hits are returned so the caller reports
    /// "all matches are empty" rather than "No matching files found".
    #[test]
    fn fused_seeds_fall_back_to_bm25_when_all_blank() {
        let (_t, repo, db) = setup(&[("frobnicator.py", "\n  \n")]);
        store_vector(&db, "frobnicator.py", &[1.0, 0.0]);
        let git_index = repo.open_index().unwrap();
        let mut skipped = Vec::new();
        let bm25 = readable_bm25(&repo, &git_index, &db, "frobnicator", 5, &mut skipped).unwrap();
        let (seeds, _) = fuse_filtered(
            &repo,
            &git_index,
            db.connection(),
            &Fixed(vec![1.0, 0.0]),
            "frobnicator",
            &bm25,
            5,
        )
        .unwrap();
        assert_eq!(seeds, bm25);
    }
}
