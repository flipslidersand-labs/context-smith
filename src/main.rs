use clap::{Parser, Subcommand};
use std::path::PathBuf;

use context_smith::GitRepo;
use context_smith::budget::{allocate, Candidate};
use context_smith::bundle_writer::write_bundle;
use context_smith::dep_builder::bfs_expand;
use context_smith::index_builder::{build_index, IndexDb};
use context_smith::search_index::search_bm25;

#[derive(Parser)]
#[command(name = "contextsmith", about = "AI context compiler for Git repositories")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build index for a repository (saved to {repo}/.contextsmith/index.db)
    Index {
        #[arg(long)]
        repo: String,
        /// Override the default output path (.contextsmith/index.db inside the repo)
        #[arg(long)]
        out: Option<String>,
    },
    /// Build a context bundle for a task
    Build {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        task: String,
        #[arg(long, default_value_t = 30000)]
        budget: usize,
        #[arg(long, default_value = "context.bundle")]
        out: String,
        /// Include BM25 score annotations in task.md
        #[arg(long)]
        explain: bool,
        /// Number of recent commits to include as diff context
        #[arg(long, default_value_t = 3)]
        diff_commits: usize,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Index { repo, out } => {
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
            let stats = build_index(&repo, &db)?;
            println!(
                "indexed: {} files ({} with symbols), {} symbols, {} deps → {}",
                stats.files_total,
                stats.files_indexed,
                stats.symbols_total,
                stats.deps_total,
                db_path.display(),
            );
        }

        Command::Build { repo, task, budget, out, explain, diff_commits } => {
            let repo = GitRepo::new(&repo)?;
            let db_path = repo.root().join(".contextsmith").join("index.db");
            if !db_path.exists() {
                anyhow::bail!(
                    "Index not found at {}. Run `contextsmith index --repo <path>` first.",
                    db_path.display()
                );
            }
            let db = IndexDb::open(&db_path)?;

            // Step 1: BM25 search → top 20 seed files
            let seeds = search_bm25(db.connection(), &task, 20)?;
            if seeds.is_empty() {
                eprintln!("No matching files found for task: {}", task);
                std::process::exit(1);
            }

            // Step 2: BFS expand from seeds (depth=2)
            let mut scored = bfs_expand(db.connection(), &seeds, 2)?;
            // Merge seed scores (seeds already included in bfs_expand output)
            for (fid, s) in &seeds {
                scored.entry(*fid).and_modify(|v| {
                    if *s > *v {
                        *v = *s;
                    }
                }).or_insert(*s);
            }

            // Step 3: Load file paths + content from DB
            let mut candidates: Vec<Candidate> = Vec::new();
            {
                let conn = db.connection();
                let mut stmt = conn.prepare("SELECT id, path FROM files WHERE lang != 'unknown'")?;
                let file_rows: Vec<(i64, String)> = stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .filter_map(|r| r.ok())
                    .collect();

                for (file_id, rel_path) in file_rows {
                    let score = match scored.get(&file_id) {
                        Some(&s) if s > 0.0 => s,
                        _ => continue,
                    };
                    let abs = repo.root().join(&rel_path);
                    let content = match std::fs::read_to_string(&abs) {
                        Ok(s) => s,
                        Err(_) => continue,
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
            let selected = allocate(candidates, budget);

            // Step 5: Recent diff
            let diff_summary = repo.recent_diff(diff_commits).unwrap_or_default();

            // Step 6: Write bundle
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
    }
    Ok(())
}
