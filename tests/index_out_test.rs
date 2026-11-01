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
