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

fn init_repo(d: &Path) {
    git(d, &["init", "-q"]);
    git(d, &["config", "user.email", "t@example.com"]);
    git(d, &["config", "user.name", "t"]);
    std::fs::write(d.join("lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
    git(d, &["add", "lib.rs"]);
    git(d, &["commit", "-q", "-m", "init"]);
}

fn run_index(d: &Path, extra: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_contextsmith"))
        .args(["index", "--repo", d.to_str().unwrap()])
        .args(extra)
        .output()
        .expect("run contextsmith");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn index_refuses_symlinked_index_files_in_default_dir() {
    for name in ["index.db", "index.db-wal", "index.db-shm"] {
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let d = repo.path();
        init_repo(d);
        std::fs::create_dir(d.join(".contextsmith")).unwrap();
        let target = outside.path().join("victim");
        std::os::unix::fs::symlink(&target, d.join(".contextsmith").join(name)).unwrap();
        let (ok, stderr) = run_index(d, &[]);
        assert!(!ok, "{name} must fail; stderr: {stderr}");
        assert!(
            stderr.contains("is a symlink; refusing"),
            "{name}: {stderr}"
        );
        assert!(
            !target.exists(),
            "{name}: nothing may be written via the link"
        );
    }
}

#[test]
fn index_refuses_symlinked_out_path() {
    for suffix in ["", "-wal", "-shm"] {
        let repo = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let d = repo.path();
        init_repo(d);
        let out_dir = tempfile::tempdir().unwrap();
        let out = out_dir.path().join("my.db");
        let link = out_dir.path().join(format!("my.db{suffix}"));
        let target = outside.path().join("victim");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let (ok, stderr) = run_index(d, &["--out", out.to_str().unwrap()]);
        assert!(!ok, "{suffix:?} must fail; stderr: {stderr}");
        assert!(
            stderr.contains("is a symlink; refusing"),
            "{suffix:?}: {stderr}"
        );
        assert!(
            !target.exists(),
            "{suffix:?}: nothing may be written via the link"
        );
    }
}
