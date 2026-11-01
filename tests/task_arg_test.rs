//! #149: a `--task` value that starts with `-` (e.g. an issue title like
//! "--files performance regression in 0.8.0") must be taken as the task text, not rejected
//! by clap as an unknown option.

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

/// A tiny committed repo, already indexed, whose source mentions the words used by the tasks.
fn indexed_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q"]);
    git(d, &["config", "user.email", "t@example.com"]);
    git(d, &["config", "user.name", "t"]);
    std::fs::create_dir_all(d.join("src")).unwrap();
    std::fs::write(
        d.join("src/lib.rs"),
        "// files performance regression budget\npub fn files_performance() -> u32 { 1 }\n",
    )
    .unwrap();
    git(d, &["add", "src/lib.rs"]);
    git(d, &["commit", "-q", "-m", "init"]);
    let out = bin(&["index", "--repo", d.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    tmp
}

fn bin(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_contextsmith"))
        .args(args)
        .output()
        .expect("run contextsmith")
}

fn assert_ok(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "exit {:?}: {stderr}",
        out.status.code()
    );
    assert!(
        !stderr.contains("unexpected argument"),
        "clap rejected the task: {stderr}"
    );
    String::from_utf8(out.stdout.clone()).unwrap()
}

#[test]
fn query_accepts_a_task_starting_with_double_hyphen() {
    let repo = indexed_repo();
    let out = bin(&[
        "query",
        "--repo",
        repo.path().to_str().unwrap(),
        "--task",
        "--files performance regression in 0.8.0",
    ]);
    let stdout = assert_ok(&out);
    assert!(
        stdout.contains("--files performance regression in 0.8.0"),
        "{stdout}"
    );
}

#[test]
fn query_accepts_a_task_starting_with_single_hyphen() {
    let repo = indexed_repo();
    let out = bin(&[
        "query",
        "--repo",
        repo.path().to_str().unwrap(),
        "--task",
        "-x files performance",
    ]);
    assert_ok(&out);
}

#[test]
fn equals_form_still_works() {
    let repo = indexed_repo();
    let out = bin(&[
        "query",
        "--repo",
        repo.path().to_str().unwrap(),
        "--task=--files performance",
    ]);
    assert_ok(&out);
}

#[test]
fn a_hyphen_task_does_not_swallow_other_options() {
    let repo = indexed_repo();
    let out = bin(&[
        "query",
        "--repo",
        repo.path().to_str().unwrap(),
        "--task",
        "--budget 5 files performance",
        "--budget",
        "1234",
    ]);
    let stdout = assert_ok(&out);
    assert!(
        stdout.contains("\"budget\": 1234"),
        "budget option was not applied: {stdout}"
    );
    assert!(
        stdout.contains("--budget 5 files performance"),
        "task text was altered: {stdout}"
    );
}

#[test]
fn build_accepts_a_task_starting_with_hyphen() {
    let repo = indexed_repo();
    let out_dir = repo.path().join("bundle");
    let out = bin(&[
        "build",
        "--repo",
        repo.path().to_str().unwrap(),
        "--task",
        "--files performance regression",
        "--out",
        out_dir.to_str().unwrap(),
    ]);
    assert_ok(&out);
    assert!(out_dir.join("task.md").exists(), "bundle was not written");
}
