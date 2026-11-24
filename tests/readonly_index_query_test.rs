//! `query` must work on an index that cannot be written: the file itself read-only, or
//! also its directory (an index shipped read-only). `query` opens the index with
//! `open_for_read`, which must not need write access (#312).

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
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

fn chmod(path: &Path, mode: u32) {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// True when chmod restrictions are not enforced (e.g. uid 0): a file can be created
/// inside a 0555 directory.
fn permissions_not_enforced() -> bool {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("probe");
    std::fs::create_dir(&dir).unwrap();
    chmod(&dir, 0o555);
    let writable = std::fs::write(dir.join("x"), b"x").is_ok();
    chmod(&dir, 0o755);
    writable
}

fn committed_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q"]);
    git(d, &["config", "user.email", "t@example.com"]);
    git(d, &["config", "user.name", "t"]);
    std::fs::write(d.join("lib.rs"), "pub fn readonly_marker() {}\n").unwrap();
    git(d, &["add", "lib.rs"]);
    git(d, &["commit", "-q", "-m", "init"]);
    tmp
}

fn query_finds_marker(repo: &Path, db: &Path) {
    let out = bin(&[
        "query",
        "--repo",
        repo.to_str().unwrap(),
        "--index",
        db.to_str().unwrap(),
        "--task",
        "readonly_marker",
    ]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "query failed: {stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("lib.rs"), "lib.rs not selected: {stdout}");
}

fn build_index(repo: &Path, db: &Path) {
    let out = bin(&[
        "index",
        "--repo",
        repo.to_str().unwrap(),
        "--out",
        db.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn query_works_on_read_only_index_file() {
    if permissions_not_enforced() {
        eprintln!("skipping: file permissions are not enforced (running as root?)");
        return;
    }
    let repo = committed_repo();
    let cache = tempfile::tempdir().unwrap();
    let db = cache.path().join("index.db");
    build_index(repo.path(), &db);
    chmod(&db, 0o444);
    query_finds_marker(repo.path(), &db);
}

#[test]
fn query_works_on_read_only_index_dir_and_file() {
    if permissions_not_enforced() {
        eprintln!("skipping: file permissions are not enforced (running as root?)");
        return;
    }
    let repo = committed_repo();
    let cache = tempfile::tempdir().unwrap();
    let dir = cache.path().join("ro");
    let db = dir.join("index.db");
    build_index(repo.path(), &db);
    chmod(&db, 0o444);
    chmod(&dir, 0o555);
    let before: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    query_finds_marker(repo.path(), &db);
    let after: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    chmod(&dir, 0o755); // let TempDir clean up
    assert_eq!(
        before.len(),
        after.len(),
        "query left files in the index dir"
    );
}
