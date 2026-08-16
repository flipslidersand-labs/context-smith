use clap::{Parser, Subcommand};
use std::path::PathBuf;

use context_smith::GitRepo;
use context_smith::index_builder::{build_index, IndexDb};

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
    /// Build a context bundle for a task (Phase 5+)
    Build {
        #[arg(long)]
        repo: String,
        #[arg(long)]
        task: String,
        #[arg(long, default_value_t = 30000)]
        budget: usize,
        #[arg(long, default_value = "context.bundle")]
        out: String,
        #[arg(long)]
        explain: bool,
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
        Command::Build { .. } => {
            eprintln!("contextsmith build — not yet implemented (Phase 5+)");
            std::process::exit(1);
        }
    }
    Ok(())
}
