use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "contextsmith", about = "AI context compiler for Git repositories")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build index for a repository
    Index {
        #[arg(long)]
        repo: String,
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
        #[arg(long)]
        explain: bool,
    },
}

fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();
    println!("contextsmith — not yet implemented");
    Ok(())
}
