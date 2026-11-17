use crate::budget::{allocate, Candidate};
use crate::dep_builder::bfs_expand;
use crate::index_builder::IndexDb;
use crate::path_kind::path_score_factor;
use crate::search_index::search_bm25;
use crate::GitRepo;
use anyhow::Context;
use rusqlite::OptionalExtension;
use std::path::{Path, PathBuf};

/// Options for [`select_candidates`].
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct SelectOptions {
    /// Token budget for the selection.
    pub budget: usize,
    /// Disable semantic (embedding) seeds; BM25 only. Only has an effect when
    /// built with the `remote-embed` feature.
    pub no_embed: bool,
}

impl SelectOptions {
    pub fn new(budget: usize) -> Self {
        Self {
            budget,
            no_embed: false,
        }
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
) -> anyhow::Result<Vec<(i64, f32)>> {
    #[cfg(feature = "remote-embed")]
    {
        if !no_embed {
            if let Some(embedder) = crate::embed::RemoteEmbedder::from_env() {
                match crate::embed::fuse_seeds_with_status(conn, &embedder, task, &bm25, 20) {
                    Ok((seeds, fused)) => {
                        if fused {
                            eprintln!("semantic: RRF-fused BM25 with embedding seeds");
                        } else {
                            eprintln!("semantic: no usable embeddings; using BM25 seeds only");
                        }
                        return Ok(seeds);
                    }
                    Err(e) => {
                        eprintln!("semantic: embedding failed ({e}); falling back to BM25")
                    }
                }
            }
        }
    }
    #[cfg(not(feature = "remote-embed"))]
    let _ = (conn, task, no_embed);
    Ok(bm25)
}

/// Run the selection pipeline for a task: BM25 (optionally RRF-fused with
/// semantic seeds) → dependency BFS → load content → greedy budget allocation.
/// Returns the selected candidates in score order, or an error when no files
/// match (so the caller decides how to exit, preserving RAII drop order).
pub fn select_candidates(
    repo: &GitRepo,
    db: &IndexDb,
    task: &str,
    opts: &SelectOptions,
) -> anyhow::Result<Vec<Candidate>> {
    // Step 1: BM25 search → top-20 seeds, optionally RRF-fused with semantic
    // vectors. Fusion can surface files with zero lexical overlap, so the empty
    // check happens after fusion (semantic may rescue an empty BM25).
    let bm25 = search_bm25(db.connection(), task, 20)?;
    let seeds = build_seeds(db.connection(), task, bm25, opts.no_embed)?;
    if seeds.is_empty() {
        anyhow::bail!("No matching files found for task: {}", task);
    }

    // Step 2: BFS expand from seeds (depth=2)
    let mut scored = bfs_expand(db.connection(), &seeds, 2)?;
    for (fid, s) in &seeds {
        scored
            .entry(*fid)
            .and_modify(|v| {
                if *s > *v {
                    *v = *s;
                }
            })
            .or_insert(*s);
    }

    // Step 3: Load paths for scored files only, then read content.
    // Filter in the SQL query so we never fetch rows for unscored files,
    // avoiding the O(all-files) read_to_string that wastes RAM (#80).
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut unreadable: Vec<String> = Vec::new();
    let git_index = repo.open_index()?;
    {
        let conn = db.connection();
        // Collect only the file IDs that have a positive score; the DB query
        // is restricted to this set so we read at most len(scored) rows.
        let scored_ids: Vec<i64> = scored
            .iter()
            .filter_map(|(&id, &s)| if s > 0.0 { Some(id) } else { None })
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
                    eprintln!("warn: skip {:?}: {e}", abs);
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
