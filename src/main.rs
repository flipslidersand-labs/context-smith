use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Clap value_parser: reject empty strings early with a clear error message.
fn non_empty_string(s: &str) -> Result<String, String> {
    if s.trim().is_empty() {
        Err("value must not be empty".to_owned())
    } else {
        Ok(s.to_owned())
    }
}

use context_smith::budget::{allocate, Candidate};
use context_smith::bundle_writer::{render_task_md, write_bundle};
use context_smith::dep_builder::bfs_expand;
use context_smith::index_builder::{build_index, IndexDb};
use context_smith::search_index::search_bm25;
use context_smith::GitRepo;

#[derive(Parser)]
#[command(
    name = "contextsmith",
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
        #[arg(long, value_parser = non_empty_string)]
        task: String,
        #[arg(long, default_value_t = 30000)]
        budget: usize,
        #[arg(long, default_value = "context.bundle")]
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
        #[arg(long, value_parser = non_empty_string)]
        task: String,
        #[arg(long, default_value_t = 30000)]
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

    // Step 3: Load file paths + content from DB
    let mut candidates: Vec<Candidate> = Vec::new();
    {
        let conn = db.connection();
        let mut stmt = conn.prepare("SELECT id, path FROM files WHERE lang != 'unknown'")?;
        let file_rows: Vec<(i64, String)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        for (file_id, rel_path) in file_rows {
            let score = match scored.get(&file_id) {
                Some(&s) if s > 0.0 => s,
                _ => continue,
            };
            let abs = repo.root().join(&rel_path);
            let content = match std::fs::read_to_string(&abs) {
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
                Some(p) => PathBuf::from(p),
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
