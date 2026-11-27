use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::budget::{allocate, estimate_tokens, truncate_to_tokens, Candidate};
use crate::path_kind::escape_control;

#[derive(Serialize)]
pub struct Citation {
    pub path: PathBuf,
    pub score: f32,
    pub tokens: usize,
    pub reason: String,
    pub truncated: bool,
}

/// `[truncated: first N of M tokens]` note for a budget-cut file, or `None` if whole.
pub fn truncation_note(c: &Candidate) -> Option<String> {
    c.truncated.then(|| {
        format!(
            "[truncated: first {} of {} tokens]",
            c.tokens(),
            c.original_tokens
        )
    })
}

/// Label used for the recent diff entry in `citations.json`.
pub const DIFF_CITATION_PATH: &str = "(recent diff)";

/// The recent-diff section of `task.md`, fitted to the remaining token budget.
#[derive(Debug, Clone, Default)]
pub struct DiffPart {
    /// Rendered Markdown section (empty when nothing is shown).
    pub text: String,
    /// Estimated tokens of `text` (counted against the budget).
    pub tokens: usize,
    /// Estimated tokens of the full diff before any cut.
    pub original_tokens: usize,
    /// True when the diff was cut or dropped to fit.
    pub truncated: bool,
}

fn diff_section(body: &str, note: Option<&str>) -> String {
    let mut s = String::from("\n## Recent Diff\n\n");
    if !body.is_empty() {
        let fence = safe_fence(body);
        s.push_str(&format!("{fence}diff\n{body}\n{fence}\n"));
    }
    if let Some(n) = note {
        if !body.is_empty() {
            s.push('\n');
        }
        s.push_str(n);
        s.push('\n');
    }
    s
}

fn omitted_note(diff: &str) -> String {
    format!(
        "[diff omitted: 0 of {} tokens fit in the budget]",
        estimate_tokens(diff)
    )
}

/// Fit the recent diff into `remaining` tokens (the whole section, including
/// heading, fences and note, is counted). A cut diff is trimmed to whole lines and
/// followed by a note outside the fence, so the fence stays intact.
pub fn fit_diff(diff: &str, remaining: usize) -> DiffPart {
    if diff.is_empty() {
        return DiffPart::default();
    }
    let original_tokens = estimate_tokens(diff);
    let full = diff_section(diff, None);
    let full_tokens = estimate_tokens(&full);
    if full_tokens <= remaining {
        return DiffPart {
            text: full,
            tokens: full_tokens,
            original_tokens,
            truncated: false,
        };
    }
    let mut cap = remaining;
    loop {
        let mut body = truncate_to_tokens(diff, cap);
        // Cut back to a line boundary so a half line is not shown.
        match body.rfind('\n') {
            Some(i) => body.truncate(i),
            None => body.clear(),
        }
        let note = if body.is_empty() {
            omitted_note(diff)
        } else {
            format!(
                "[diff truncated: first {} of {original_tokens} tokens]",
                estimate_tokens(&body)
            )
        };
        let text = diff_section(&body, Some(&note));
        let tokens = estimate_tokens(&text);
        if tokens <= remaining {
            return DiffPart {
                text,
                tokens,
                original_tokens,
                truncated: true,
            };
        }
        if cap == 0 {
            // Even the bare note does not fit: still show it (never cut silently);
            // the overshoot is visible in `Used`.
            return DiffPart {
                tokens,
                text,
                original_tokens,
                truncated: true,
            };
        }
        cap = (cap - 1).min(cap * 9 / 10);
    }
}

/// Render the `task.md` document (task description, selected file list, and the
/// recent diff) as a String. Shared by [`write_bundle`] and the `query` command
/// so both produce identical Markdown. The diff counts toward `budget` and is cut
/// to fit what the selected files leave over.
pub fn render_task_md(
    task: &str,
    budget: usize,
    selected: &[Candidate],
    diff_summary: &str,
    explain: bool,
) -> String {
    let files_used: usize = selected.iter().map(|c| c.tokens()).sum();
    let diff = fit_diff(diff_summary, budget.saturating_sub(files_used));
    let used = files_used + diff.tokens;
    let mut task_md = render_head(task, budget, used, selected, explain);
    task_md.push_str(&diff.text);
    task_md
}

/// Header + selected-file list of `task.md` (everything except the diff section).
fn render_head(
    task: &str,
    budget: usize,
    used: usize,
    selected: &[Candidate],
    explain: bool,
) -> String {
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
        let note = truncation_note(c).map_or(String::new(), |n| format!(" {n}"));
        task_md.push_str(&format!(
            "{}. {}{} — {} tokens{}\n",
            i + 1,
            inline_code(&c.path.to_string_lossy()),
            explain_str,
            c.tokens(),
            note,
        ));
    }
    task_md
}

/// Markdown of one `relevant-code/*.md` file.
fn render_file_md(c: &Candidate) -> String {
    let ext = fence_info(&c.path);
    let fence = safe_fence(&c.content);
    let mut file_md = format!(
        "<!-- {} -->\n{fence}{ext}\n{}\n{fence}\n",
        comment_path(&c.path.to_string_lossy()),
        c.content,
    );
    if let Some(note) = truncation_note(c) {
        file_md.push_str(&format!("\n{note}\n"));
    }
    file_md
}

/// The fence info string for a file: its extension limited to `[A-Za-z0-9_+#-]`. A tracked file
/// name may contain a backtick (which would end a backtick fence's info string) or a newline,
/// so anything else is dropped; an empty result falls back to `txt`.
fn fence_info(path: &std::path::Path) -> String {
    let ext: String = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '#' | '-'))
        .collect();
    if ext.is_empty() {
        "txt".to_string()
    } else {
        ext
    }
}

/// Shrink `selected` so its file bodies total at most `eff` tokens, keeping the
/// original (pre-cut) token counts of already-truncated files.
fn shrink_selection(selected: &[Candidate], eff: usize) -> Vec<Candidate> {
    if eff == 0 {
        return Vec::new();
    }
    if selected.iter().map(|c| c.tokens()).sum::<usize>() <= eff {
        return selected.to_vec();
    }
    allocate(selected.to_vec(), eff)
        .into_iter()
        .map(|mut c| {
            if let Some(prev) = selected
                .iter()
                .find(|p| p.file_id == c.file_id && p.path == c.path)
            {
                if prev.truncated {
                    c.original_tokens = prev.original_tokens;
                }
            }
            c
        })
        .collect()
}

/// A fully rendered bundle whose `task.md` plus `relevant-code/*.md` fit `budget`
/// (unless the fixed overhead alone exceeds it; then bodies and diff are empty and
/// `used` reports the real overshoot).
struct Rendered {
    task_md: String,
    selected: Vec<Candidate>,
    file_mds: Vec<String>,
    diff: DiffPart,
    used: usize,
}

fn render_bundle(
    task: &str,
    budget: usize,
    selected: &[Candidate],
    diff_summary: &str,
    explain: bool,
) -> Rendered {
    let file_tokens = |sel: &[Candidate]| -> usize {
        sel.iter()
            .map(|c| estimate_tokens(&render_file_md(c)))
            .sum()
    };
    // Fixed overhead (header, list, per-file wrappers) is charged before the file
    // bodies: lower the body budget until head + files fit.
    let mut eff = selected
        .iter()
        .map(|c| c.tokens())
        .sum::<usize>()
        .min(budget);
    // Reserve room for the "diff omitted" note so a dropped diff is never silent.
    let reserve_text = if diff_summary.is_empty() {
        String::new()
    } else {
        diff_section("", Some(&omitted_note(diff_summary)))
    };
    let (sel, fixed) = loop {
        let sel = shrink_selection(selected, eff);
        let head = render_head(task, budget, budget, &sel, explain);
        let files = file_tokens(&sel);
        let fixed = estimate_tokens(&head) + files;
        // Measured on the concatenated text: estimates are not additive across strings.
        let with_reserve = estimate_tokens(&format!("{head}{reserve_text}")) + files;
        if with_reserve <= budget || eff == 0 {
            break (sel, fixed);
        }
        eff = eff.saturating_sub((with_reserve - budget).max(1));
    };
    let file_mds: Vec<String> = sel.iter().map(render_file_md).collect();

    // Fit the diff into what is left; re-check the whole because token estimates
    // are not exactly additive across strings.
    let mut remaining = budget.saturating_sub(fixed);
    loop {
        let diff = fit_diff(diff_summary, remaining);
        let mut used = budget;
        let mut task_md = String::new();
        for _ in 0..8 {
            task_md = render_head(task, budget, used, &sel, explain);
            task_md.push_str(&diff.text);
            let actual = estimate_tokens(&task_md) + file_tokens(&sel);
            if actual == used {
                break;
            }
            used = actual;
        }
        if used <= budget || remaining == 0 {
            return Rendered {
                task_md,
                selected: sel,
                file_mds,
                diff,
                used,
            };
        }
        remaining = remaining.saturating_sub((used - budget).max(1));
    }
}

/// Render a path as a single-line Markdown inline code span. The delimiter is
/// longer than the longest backtick run in the path (CommonMark code spans), and
/// a space pads content that starts or ends with a backtick.
fn inline_code(p: &str) -> String {
    let p = escape_control(p);
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
    escape_control(p).replace('&', "&amp;").replace('>', "&gt;")
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

/// Max bytes of a base slug. Leaves room for `_N` and `.md` under the 255-byte
/// file name limit of common filesystems.
const MAX_BASE_SLUG_BYTES: usize = 200;

/// Sanitize a path into a base slug, cut to `MAX_BASE_SLUG_BYTES` on a char
/// boundary. Paths that collide after cutting get `_N` from `make_slug`.
fn base_slug(path: &str) -> String {
    let mut slug = String::new();
    for ch in path.chars() {
        let ch = if ch.is_alphanumeric() || ch == '-' {
            ch
        } else {
            '_'
        };
        if slug.len() + ch.len_utf8() > MAX_BASE_SLUG_BYTES {
            break;
        }
        slug.push(ch);
    }
    slug
}

/// Return the shortest backtick fence string (minimum length 3) that does not
/// appear as a standalone fence sequence in `content`.
///
/// GFM spec §4.5: a code fence opening consists of ≥3 backticks; a closing
/// fence must be at least as long as the opener.  By using one more backtick
/// than the longest consecutive run of backticks found in the content we
/// guarantee the fence cannot be prematurely closed by content.
pub(crate) fn safe_fence(content: &str) -> String {
    let max_run = longest_backtick_run(content);
    let len = max_run.max(2) + 1; // at least 3 backticks
    "`".repeat(len)
}

/// Bail when `meta` (from `symlink_metadata` of `shown`) is a symlink; `hint` is appended to the message.
fn bail_if_symlink(meta: &std::fs::Metadata, shown: &Path, hint: &str) -> Result<()> {
    if meta.file_type().is_symlink() {
        anyhow::bail!(
            "{} is a symlink; refusing to write bundle{hint}",
            shown.display()
        );
    }
    Ok(())
}

/// Write `contents` to `path`, refusing if `path` is a symlink (dangling or not).
///
/// The contents go to a sibling temp file (opened with `create_new`, i.e. `O_EXCL`, so a
/// link planted at the temp name is never followed), which is then renamed over `path`.
/// `rename` replaces a link at `path` instead of following it, and a failed write (disk
/// full, ...) leaves the previous file intact. An existing file's permissions are kept.
/// Remaining limitations: a concurrent attacker can still win a race on the parent
/// directory, and hard links are not detected (both need a local writer).
fn write_file_no_symlink(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    let existing = match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            bail_if_symlink(&meta, path, "")?;
            Some(meta)
        }
        Err(_) => None,
    };
    let name = path
        .file_name()
        .context("bundle output path has no file name")?
        .to_string_lossy();
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        if let Some(meta) = &existing {
            f.set_permissions(meta.permissions())?;
        }
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Remove `*.md` files in `rc_dir` that this run did not write (leftovers of a previous
/// run, #192/#218). Regular files and symlinks only, no recursion; `remove_file` deletes
/// a symlink itself, never its target. Runs after all writes so that a failed write
/// leaves the previous bundle intact instead of an empty directory.
fn remove_stale_md(
    rc_dir: &Path,
    written: &std::collections::HashSet<std::ffi::OsString>,
) -> Result<()> {
    let clean_err = |p: &Path| format!("failed to clean {}", p.display());
    for entry in std::fs::read_dir(rc_dir).with_context(|| clean_err(rc_dir))? {
        let entry = entry.with_context(|| clean_err(rc_dir))?;
        let path = entry.path();
        let ft = entry.file_type().with_context(|| clean_err(&path))?;
        if (ft.is_file() || ft.is_symlink())
            && path.extension().is_some_and(|e| e == "md")
            && !written.contains(&entry.file_name())
        {
            std::fs::remove_file(&path).with_context(|| clean_err(&path))?;
        }
    }
    Ok(())
}

/// Written bundle summary.
#[derive(Debug, Clone, Copy)]
pub struct BundleStats {
    /// Estimated tokens of `task.md` plus all `relevant-code/*.md` (= the `Used` shown).
    pub used: usize,
    /// Number of files written to `relevant-code/`.
    pub files: usize,
}

/// Write a context bundle to `out_dir`. Everything written (`task.md` and
/// `relevant-code/`) is counted against `budget`; the selection is shrunk, and the
/// diff cut, to fit.
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
) -> Result<BundleStats> {
    // Refuse a symlinked --out (final component only; parents may legitimately be links,
    // e.g. /tmp on macOS). `components()` drops a trailing slash so `link/` is checked too.
    let out_checked: std::path::PathBuf = out_dir.components().collect();
    if let Ok(meta) = std::fs::symlink_metadata(&out_checked) {
        bail_if_symlink(&meta, out_dir, " (pass the real path to --out instead)")?;
    }
    std::fs::create_dir_all(out_dir)?;
    let rc_dir = out_dir.join("relevant-code");
    // Refuse to follow a symlinked relevant-code/ (never delete outside out_dir).
    // Stale files from a previous run are removed only after everything is written (see
    // `remove_stale_md`), so a failed write leaves the previous bundle in place.
    if let Ok(meta) = std::fs::symlink_metadata(&rc_dir) {
        bail_if_symlink(&meta, &rc_dir, "")?;
    }
    std::fs::create_dir_all(&rc_dir)?;

    let Rendered {
        task_md,
        selected,
        file_mds,
        diff,
        used,
    } = render_bundle(task, budget, selected, diff_summary, explain);
    let selected = &selected[..];
    write_file_no_symlink(&out_dir.join("task.md"), &task_md)?;

    // relevant-code/{slug}.md per file — track seen slugs to resolve collisions
    let mut slug_counts: HashMap<String, usize> = HashMap::new();
    let mut used_slugs: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut written: std::collections::HashSet<std::ffi::OsString> =
        std::collections::HashSet::new();
    for (c, file_md) in selected.iter().zip(&file_mds) {
        let base_slug = base_slug(&c.path.to_string_lossy());
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

        let file_name = format!("{slug}.md");
        let target = rc_dir.join(&file_name);
        // A symlinked .md left in relevant-code/ is generated-directory debris (#218):
        // drop the link itself (never its target) so the new file can take its name.
        if std::fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
            std::fs::remove_file(&target)
                .with_context(|| format!("failed to clean {}", target.display()))?;
        }
        write_file_no_symlink(&target, file_md)?;
        written.insert(file_name.into());
    }

    // citations.json
    let mut citations: Vec<Citation> = selected
        .iter()
        .map(|c| Citation {
            path: c.path.clone(),
            score: c.score,
            tokens: c.tokens(),
            reason: format!("relevance_score={:.3}", c.score),
            truncated: c.truncated,
        })
        .collect();
    if diff.original_tokens > 0 {
        citations.push(Citation {
            path: PathBuf::from(DIFF_CITATION_PATH),
            score: 0.0,
            tokens: diff.tokens,
            reason: "recent_diff".to_string(),
            truncated: diff.truncated,
        });
    }
    let json = serde_json::to_string_pretty(&citations)?;
    write_file_no_symlink(&out_dir.join("citations.json"), &json)?;

    remove_stale_md(&rc_dir, &written)?;

    Ok(BundleStats {
        used,
        files: selected.len(),
    })
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
            truncated: false,
            original_tokens: 0,
        }
    }

    #[test]
    fn fence_info_keeps_plain_extensions_and_drops_unsafe_characters() {
        use std::path::Path;
        assert_eq!(fence_info(Path::new("a/b.rs")), "rs");
        assert_eq!(fence_info(Path::new("a/b.c++")), "c++");
        assert_eq!(fence_info(Path::new("a/b.r`s")), "rs");
        assert_eq!(fence_info(Path::new("a/b.r\ns")), "rs");
        assert_eq!(fence_info(Path::new("a/b.`")), "txt");
        assert_eq!(fence_info(Path::new("Makefile")), "txt");
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
    fn rendered_bundle_has_no_raw_control_characters_from_paths() {
        // ESC/BEL (OSC title + SGR), DEL, C1 (CSI 0x9b), U+2028/2029 in one name.
        let name = "a\u{1b}]0;PWNED\u{7}\u{1b}[31m\u{7f}\u{9b}2K\u{2028}\u{2029}.rs";
        let c = cand(name, 0.9, "fn x() {}");
        let bad = |s: &str| {
            s.chars()
                .any(|ch| ch != '\n' && crate::path_kind::is_ctrl(ch))
        };

        let task_md = render_task_md("t", 1000, std::slice::from_ref(&c), "", false);
        assert!(!bad(&task_md), "task.md: {task_md:?}");
        assert!(task_md.contains("\\x1b]0;PWNED\\x07"), "{task_md:?}");

        let file_md = render_file_md(&c);
        let first = file_md.lines().next().unwrap();
        assert!(
            first.starts_with("<!-- ") && first.ends_with(" -->"),
            "{first:?}"
        );
        assert!(!bad(first), "<!-- path -->: {first:?}");

        let tmp = tempfile::tempdir().unwrap();
        write_bundle("t", 1000, std::slice::from_ref(&c), "", tmp.path(), false).unwrap();
        let mut seen = 0;
        for e in std::fs::read_dir(tmp.path().join("relevant-code")).unwrap() {
            let md = std::fs::read_to_string(e.unwrap().path()).unwrap();
            let head = md.lines().next().unwrap();
            assert!(!bad(head), "{head:?}");
            seen += 1;
        }
        assert_eq!(seen, 1);
        let task_md = std::fs::read_to_string(tmp.path().join("task.md")).unwrap();
        assert!(!bad(&task_md), "{task_md:?}");
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

    #[test]
    fn write_bundle_failed_write_keeps_previous_bundle() {
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
        // A directory at the new file's name makes the rename of the new file fail
        // (no chmod: CI may run as root).
        std::fs::create_dir_all(rc.join("src_new_rs.md")).unwrap();

        let r = write_bundle(
            "t2",
            1000,
            &[cand("src/new.rs", 0.9, "new")],
            "",
            &out,
            false,
        );

        assert!(r.is_err(), "write into a blocked name must fail");
        assert!(
            std::fs::read_to_string(rc.join("src_old_rs.md"))
                .unwrap()
                .contains("old"),
            "previous bundle must survive a failed write"
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

    #[cfg(unix)]
    fn assert_symlinked_output_rejected(name: &str) {
        let tmp = tempfile::tempdir().unwrap();
        let external = tmp.path().join("external.txt");
        std::fs::write(&external, "precious").unwrap();
        let out = tmp.path().join("bundle");
        std::fs::create_dir_all(&out).unwrap();
        std::os::unix::fs::symlink(&external, out.join(name)).unwrap();

        let r = write_bundle("t", 1000, &[cand("src/a.rs", 0.9, "a")], "", &out, false);
        let err = r.expect_err("symlinked output file must be rejected");
        assert!(err.to_string().contains("is a symlink"), "{err}");
        assert_eq!(std::fs::read_to_string(&external).unwrap(), "precious");
    }

    #[cfg(unix)]
    #[test]
    fn write_bundle_rejects_symlinked_task_md() {
        assert_symlinked_output_rejected("task.md");
    }

    #[cfg(unix)]
    #[test]
    fn write_bundle_rejects_symlinked_citations_json() {
        assert_symlinked_output_rejected("citations.json");
    }

    #[cfg(unix)]
    #[test]
    fn write_bundle_rejects_dangling_symlinked_task_md() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing.txt");
        let out = tmp.path().join("bundle");
        std::fs::create_dir_all(&out).unwrap();
        std::os::unix::fs::symlink(&missing, out.join("task.md")).unwrap();

        let r = write_bundle("t", 1000, &[cand("src/a.rs", 0.9, "a")], "", &out, false);
        assert!(r.is_err());
        assert!(!missing.exists(), "dangling target must not be created");
    }

    #[cfg(unix)]
    #[test]
    fn write_bundle_rejects_symlinked_out_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let out = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &out).unwrap();

        for target in [out.clone(), tmp.path().join("link/")] {
            let r = write_bundle("t", 1000, &[cand("src/a.rs", 0.9, "a")], "", &target, false);
            let err = r.expect_err("symlinked --out must be rejected");
            assert!(err.to_string().contains("is a symlink"), "{err}");
            assert!(err.to_string().contains("real path"), "{err}");
        }
        assert!(!real.join("task.md").exists());
        assert!(!real.join("relevant-code").exists());
        assert!(!real.join("citations.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn write_bundle_rejects_dangling_symlinked_out_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing");
        let out = tmp.path().join("link");
        std::os::unix::fs::symlink(&missing, &out).unwrap();

        let r = write_bundle("t", 1000, &[cand("src/a.rs", 0.9, "a")], "", &out, false);
        let msg = format!("{:#}", r.unwrap_err());
        assert!(msg.contains("is a symlink"), "unexpected error: {msg}");
        assert!(!missing.exists());
    }

    #[test]
    fn write_file_no_symlink_replaces_existing_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f.txt");
        std::fs::write(&p, "old old old").unwrap();
        write_file_no_symlink(&p, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn write_file_no_symlink_keeps_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("task.md");
        std::fs::write(&p, "old").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_file_no_symlink(&p, "new").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "rewrite must not widen permissions (#617)");
    }

    #[test]
    fn write_file_no_symlink_leaves_no_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("f.txt");
        write_file_no_symlink(&p, "one").unwrap();
        write_file_no_symlink(&p, "two").unwrap();
        let names: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["f.txt".to_string()], "temp file left behind");
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

    fn slug_file_names(out: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(out.join("relevant-code"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn write_bundle_long_ascii_path_is_truncated_under_255_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("bundle");
        let path = format!("{}/lib.rs", "d".repeat(300));
        write_bundle("t", 1000, &[cand(&path, 0.9, "x")], "", &out, false).unwrap();
        let names = slug_file_names(&out);
        assert_eq!(names.len(), 1);
        assert!(names[0].len() <= 255, "len={}", names[0].len());
    }

    #[test]
    fn write_bundle_long_multibyte_path_is_cut_on_char_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("bundle");
        // 3 bytes per char: 100 chars = 300 bytes.
        let path = format!("{}/lib.rs", "日".repeat(100));
        write_bundle("t", 1000, &[cand(&path, 0.9, "x")], "", &out, false).unwrap();
        let names = slug_file_names(&out);
        assert_eq!(names.len(), 1);
        assert!(names[0].len() <= 255, "len={}", names[0].len());
        assert!(names[0].starts_with('日'));
    }

    #[test]
    fn write_bundle_long_paths_colliding_after_truncation_stay_distinct() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("bundle");
        let prefix = "d".repeat(300);
        let selected = vec![
            cand(&format!("{prefix}/a.rs"), 0.9, "a"),
            cand(&format!("{prefix}/b.rs"), 0.8, "b"),
            cand(&format!("{}/c.rs", prefix.to_uppercase()), 0.7, "c"),
        ];
        write_bundle("t", 1000, &selected, "", &out, false).unwrap();
        let names = slug_file_names(&out);
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(names.iter().all(|n| n.len() <= 255), "{names:?}");
    }

    #[test]
    fn base_slug_keeps_exactly_200_bytes_and_cuts_201() {
        assert_eq!(base_slug(&"a".repeat(200)), "a".repeat(200));
        assert_eq!(base_slug(&"a".repeat(201)), "a".repeat(200));
        // Non-alphanumerics become `_` (1 byte) and count toward the limit.
        assert_eq!(base_slug(&"/".repeat(201)), "_".repeat(200));
    }

    #[test]
    fn base_slug_multibyte_char_straddling_limit_is_dropped_whole() {
        for ch in ['é', '日', '𝐀'] {
            let w = ch.len_utf8();
            assert!(ch.is_alphanumeric(), "{ch} must be kept as-is");
            // Place the char so it starts before byte 200 and ends after it.
            for pad in (200 - w + 1)..200 {
                let input = format!("{}{ch}{ch}", "a".repeat(pad));
                let slug = base_slug(&input);
                assert_eq!(slug, "a".repeat(pad), "ch={ch} pad={pad}");
                assert!(slug.len() <= 200);
            }
            // Char ending exactly at byte 200 is kept.
            let input = format!("{}{ch}{ch}", "a".repeat(200 - w));
            let slug = base_slug(&input);
            assert_eq!(slug.len(), 200, "ch={ch}");
            assert!(slug.ends_with(ch));
        }
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

    #[test]
    fn truncated_file_gets_note_in_task_md_file_md_and_citations() {
        let mut c = cand("src/a.rs", 1.0, &"a".repeat(400));
        c.truncated = true;
        c.original_tokens = 300;
        let whole = cand("src/b.rs", 0.5, "fn b() {}");
        let dir = tempfile::tempdir().unwrap();
        write_bundle("t", 1000, &[c, whole], "", dir.path(), false).unwrap();

        let note = "[truncated: first 100 of 300 tokens]";
        let task_md = std::fs::read_to_string(dir.path().join("task.md")).unwrap();
        assert!(task_md.contains(note), "{task_md}");
        assert_eq!(task_md.matches("[truncated").count(), 1);

        let a = std::fs::read_to_string(dir.path().join("relevant-code/src_a_rs.md")).unwrap();
        assert!(a.trim_end().ends_with(note), "{a}");
        let b = std::fs::read_to_string(dir.path().join("relevant-code/src_b_rs.md")).unwrap();
        assert!(!b.contains("truncated"));

        let cj = std::fs::read_to_string(dir.path().join("citations.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&cj).unwrap();
        assert_eq!(v[0]["truncated"], true);
        assert_eq!(v[1]["truncated"], false);
    }

    fn big_diff() -> String {
        (0..20000)
            .map(|i| format!("+line {i} of a very large diff\n"))
            .collect()
    }

    /// Tokens of everything written: task.md + every relevant-code/*.md.
    fn written_tokens(out: &Path) -> usize {
        let mut n = estimate_tokens(&std::fs::read_to_string(out.join("task.md")).unwrap());
        for e in std::fs::read_dir(out.join("relevant-code")).unwrap() {
            n += estimate_tokens(&std::fs::read_to_string(e.unwrap().path()).unwrap());
        }
        n
    }

    fn many_files() -> Vec<Candidate> {
        (0..6)
            .map(|i| {
                cand(
                    &format!("src/module_{i}.rs"),
                    1.0 - i as f32 * 0.1,
                    &"fn f() { /* body */ }\n".repeat(40),
                )
            })
            .collect()
    }

    #[test]
    fn build_output_total_fits_budget() {
        for budget in [1000usize, 300] {
            let tmp = tempfile::tempdir().unwrap();
            let out = tmp.path().join("b");
            let sel = crate::budget::allocate(many_files(), budget);
            write_bundle("t", budget, &sel, &big_diff(), &out, false).unwrap();
            let total = written_tokens(&out);
            assert!(total <= budget, "budget={budget} total={total}");
            let md = std::fs::read_to_string(out.join("task.md")).unwrap();
            assert!(
                md.contains(&format!("**Used:** {total} tokens")),
                "Used must equal the real total {total}; got:\n{md}"
            );
        }
    }

    #[test]
    fn build_dropped_diff_is_noted_in_task_md() {
        // Files use up nearly the whole budget, so no diff body fits: task.md must
        // still say the diff was omitted (never a silent cut).
        for budget in (100usize..2100).step_by(53).chain([300, 1000, 2000]) {
            let tmp = tempfile::tempdir().unwrap();
            let out = tmp.path().join("b");
            let sel = crate::budget::allocate(many_files(), budget);
            write_bundle("t", budget, &sel, &big_diff(), &out, false).unwrap();
            let md = std::fs::read_to_string(out.join("task.md")).unwrap();
            assert!(md.contains("## Recent Diff"), "budget={budget}:\n{md}");
            assert!(
                md.contains("[diff omitted:") || md.contains("[diff truncated:"),
                "budget={budget}:\n{md}"
            );
            assert!(written_tokens(&out) <= budget, "budget={budget}");
            let json = std::fs::read_to_string(out.join("citations.json")).unwrap();
            assert!(json.contains(DIFF_CITATION_PATH));
        }
    }

    #[test]
    fn build_overhead_above_budget_reports_real_overshoot() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("b");
        let sel = crate::budget::allocate(many_files(), 10);
        write_bundle("t", 10, &sel, &big_diff(), &out, false).unwrap();
        let md = std::fs::read_to_string(out.join("task.md")).unwrap();
        let total = written_tokens(&out);
        assert!(total > 10);
        assert!(md.contains(&format!("**Used:** {total} tokens")), "{md}");
        assert!(!md.contains("```"), "no diff body expected:\n{md}");
        assert!(!out.join("relevant-code/src_module_0_rs.md").exists());
    }

    #[test]
    fn build_diff_is_cut_with_note_and_balanced_fence() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("b");
        let sel = vec![cand("src/a.rs", 0.9, "fn a() {}")];
        write_bundle("t", 400, &sel, &big_diff(), &out, false).unwrap();
        let md = std::fs::read_to_string(out.join("task.md")).unwrap();
        assert!(md.contains("[diff truncated: first"), "{md}");
        assert_eq!(md.matches("```").count(), 2, "fence must stay balanced");
        let json = std::fs::read_to_string(out.join("citations.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let last = v.as_array().unwrap().last().unwrap();
        assert_eq!(last["path"], DIFF_CITATION_PATH);
        assert_eq!(last["truncated"], true);
        assert!(written_tokens(&out) <= 400);
    }

    #[test]
    fn fit_diff_small_diff_is_kept_whole() {
        let p = fit_diff("+a\n+b", 1000);
        assert!(!p.truncated);
        assert!(p.text.contains("+a\n+b"));
        assert_eq!(p.tokens, estimate_tokens(&p.text));
    }

    #[test]
    fn fit_diff_fits_remaining_and_never_drops_silently() {
        let diff = big_diff();
        let note_cost = estimate_tokens(&diff_section("", Some(&omitted_note(&diff))));
        for rem in [0usize, 1, 5, 10, 30, 100, 1000] {
            let p = fit_diff(&diff, rem);
            assert!(p.truncated);
            assert!(p.text.contains("[diff "), "rem={rem}: note missing");
            if rem >= note_cost {
                assert!(p.tokens <= rem, "rem={rem} tokens={}", p.tokens);
            }
        }
    }

    #[test]
    fn citation_reason_does_not_claim_raw_bm25() {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(
            "t",
            1000,
            &[cand("src/a.rs", 0.5, "fn a() {}")],
            "",
            dir.path(),
            false,
        )
        .unwrap();
        let cj = std::fs::read_to_string(dir.path().join("citations.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&cj).unwrap();
        assert_eq!(v[0]["reason"], "relevance_score=0.500");
    }
}
