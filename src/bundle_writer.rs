use anyhow::Result;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::budget::Candidate;

#[derive(Serialize)]
pub struct Citation {
    pub path: PathBuf,
    pub score: f32,
    pub tokens: usize,
    pub reason: String,
}

/// Render the `task.md` document (task description, selected file list, and the
/// recent diff) as a String. Shared by [`write_bundle`] and the `query` command
/// so both produce identical Markdown.
pub fn render_task_md(
    task: &str,
    budget: usize,
    selected: &[Candidate],
    diff_summary: &str,
    explain: bool,
) -> String {
    let used: usize = selected.iter().map(|c| c.tokens()).sum();

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

    task_md
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

    let task_md = render_task_md(task, budget, selected, diff_summary, explain);
    std::fs::write(out_dir.join("task.md"), &task_md)?;

    // relevant-code/{slug}.md per file — track seen slugs to resolve collisions
    let mut slug_counts: HashMap<String, usize> = HashMap::new();
    for c in selected {
        let base_slug = c.path.to_string_lossy().replace(['/', '\\', '.'], "_");
        let count = slug_counts.entry(base_slug.clone()).or_insert(0);
        let slug = if *count == 0 {
            base_slug
        } else {
            format!("{}_{}", base_slug, count)
        };
        *count += 1;

        let ext = c.path.extension().and_then(|e| e.to_str()).unwrap_or("txt");
        let file_md = format!(
            "<!-- {} -->\n```{ext}\n{}\n```\n",
            c.path.display(),
            c.content,
        );
        std::fs::write(
            out_dir.join("relevant-code").join(format!("{slug}.md")),
            file_md,
        )?;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(path: &str, score: f32, content: &str) -> Candidate {
        Candidate {
            file_id: 1,
            path: PathBuf::from(path),
            score,
            content: content.into(),
        }
    }

    #[test]
    fn render_task_md_lists_files_and_diff() {
        let sel = vec![cand("src/a.rs", 0.9, "fn a() {}")];
        let md = render_task_md("do X", 1000, &sel, "diff body", false);
        assert!(md.contains("**Task:** do X"));
        assert!(md.contains("**Budget:** 1000 tokens"));
        assert!(md.contains("`src/a.rs`"));
        assert!(md.contains("## Recent Diff"));
        assert!(md.contains("diff body"));
        // Without --explain the score annotation is omitted.
        assert!(!md.contains("score="));
    }

    #[test]
    fn render_task_md_explain_shows_score_and_omits_empty_diff() {
        let sel = vec![cand("src/a.rs", 0.875, "fn a() {}")];
        let md = render_task_md("t", 1000, &sel, "", true);
        assert!(md.contains("(score=0.875)"));
        assert!(!md.contains("## Recent Diff"), "empty diff must be omitted");
    }
}
