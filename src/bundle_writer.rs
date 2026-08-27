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

/// Compute a collision-free slug for a file's output `.md` filename.
///
/// The first occurrence of a base slug uses it as-is; subsequent occurrences
/// append `_0`, `_1`, `_2`, … (0-indexed from the first collision) so the
/// sequence for three collisions is: `slug`, `slug_0`, `slug_1`.
pub(crate) fn make_slug(base: &str, count: usize) -> String {
    if count == 0 {
        base.to_string()
    } else {
        format!("{}_{}", base, count - 1)
    }
}

/// Return the shortest backtick fence string (minimum length 3) that does not
/// appear as a standalone fence sequence in `content`.
///
/// GFM spec §4.5: a code fence opening consists of ≥3 backticks; a closing
/// fence must be at least as long as the opener.  By using one more backtick
/// than the longest consecutive run of backticks found in the content we
/// guarantee the fence cannot be prematurely closed by content.
pub(crate) fn safe_fence(content: &str) -> String {
    let max_run = content
        .chars()
        .fold((0usize, 0usize), |(max, cur), ch| {
            if ch == '`' {
                let next = cur + 1;
                (max.max(next), next)
            } else {
                (max, 0)
            }
        })
        .0;
    let len = max_run.max(2) + 1; // at least 3 backticks
    "`".repeat(len)
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
        let slug = make_slug(&base_slug, *count);
        *count += 1;

        let ext = c.path.extension().and_then(|e| e.to_str()).unwrap_or("txt");
        let fence = safe_fence(&c.content);
        let file_md = format!(
            "<!-- {} -->\n{fence}{ext}\n{}\n{fence}\n",
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
    fn safe_fence_plain_content_uses_triple_backticks() {
        assert_eq!(safe_fence("fn main() {}"), "```");
    }

    #[test]
    fn safe_fence_content_with_triple_backtick_uses_four() {
        assert_eq!(safe_fence("before\n```\nafter"), "````");
    }

    #[test]
    fn safe_fence_content_with_four_backticks_uses_five() {
        assert_eq!(safe_fence("````"), "`````");
    }

    #[test]
    fn write_bundle_codefence_injection_not_possible() {
        // A file whose content contains a triple-backtick block.
        // The rendered .md must not have a bare ``` line that closes the fence early.
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("bundle");
        let malicious_content = "before\n```\ninjected: bad content\n```\nafter";
        let selected = vec![cand("src/attack.rs", 0.9, malicious_content)];
        write_bundle("task", 10000, &selected, "", &out, false).unwrap();

        let md =
            std::fs::read_to_string(out.join("relevant-code").join("src_attack_rs.md")).unwrap();
        // The fence must use ≥4 backticks because the content has ```.
        assert!(
            md.starts_with("<!-- src/attack.rs -->\n````"),
            "fence must be longer than content's backtick run; got:\n{md}"
        );
        // The full content must be preserved verbatim between the fences.
        assert!(
            md.contains(malicious_content),
            "original content must appear unchanged"
        );
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

    #[test]
    fn make_slug_no_collision_returns_base() {
        assert_eq!(make_slug("src_lib_rs", 0), "src_lib_rs");
    }

    #[test]
    fn make_slug_first_collision_uses_zero_suffix() {
        // Previously the first collision produced `_1`, skipping `_0`.
        assert_eq!(make_slug("src_lib_rs", 1), "src_lib_rs_0");
    }

    #[test]
    fn make_slug_subsequent_collisions_are_sequential() {
        assert_eq!(make_slug("src_lib_rs", 2), "src_lib_rs_1");
        assert_eq!(make_slug("src_lib_rs", 3), "src_lib_rs_2");
    }

    #[test]
    fn write_bundle_slug_collision_no_gap() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("bundle");
        // Three candidates with the same path — they collide on the slug.
        let path = "src/lib.rs";
        let selected = vec![
            cand(path, 0.9, "fn a() {}"),
            cand(path, 0.8, "fn b() {}"),
            cand(path, 0.7, "fn c() {}"),
        ];
        write_bundle("task", 10000, &selected, "", &out, false).unwrap();

        let rc = out.join("relevant-code");
        // Expected slugs: src_lib_rs, src_lib_rs_0, src_lib_rs_1 (no gap at _0).
        assert!(
            rc.join("src_lib_rs.md").exists(),
            "first file uses base slug"
        );
        assert!(
            rc.join("src_lib_rs_0.md").exists(),
            "first collision must use _0 suffix, not _1"
        );
        assert!(
            rc.join("src_lib_rs_1.md").exists(),
            "second collision uses _1 suffix"
        );
        // Ensure the old buggy name (_1 for first collision) is NOT present as
        // a standalone file (it's only valid as the second collision slot now).
        // The three files should be exactly the three we expect.
        let entries: Vec<_> = std::fs::read_dir(&rc)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries.len(), 3, "exactly three slug files: {entries:?}");
    }
}
