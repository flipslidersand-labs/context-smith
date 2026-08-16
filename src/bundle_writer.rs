use anyhow::Result;
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::budget::Candidate;

#[derive(Serialize)]
pub struct Citation {
    pub path: PathBuf,
    pub score: f32,
    pub tokens: usize,
    pub reason: String,
}

/// Write a context bundle to `out_dir`.
///
/// Layout:
///   out_dir/
///     task.md              — task description + file list with scores
///     relevant-code/       — one .md per selected file
///     citations.json       — machine-readable file scores
pub fn write_bundle(
    task: &str,
    budget: usize,
    selected: &[Candidate],
    diff_summary: &str,
    out_dir: &Path,
    explain: bool,
) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;
    std::fs::create_dir_all(out_dir.join("relevant-code"))?;

    let used: usize = selected.iter().map(|c| c.tokens()).sum();

    // task.md
    let mut task_md = format!(
        "# Context Bundle\n\n**Task:** {task}\n\n**Budget:** {budget} tokens  \n**Used:** {used} tokens\n\n## Selected Files\n\n",
    );
    for (i, c) in selected.iter().enumerate() {
        let explain_str = if explain {
            format!(" (score={:.3})", c.score)
        } else {
            String::new()
        };
        task_md.push_str(&format!(
            "{}. `{}`{} — {} tokens\n",
            i + 1,
            c.path.display(),
            explain_str,
            c.tokens(),
        ));
    }

    if !diff_summary.is_empty() {
        task_md.push_str("\n## Recent Diff\n\n```diff\n");
        task_md.push_str(diff_summary);
        task_md.push_str("\n```\n");
    }

    std::fs::write(out_dir.join("task.md"), &task_md)?;

    // relevant-code/{slug}.md per file
    for c in selected {
        let slug = c
            .path
            .to_string_lossy()
            .replace(['/', '\\', '.'], "_");
        let ext = c.path.extension().and_then(|e| e.to_str()).unwrap_or("txt");
        let file_md = format!(
            "<!-- {} -->\n```{ext}\n{}\n```\n",
            c.path.display(),
            c.content,
        );
        std::fs::write(out_dir.join("relevant-code").join(format!("{slug}.md")), file_md)?;
    }

    // citations.json
    let citations: Vec<Citation> = selected
        .iter()
        .map(|c| Citation {
            path: c.path.clone(),
            score: c.score,
            tokens: c.tokens(),
            reason: format!("bm25_score={:.3}", c.score),
        })
        .collect();
    let json = serde_json::to_string_pretty(&citations)?;
    std::fs::write(out_dir.join("citations.json"), json)?;

    Ok(())
}
