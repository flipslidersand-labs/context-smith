//! `index --out <path>` must create the missing parent directories of <path>, so that an index
//! can be kept outside the repository (e.g. in a cache directory) with a single command.
//! Before, it failed with "unable to open database file" and the caller had to `mkdir -p` first.

use std::path::Path;
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(st.success(), "git {args:?} failed");
}

fn bin(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_contextsmith"))
        .args(args)
        .output()
        .expect("run contextsmith")
}

fn committed_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q"]);
    git(d, &["config", "user.email", "t@example.com"]);
    git(d, &["config", "user.name", "t"]);
    std::fs::write(d.join("lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
    git(d, &["add", "lib.rs"]);
    git(d, &["commit", "-q", "-m", "init"]);
    tmp
}

#[test]
fn out_creates_missing_parent_directories() {
    let repo = committed_repo();
    let cache = tempfile::tempdir().unwrap();
    let db = cache.path().join("a/b/c/index.db"); // none of a/b/c exists
    let out = bin(&[
        "index",
        "--repo",
        repo.path().to_str().unwrap(),
        "--out",
        db.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "index --out failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(db.is_file(), "index was not written to {}", db.display());
}

#[test]
fn out_does_not_touch_the_repository() {
    let repo = committed_repo();
    let cache = tempfile::tempdir().unwrap();
    let db = cache.path().join("nested/index.db");
    let out = bin(&[
        "index",
        "--repo",
        repo.path().to_str().unwrap(),
        "--out",
        db.to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        status.stdout.is_empty(),
        "repository was modified: {}",
        String::from_utf8_lossy(&status.stdout)
    );
}

#[test]
fn empty_out_is_rejected() {
    let repo = committed_repo();
    let out = bin(&[
        "index",
        "--repo",
        repo.path().to_str().unwrap(),
        "--out",
        "",
    ]);
    assert!(!out.status.success(), "index --out \"\" must fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("must not be empty"), "stderr: {err}");
}

/// #494: `query --index` with an index built for another repository warns on stderr
/// (and still runs); the matching repository stays silent.
#[test]
fn query_warns_when_index_was_built_for_another_repo() {
    let a = committed_repo();
    let b = committed_repo();
    let cache = tempfile::tempdir().unwrap();
    let db = cache.path().join("index.db");
    let out = bin(&[
        "index",
        "--repo",
        a.path().to_str().unwrap(),
        "--out",
        db.to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let query = |repo: &Path| {
        bin(&[
            "query",
            "--repo",
            repo.to_str().unwrap(),
            "--index",
            db.to_str().unwrap(),
            "--task",
            "answer",
        ])
    };
    let same = query(a.path());
    assert!(
        !String::from_utf8_lossy(&same.stderr).contains("index was built for"),
        "{}",
        String::from_utf8_lossy(&same.stderr)
    );
    let other = query(b.path());
    assert!(
        String::from_utf8_lossy(&other.stderr).contains("index was built for"),
        "{}",
        String::from_utf8_lossy(&other.stderr)
    );
}

fn unborn_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    git(tmp.path(), &["init", "-q"]);
    tmp
}

/// A repo with no commits makes `index` fail. It must not leave an empty `index.db`
/// behind: `query` would then open it and say "No matching files found" instead of
/// "Index not found".
#[test]
fn failed_index_in_repo_without_commits_leaves_no_index_db() {
    let repo = unborn_repo();
    let r = repo.path().to_str().unwrap();
    let out = bin(&["index", "--repo", r]);
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("no commits"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !repo.path().join(".contextsmith/index.db").exists(),
        "empty index.db left behind"
    );
    let q = bin(&["query", "--repo", r, "--task", "anything"]);
    assert!(!q.status.success());
    assert!(
        String::from_utf8_lossy(&q.stderr).contains("Index not found"),
        "{}",
        String::from_utf8_lossy(&q.stderr)
    );
}

#[test]
fn failed_index_with_out_leaves_no_db_and_no_parent_dir() {
    let repo = unborn_repo();
    let cache = tempfile::tempdir().unwrap();
    let db = cache.path().join("nested/index.db");
    let out = bin(&[
        "index",
        "--repo",
        repo.path().to_str().unwrap(),
        "--out",
        db.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    assert!(!db.exists(), "empty index.db left behind");
    assert!(!db.parent().unwrap().exists(), "parent dir left behind");
}

/// An existing `index.db` at the target is neither replaced nor removed when the
/// repo has no commits (e.g. the user reset to an unborn branch).
#[test]
fn failed_index_keeps_an_existing_index_db() {
    let repo = unborn_repo();
    let dir = repo.path().join(".contextsmith");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.db"), b"existing bytes").unwrap();
    let out = bin(&["index", "--repo", repo.path().to_str().unwrap()]);
    assert!(!out.status.success());
    assert_eq!(
        std::fs::read(dir.join("index.db")).unwrap(),
        b"existing bytes"
    );
}
