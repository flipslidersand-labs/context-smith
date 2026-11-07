use anyhow::{Context, Result};
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
    // Collapse newlines/whitespace runs so the task cannot inject block-level
    // Markdown structure (inline HTML in the task is out of scope). Paths are
    // escaped separately by `inline_code`.
    let task = task.split_whitespace().collect::<Vec<_>>().join(" ");

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
            "{}. {}{} — {} tokens\n",
            i + 1,
            inline_code(&c.path.to_string_lossy()),
            explain_str,
            c.tokens(),
        ));
    }

    if !diff_summary.is_empty() {
        let fence = safe_fence(diff_summary);
        task_md.push_str(&format!("\n## Recent Diff\n\n{fence}diff\n"));
        task_md.push_str(diff_summary);
        task_md.push_str(&format!("\n{fence}\n"));
    }

    task_md
}

/// Replace line breaks with visible escapes so a path stays on one line.
/// Backslashes are doubled first so a literal `\n` in a name stays distinguishable.
fn escape_newlines(p: &str) -> String {
    p.replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

/// Render a path as a single-line Markdown inline code span. The delimiter is
/// longer than the longest backtick run in the path (CommonMark code spans), and
/// a space pads content that starts or ends with a backtick.
fn inline_code(p: &str) -> String {
    let p = escape_newlines(p);
    let delim = "`".repeat(longest_backtick_run(&p) + 1);
    if p.starts_with('`') || p.ends_with('`') {
        format!("{delim} {p} {delim}")
    } else {
        format!("{delim}{p}{delim}")
    }
}

/// Render a path for use inside `<!-- ... -->`: one line, and no `>` so neither
/// `-->` nor `--!>` can end the comment early.
fn comment_path(p: &str) -> String {
    escape_newlines(p)
        .replace('&', "&amp;")
        .replace('>', "&gt;")
}

fn longest_backtick_run(s: &str) -> usize {
    s.split(|c| c != '`').map(str::len).max().unwrap_or(0)
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
    let rc_dir = out_dir.join("relevant-code");
    // Refuse to follow a symlinked relevant-code/ (never delete outside out_dir).
    if let Ok(meta) = std::fs::symlink_metadata(&rc_dir) {
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "{} is a symlink; refusing to write bundle",
                rc_dir.display()
            );
        }
        // Remove stale *.md from a previous run (regular files only, no recursion).
        let clean_err = |p: &Path| format!("failed to clean {}", p.display());
        for entry in std::fs::read_dir(&rc_dir).with_context(|| clean_err(&rc_dir))? {
            let entry = entry.with_context(|| clean_err(&rc_dir))?;
            let path = entry.path();
            let ft = entry.file_type().with_context(|| clean_err(&path))?;
            // Symlinked .md: remove_file deletes the link itself, never the target.
            if (ft.is_file() || ft.is_symlink()) && path.extension().is_some_and(|e| e == "md") {
                std::fs::remove_file(&path).with_context(|| clean_err(&path))?;
            }
        }
    }
    std::fs::create_dir_all(&rc_dir)?;

    let task_md = render_task_md(task, budget, selected, diff_summary, explain);
    std::fs::write(out_dir.join("task.md"), &task_md)?;

    // relevant-code/{slug}.md per file — track seen slugs to resolve collisions
    let mut slug_counts: HashMap<String, usize> = HashMap::new();
    let mut used_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for c in selected {
        let base_slug: String = c
            .path
            .to_string_lossy()
            .chars()
            .map(|ch| {
                if ch.is_alphanumeric() || ch == '-' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        // Advance the per-base counter until the (uppercased, case-folded) slug is unused,
        // so generated `_N` slugs never collide with another base slug and
        // case-insensitive filesystems don't merge files either.
        let count = slug_counts.entry(base_slug.clone()).or_insert(0);
        let slug = loop {
            let candidate = make_slug(&base_slug, *count);
            *count += 1;
            if used_slugs.insert(candidate.to_uppercase()) {
                break candidate;
            }
        };

        let ext = c.path.extension().and_then(|e| e.to_str()).unwrap_or("txt");
        let fence = safe_fence(&c.content);
        let file_md = format!(
            "<!-- {} -->\n{fence}{ext}\n{}\n{fence}\n",
            comment_path(&c.path.to_string_lossy()),
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

    const HOSTILE_PATH: &str = "a`b``c\n## injected --> x\r\n<b>.rs";

    #[test]
    fn task_md_hostile_path_keeps_list_line_intact() {
        let md = render_task_md("t", 100, &[cand(HOSTILE_PATH, 0.5, "x")], "", false);
        let list: Vec<&str> = md
            .lines()
            .skip_while(|l| *l != "## Selected Files")
            .collect();
        // heading, blank, exactly one list line.
        assert_eq!(list.len(), 3, "path must not add lines; got:\n{md}");
        assert_eq!(
            list[2],
            "1. ```a`b``c\\n## injected --> x\\r\\n<b>.rs``` — 1 tokens"
        );
        assert!(!md.lines().any(|l| l.starts_with("## injected")));
    }

    #[test]
    fn relevant_code_hostile_path_comment_is_single_closed_line() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("bundle");
        write_bundle("t", 100, &[cand(HOSTILE_PATH, 0.5, "x")], "", &out, false).unwrap();
        let dir = out.join("relevant-code");
        let entries: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(entries.len(), 1);
        let md = std::fs::read_to_string(entries[0].as_ref().unwrap().path()).unwrap();
        let first = md.lines().next().unwrap();
        assert!(
            first.starts_with("<!-- ") && first.ends_with(" -->"),
            "got: {first}"
        );
        let inner = &first[5..first.len() - 4];
        assert!(!inner.contains("-->"), "comment closed early: {first}");
        assert!(md.lines().nth(1).unwrap().starts_with("```"), "got:\n{md}");
        assert!(!md.lines().any(|l| l.starts_with("## injected")));
    }

    #[test]
    fn inline_code_picks_longer_delimiter_and_escapes_newlines() {
        assert_eq!(inline_code("src/a.rs"), "`src/a.rs`");
        assert_eq!(inline_code("a`b"), "``a`b``");
        assert_eq!(inline_code("`a"), "`` `a ``");
        assert_eq!(inline_code("a\nb\rc"), "`a\\nb\\rc`");
        // A literal backslash-n in the name must differ from a real newline.
        assert_eq!(inline_code("a\\nb"), "`a\\\\nb`");
        assert_ne!(inline_code("a\\nb"), inline_code("a\nb"));
    }

    #[test]
    fn comment_path_escapes_terminator_and_newlines() {
        assert_eq!(comment_path("src/a.rs"), "src/a.rs");
        assert_eq!(comment_path("a-->b"), "a--&gt;b");
        assert_eq!(comment_path("a--!>b"), "a--!&gt;b");
        assert_eq!(comment_path("a\nb"), "a\\nb");
        // A literal `&gt;` in the name must differ from an escaped `>`.
        assert_eq!(comment_path("a&gt;b"), "a&amp;gt;b");
        assert_ne!(comment_path("a&gt;b"), comment_path("a>b"));
        assert_ne!(comment_path("a\\nb"), comment_path("a\nb"));
    }

    #[test]
    fn write_bundle_regeneration_removes_stale_md_only() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("bundle");
        let rc = out.join("relevant-code");
        write_bundle(
            "t1",
            1000,
            &[cand("src/old.rs", 0.9, "old")],
            "",
            &out,
            false,
        )
        .unwrap();
        std::fs::write(rc.join("keep.txt"), "x").unwrap();
        std::fs::create_dir_all(rc.join("sub")).unwrap();
        std::fs::write(rc.join("sub").join("nested.md"), "x").unwrap();
        std::fs::write(out.join("other.md"), "x").unwrap();

        write_bundle(
            "t2",
            1000,
            &[cand("src/new.rs", 0.9, "new")],
            "",
            &out,
            false,
        )
        .unwrap();

        assert!(
            !rc.join("src_old_rs.md").exists(),
            "stale md must be removed"
        );
        assert!(rc.join("src_new_rs.md").exists());
        assert!(rc.join("keep.txt").exists(), "non-md must remain");
        assert!(rc.join("sub").join("nested.md").exists(), "no recursion");
        assert!(
            out.join("other.md").exists(),
            "outside relevant-code must remain"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_bundle_removes_symlink_md_without_touching_target() {
        let tmp = tempfile::tempdir().unwrap();
        let external = tmp.path().join("external.txt");
        std::fs::write(&external, "precious").unwrap();
        let out = tmp.path().join("bundle");
        let rc = out.join("relevant-code");
        std::fs::create_dir_all(&rc).unwrap();
        std::os::unix::fs::symlink(&external, rc.join("src_a_rs.md")).unwrap();

        write_bundle("t", 1000, &[cand("src/a.rs", 0.9, "new")], "", &out, false).unwrap();

        assert_eq!(std::fs::read_to_string(&external).unwrap(), "precious");
        let md = rc.join("src_a_rs.md");
        assert!(!std::fs::symlink_metadata(&md)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(std::fs::read_to_string(&md).unwrap().contains("new"));
    }

    #[cfg(unix)]
    #[test]
    fn write_bundle_rejects_symlinked_relevant_code() {
        let tmp = tempfile::tempdir().unwrap();
        let victim = tmp.path().join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("a.md"), "x").unwrap();
        let out = tmp.path().join("bundle");
        std::fs::create_dir_all(&out).unwrap();
        std::os::unix::fs::symlink(&victim, out.join("relevant-code")).unwrap();

        let r = write_bundle("t", 1000, &[cand("src/a.rs", 0.9, "a")], "", &out, false);
        assert!(r.is_err());
        assert!(
            victim.join("a.md").exists(),
            "symlink target must be untouched"
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
    fn render_task_md_diff_with_codefence_uses_longer_fence() {
        let diff = "+```rust\n+fn x() {}\n+```\n## injected";
        let md = render_task_md("t", 1000, &[], diff, false);
        assert!(
            md.contains("\n````diff\n"),
            "diff fence must be longer than content run; got:\n{md}"
        );
        assert!(md.contains(diff), "diff must be preserved verbatim");
        assert!(md.ends_with("\n````\n"), "closing fence must match opener");
    }

    #[test]
    fn render_task_md_multiline_task_is_collapsed_to_one_line() {
        let md = render_task_md("line1\n\n## Injected\r\nline3", 1000, &[], "", false);
        assert!(
            md.contains("**Task:** line1 ## Injected line3\n"),
            "got:\n{md}"
        );
        assert!(!md.contains("\n## Injected"));
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

    fn count_files_and_contents(rc: &Path) -> (usize, Vec<String>) {
        let mut contents: Vec<String> = std::fs::read_dir(rc)
            .unwrap()
            .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
            .collect();
        contents.sort();
        (contents.len(), contents)
    }

    #[test]
    fn write_bundle_slug_generated_suffix_does_not_overwrite_other_base() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("bundle");
        // 2nd candidate gets `a_b_rs_0`; 3rd has that exact base slug.
        let selected = vec![
            cand("a/b.rs", 0.9, "one"),
            cand("a/b.rs", 0.8, "two"),
            cand("a_b_rs_0", 0.7, "three"),
        ];
        write_bundle("task", 10000, &selected, "", &out, false).unwrap();
        let (n, contents) = count_files_and_contents(&out.join("relevant-code"));
        assert_eq!(n, selected.len(), "no file may be overwritten");
        for word in ["one", "two", "three"] {
            assert!(contents.iter().any(|c| c.contains(word)), "{word} lost");
        }
    }

    #[test]
    fn write_bundle_slug_case_insensitive_collision_keeps_both() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("bundle");
        let selected = vec![cand("Foo.rs", 0.9, "upper"), cand("foo.rs", 0.8, "lower")];
        write_bundle("task", 10000, &selected, "", &out, false).unwrap();
        let (n, contents) = count_files_and_contents(&out.join("relevant-code"));
        assert_eq!(n, 2);
        assert!(contents.iter().any(|c| c.contains("upper")));
        assert!(contents.iter().any(|c| c.contains("lower")));
    }

    #[test]
    fn write_bundle_slug_reverse_order_keeps_all_three() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("bundle");
        let selected = vec![
            cand("a_b_rs_0", 0.9, "three"),
            cand("a/b.rs", 0.8, "one"),
            cand("a/b.rs", 0.7, "two"),
        ];
        write_bundle("task", 10000, &selected, "", &out, false).unwrap();
        let (n, contents) = count_files_and_contents(&out.join("relevant-code"));
        assert_eq!(n, 3, "no file may be overwritten");
        for word in ["one", "two", "three"] {
            assert!(contents.iter().any(|c| c.contains(word)), "{word} lost");
        }
    }

    #[test]
    fn write_bundle_slug_final_sigma_names_are_case_insensitively_distinct() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("bundle");
        let selected = vec![cand("ΑΣ.rs", 0.9, "x"), cand("ασ.rs", 0.8, "y")];
        write_bundle("task", 10000, &selected, "", &out, false).unwrap();
        let names: std::collections::HashSet<String> = std::fs::read_dir(out.join("relevant-code"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap().to_uppercase())
            .collect();
        assert_eq!(names.len(), 2, "names must differ ignoring case: {names:?}");
    }

    #[test]
    fn write_bundle_slug_sanitizes_special_chars() {
        // Paths containing null bytes, Windows reserved chars, and other
        // non-alphanumeric characters must be replaced by '_' so the resulting
        // filename is safe on all platforms.
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("bundle2");
        // Use a path string that contains characters the old denylist missed.
        let path = "src/foo\x00bar<baz>.rs";
        let selected = vec![cand(path, 0.9, "fn f() {}")];
        write_bundle("task", 10000, &selected, "", &out, false).unwrap();

        let rc = out.join("relevant-code");
        // The slug must not contain the null byte or '<'/'>' — every
        // non-alphanumeric/non-hyphen char is replaced by '_'.
        let entries: Vec<_> = std::fs::read_dir(&rc)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(entries.len(), 1);
        let name = &entries[0];
        assert!(
            !name.contains('\x00'),
            "slug must not contain null byte: {name:?}"
        );
        assert!(
            !name.contains('<') && !name.contains('>'),
            "slug must not contain angle brackets: {name:?}"
        );
        // All chars in the stem (before .md) must be alphanumeric, '-', or '_'.
        let stem = name.trim_end_matches(".md");
        assert!(
            stem.chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_'),
            "slug contains unexpected char: {name:?}"
        );
    }
}
