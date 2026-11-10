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
            let content = match repo.read_tracked(Path::new(&rel_path)) {
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

    // Step 4: Greedy budget allocation
    Ok(allocate(candidates, opts.budget))
}

#[cfg(test)]
mod tests {
    use super::SelectOptions;

    #[test]
    fn select_options_new_enables_embedding_by_default() {
        let o = SelectOptions::new(1234);
        assert_eq!(o.budget, 1234);
        assert!(!o.no_embed);
    }
}
