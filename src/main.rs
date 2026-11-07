use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

/// Clap value_parser: reject empty strings early with a clear error message.
fn non_empty_string(s: &str) -> Result<String, String> {
    if s.trim().is_empty() {
        Err("value must not be empty".to_owned())
    } else {
        Ok(s.to_owned())
    }
}

/// Clap value_parser: reject zero budget with a clear error message.
fn positive_usize(s: &str) -> Result<usize, String> {
    let v: usize = s
        .parse()
        .map_err(|_| format!("'{}' is not a valid integer", s))?;
    if v == 0 {
        Err("--budget must be \u{2265} 1".to_owned())
    } else {
        Ok(v)
    }
}

use context_smith::budget::{allocate, Candidate};
use context_smith::bundle_writer::{render_task_md, write_bundle};
use context_smith::dep_builder::bfs_expand;
use context_smith::index_builder::{build_index, IndexDb};
use context_smith::path_kind::path_score_factor;
use context_smith::search_index::search_bm25;
use context_smith::GitRepo;

#[derive(Parser)]
#[command(
    name = "contextsmith",
    version,
    about = "AI context compiler for Git repositories"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build index for a repository (saved to {repo}/.contextsmith/index.db)
    Index {
        #[arg(long, value_parser = non_empty_string)]
        repo: String,
        /// Override the default output path (.contextsmith/index.db inside the repo)
        #[arg(long)]
        out: Option<String>,
        /// Force a full re-index even for files whose content has not changed
        #[arg(long)]
        force: bool,
    },
    /// Build a context bundle for a task
    Build {
        #[arg(long, value_parser = non_empty_string)]
        repo: String,
        /// Task description. May start with `-` (e.g. an issue title such as "--files is slow").
        #[arg(long, value_parser = non_empty_string, allow_hyphen_values = true)]
        task: String,
        #[arg(long, default_value_t = 30000, value_parser = positive_usize)]
        budget: usize,
        #[arg(long, default_value = "context.bundle", value_parser = non_empty_string)]
        out: String,
        /// Path to index db (default: {repo}/.contextsmith/index.db); use when index was built with --out
        #[arg(long)]
        index: Option<String>,
        /// Include BM25 score annotations in task.md
        #[arg(long)]
        explain: bool,
        /// Number of recent commits to include as diff context
        #[arg(long, default_value_t = 3)]
        diff_commits: usize,
        /// Disable semantic (embedding) seeds even when the index has them; BM25 only
        #[arg(long)]
        no_embed: bool,
    },
    /// Select task-relevant files and print to stdout (no files written).
    /// Designed for scripts and pipelines: `--format json` for machine-readable
    /// output, `--format md` for the task.md-style summary.
    Query {
        #[arg(long, value_parser = non_empty_string)]
        repo: String,
        /// Task description. May start with `-` (e.g. an issue title such as "--files is slow").
        #[arg(long, value_parser = non_empty_string, allow_hyphen_values = true)]
        task: String,
        #[arg(long, default_value_t = 30000, value_parser = positive_usize)]
        budget: usize,
        /// Path to index db (default: {repo}/.contextsmith/index.db)
        #[arg(long)]
        index: Option<String>,
        /// Output format
        #[arg(long, value_enum, default_value_t = QueryFormat::Json)]
        format: QueryFormat,
        /// Include BM25 score annotations (md format only)
        #[arg(long)]
        explain: bool,
        /// Disable semantic (embedding) seeds even when the index has them; BM25 only
        #[arg(long)]
        no_embed: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum QueryFormat {
    Json,
    Md,
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
            if let Some(embedder) = context_smith::embed::RemoteEmbedder::from_env() {
                match context_smith::embed::fuse_seeds(conn, &embedder, task, &bm25, 20) {
                    Ok(fused) => {
                        eprintln!("semantic: RRF-fused BM25 with embedding seeds");
                        return Ok(fused);
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
fn select_candidates(
    repo: &GitRepo,
    db: &IndexDb,
    task: &str,
    budget: usize,
    no_embed: bool,
) -> anyhow::Result<Vec<Candidate>> {
    // Step 1: BM25 search → top-20 seeds, optionally RRF-fused with semantic
    // vectors. Fusion can surface files with zero lexical overlap, so the empty
    // check happens after fusion (semantic may rescue an empty BM25).
    let bm25 = search_bm25(db.connection(), task, 20)?;
    let seeds = build_seeds(db.connection(), task, bm25, no_embed)?;
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
                use rusqlite::OptionalExtension;
                match stmt.query_row([file_id], |row| row.get(0)).optional()? {
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
                    continue;
                }
            };
            candidates.push(Candidate {
                file_id,
                path: PathBuf::from(rel_path),
                score,
                content,
            });
        }
    }

    // Step 4: Greedy budget allocation
    Ok(allocate(candidates, budget))
}

/// Resolve the index db path for a repo, erroring if it does not exist.
fn resolve_index_path(repo: &GitRepo, index: Option<String>) -> anyhow::Result<PathBuf> {
    let db_path = match index {
        Some(p) => PathBuf::from(p),
        None => repo.root().join(".contextsmith").join("index.db"),
    };
    if !db_path.exists() {
        anyhow::bail!(
            "Index not found at {}. Run `contextsmith index --repo <path>` first.",
            db_path.display()
        );
    }
    Ok(db_path)
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Index { repo, out, force } => {
            let repo = GitRepo::new(&repo)?;
            let db_path = match out {
                Some(p) => {
                    // Keep an index outside the repository (e.g. in a cache dir) with one command.
                    let p = PathBuf::from(p);
                    if let Some(parent) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                        std::fs::create_dir_all(parent)?;
                    }
                    p
                }
                None => {
                    let dir = repo.root().join(".contextsmith");
                    std::fs::create_dir_all(&dir)?;
                    dir.join("index.db")
                }
            };
            let db = IndexDb::open(&db_path)?;
            let stats = build_index(&repo, &db, force)?;
            println!(
                "indexed: {} files ({} re-indexed, {} skipped, {} deleted), {} symbols, {} deps → {}",
                stats.files_total,
                stats.files_indexed,
                stats.files_skipped,
                stats.files_deleted,
                stats.symbols_total,
                stats.deps_total,
                db_path.display(),
            );

            // Optional: generate semantic embeddings via the remote embedding
            // service. Non-fatal — a failure here leaves the BM25 index usable.
            #[cfg(feature = "remote-embed")]
            match context_smith::embed::RemoteEmbedder::from_env() {
                Some(embedder) => {
                    match context_smith::embed::populate_embeddings(
                        db.connection(),
                        repo.root(),
                        &embedder,
                        &db,
                    ) {
                        Ok(n) => println!("embeddings: {n} files (remote-embed)"),
                        Err(e) => eprintln!("warning: embedding generation failed: {e}"),
                    }
                }
                None => eprintln!(
                    "note: remote-embed built but EMBEDDING_SVC_URL unset; skipping embeddings"
                ),
            }
        }

        Command::Build {
            repo,
            task,
            budget,
            out,
            index,
            explain,
            diff_commits,
            no_embed,
        } => {
            let repo = GitRepo::new(&repo)?;
            let db_path = resolve_index_path(&repo, index)?;
            let db = IndexDb::open(&db_path)?;

            let selected = select_candidates(&repo, &db, &task, budget, no_embed)?;

            // Recent diff, then write the bundle to disk.
            let diff_summary = match repo.recent_diff(diff_commits) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("warn: recent_diff failed ({e}); omitting diff from bundle");
                    String::new()
                }
            };
            let out_path = PathBuf::from(&out);
            let used: usize = selected.iter().map(|c| c.tokens()).sum();
            write_bundle(&task, budget, &selected, &diff_summary, &out_path, explain)?;

            println!(
                "bundle: {} files, {}/{} tokens → {}",
                selected.len(),
                used,
                budget,
                out_path.display(),
            );
        }

        Command::Query {
            repo,
            task,
            budget,
            index,
            format,
            explain,
            no_embed,
        } => {
            let repo = GitRepo::new(&repo)?;
            let db_path = resolve_index_path(&repo, index)?;
            let db = IndexDb::open(&db_path)?;

            let selected = select_candidates(&repo, &db, &task, budget, no_embed)?;
            let used: usize = selected.iter().map(|c| c.tokens()).sum();

            match format {
                QueryFormat::Json => {
                    let files: Vec<serde_json::Value> = selected
                        .iter()
                        .map(|c| {
                            serde_json::json!({
                                "path": c.path.to_string_lossy(),
                                "score": c.score,
                                "tokens": c.tokens(),
                            })
                        })
                        .collect();
                    let doc = serde_json::json!({
                        "task": task,
                        "budget": budget,
                        "used_tokens": used,
                        "files": files,
                    });
                    println!("{}", serde_json::to_string_pretty(&doc)?);
                }
                QueryFormat::Md => {
                    // No diff in query output — keep it focused on the selection.
                    print!("{}", render_task_md(&task, budget, &selected, "", explain));
                }
            }
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use context_smith::index_builder::build_index;

    #[test]
    fn select_candidates_ignores_file_swapped_for_external_symlink_after_index() {
        let outside = tempfile::TempDir::new().unwrap();
        let ext = outside.path().join("s.rs");
        std::fs::write(&ext, "pub fn alpha_widget() {}\n// EXTSECRET\n").unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let git = git2::Repository::init(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("a.rs"), "pub fn alpha_widget() {}\n").unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new("a.rs")).unwrap();
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let repo = GitRepo::new(tmp.path()).unwrap();
        let dbdir = tempfile::TempDir::new().unwrap();
        let db = IndexDb::open(&dbdir.path().join("i.db")).unwrap();
        build_index(&repo, &db, false).unwrap();

        // Swap after indexing: query time must not follow the new symlink.
        std::fs::remove_file(tmp.path().join("a.rs")).unwrap();
        std::os::unix::fs::symlink(&ext, tmp.path().join("a.rs")).unwrap();

        if let Ok(sel) = select_candidates(&repo, &db, "alpha_widget", 10_000, true) {
            assert!(sel.iter().all(|c| !c.content.contains("EXTSECRET")));
        }
    }

    fn indexed_repo(files: &[&str]) -> (tempfile::TempDir, GitRepo, tempfile::TempDir, IndexDb) {
        let tmp = tempfile::TempDir::new().unwrap();
        let git = git2::Repository::init(tmp.path()).unwrap();
        let mut index = git.index().unwrap();
        for f in files {
            std::fs::write(tmp.path().join(f), "pub fn alpha_widget() {}\n").unwrap();
            index.add_path(Path::new(f)).unwrap();
        }
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        let dbdir = tempfile::TempDir::new().unwrap();
        let db = IndexDb::open(&dbdir.path().join("i.db")).unwrap();
        build_index(&repo, &db, false).unwrap();
        (tmp, repo, dbdir, db)
    }

    #[test]
    fn select_candidates_propagates_files_query_error() {
        let (_t, repo, _d, db) = indexed_repo(&["a.rs"]);
        db.connection()
            .execute_batch("UPDATE files SET path = CAST(path AS BLOB)")
            .unwrap();
        let res = select_candidates(&repo, &db, "alpha_widget", 10_000, true);
        assert!(res.is_err(), "non-NoRows DB error must not be swallowed");
    }

    #[test]
    fn select_candidates_skips_candidate_with_no_files_row() {
        let (_t, repo, _d, db) = indexed_repo(&["a.rs", "b.rs"]);
        db.connection()
            .execute_batch("PRAGMA foreign_keys=OFF; DELETE FROM files WHERE path = 'b.rs'")
            .unwrap();
        let sel = select_candidates(&repo, &db, "alpha_widget", 10_000, true).unwrap();
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].path, PathBuf::from("a.rs"));
    }
}
