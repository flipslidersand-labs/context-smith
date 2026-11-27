//! `contextsmith index` prints a one-line summary on stdout:
//! `indexed: N files (A re-indexed, B skipped, C deleted), S symbols, D deps → path`.
//! Scripts rely on it, and a re-run is incremental while `--force` rebuilds everything.
//! These tests drive the real binary on a small temporary git repository and compare the
//! counts (not the whole line, which would break on any wording change).

use std::path::Path;
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("run git");
    assert!(st.success(), "git {args:?} failed");
}

fn commit_all(dir: &Path, msg: &str) {
    git(dir, &["add", "a.rs", "b.rs"]);
    git(dir, &["commit", "-q", "-m", msg]);
}

/// Repo with two tracked files (`a.rs`, `b.rs`), committed.
fn two_file_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q"]);
    // Repo-local, fictitious identity: independent of the global git config.
    git(d, &["config", "user.email", "test@example.invalid"]);
    git(d, &["config", "user.name", "Test User"]);
    git(d, &["config", "commit.gpgsign", "false"]);
    std::fs::write(d.join("a.rs"), "pub fn alpha() -> u32 { 1 }\n").unwrap();
    std::fs::write(d.join("b.rs"), "pub fn beta() -> u32 { 2 }\n").unwrap();
    commit_all(d, "init");
    tmp
}

/// The numbers in the `indexed:` line.
#[derive(Debug, PartialEq)]
struct Summary {
    files: usize,
    reindexed: usize,
    skipped: usize,
    deleted: usize,
}

/// The number written right before `label` (e.g. `label = "skipped"` in `... 2 skipped, ...`).
fn number_before(line: &str, label: &str) -> usize {
    let tokens: Vec<&str> = line
        .split(|c: char| !(c.is_alphanumeric() || c == '-'))
        .filter(|t| !t.is_empty())
        .collect();
    let i = tokens
        .iter()
        .position(|t| *t == label)
        .unwrap_or_else(|| panic!("no `{label}` in summary line: {line:?}"));
    tokens[i - 1]
        .parse()
        .unwrap_or_else(|_| panic!("no number before `{label}` in: {line:?}"))
}

fn index(repo: &Path, extra: &[&str]) -> Summary {
    let out: Output = Command::new(env!("CARGO_BIN_EXE_contextsmith"))
        .args(["index", "--repo", repo.to_str().unwrap()])
        .args(extra)
        .env_remove("GIT_DIR")
        .env_remove("EMBEDDING_SVC_URL")
        .env_remove("EMBEDDING_API_KEY")
        .output()
        .expect("run contextsmith");
    assert!(
        out.status.success(),
        "index {extra:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find(|l| l.starts_with("indexed: "))
        .unwrap_or_else(|| panic!("no `indexed:` line on stdout: {stdout:?}"));
    Summary {
        files: number_before(line, "files"),
        reindexed: number_before(line, "re-indexed"),
        skipped: number_before(line, "skipped"),
        deleted: number_before(line, "deleted"),
    }
}

#[test]
fn rerun_is_incremental_and_reports_changed_and_deleted_files() {
    let repo = two_file_repo();
    let d = repo.path();

    // (1) first run: everything is new.
    assert_eq!(
        index(d, &[]),
        Summary {
            files: 2,
            reindexed: 2,
            skipped: 0,
            deleted: 0
        },
        "first index"
    );

    // (2) nothing changed: everything is skipped.
    assert_eq!(
        index(d, &[]),
        Summary {
            files: 2,
            reindexed: 0,
            skipped: 2,
            deleted: 0
        },
        "re-run without changes"
    );

    // (3) one file changes and is committed: only that one is re-indexed.
    std::fs::write(d.join("a.rs"), "pub fn alpha() -> u32 { 100 }\n").unwrap();
    commit_all(d, "change a");
    assert_eq!(
        index(d, &[]),
        Summary {
            files: 2,
            reindexed: 1,
            skipped: 1,
            deleted: 0
        },
        "after changing one committed file"
    );

    // (5) one file is removed from git: it counts as deleted, the other stays skipped.
    git(d, &["rm", "-q", "b.rs"]);
    git(d, &["commit", "-q", "-m", "drop b"]);
    assert_eq!(
        index(d, &[]),
        Summary {
            files: 1,
            reindexed: 0,
            skipped: 1,
            deleted: 1
        },
        "after git rm of one file"
    );

    // The deletion is reported once: a further run sees nothing to delete.
    assert_eq!(
        index(d, &[]),
        Summary {
            files: 1,
            reindexed: 0,
            skipped: 1,
            deleted: 0
        },
        "re-run after the deletion was recorded"
    );
}

#[test]
fn force_reindexes_every_file_even_when_nothing_changed() {
    let repo = two_file_repo();
    let d = repo.path();
    index(d, &[]);
    // Without --force the second run skips both files (guards the premise of this test).
    assert_eq!(
        index(d, &[]).skipped,
        2,
        "premise: unchanged files are skipped"
    );

    // (4) --force: nothing is skipped.
    assert_eq!(
        index(d, &["--force"]),
        Summary {
            files: 2,
            reindexed: 2,
            skipped: 0,
            deleted: 0
        },
        "index --force"
    );

    // --force does not stick: the next plain run is incremental again.
    assert_eq!(
        index(d, &[]),
        Summary {
            files: 2,
            reindexed: 0,
            skipped: 2,
            deleted: 0
        },
        "plain run after --force"
    );
}
