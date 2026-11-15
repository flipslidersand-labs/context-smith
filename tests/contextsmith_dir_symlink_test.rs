//! A committed `.contextsmith` symlink must not redirect index.db (and its -wal/-shm)
//! outside the repository. Mirrors the symlink refusal in bundle_writer.

#![cfg(unix)]

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run git");
    assert!(st.success(), "git {args:?} failed");
}

#[test]
fn index_refuses_symlinked_default_contextsmith_dir() {
    let repo = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let d = repo.path();
    git(d, &["init", "-q"]);
    git(d, &["config", "user.email", "t@example.com"]);
    git(d, &["config", "user.name", "t"]);
    std::fs::write(d.join("lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), d.join(".contextsmith")).unwrap();
    git(d, &["add", "lib.rs", ".contextsmith"]);
    git(d, &["commit", "-q", "-m", "init"]);

    let out = Command::new(env!("CARGO_BIN_EXE_contextsmith"))
        .args(["index", "--repo", d.to_str().unwrap()])
        .output()
        .expect("run contextsmith");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "must fail; stderr: {stderr}");
    assert!(stderr.contains("is a symlink; refusing"), "{stderr}");
    assert_eq!(
        std::fs::read_dir(outside.path()).unwrap().count(),
        0,
        "nothing may be created outside the repo"
    );
}
