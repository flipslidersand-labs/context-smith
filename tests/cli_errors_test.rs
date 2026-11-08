//! CLI-level tests for argument validation, error paths and the `build` / `query --format md`
//! outputs. Runs the real binary against a throwaway git repo; the index lives in a tempdir.

use std::path::Path;
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("run git");
    assert!(st.success(), "git {args:?} failed");
}

fn bin(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_contextsmith"))
        .args(args)
        .env_remove("EMBEDDING_SVC_URL")
        .env_remove("EMBEDDING_API_KEY")
        .output()
        .expect("run contextsmith")
}

fn committed_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q"]);
    std::fs::write(
        d.join("a.rs"),
        "// widget\npub fn alpha_widget() -> u32 { 1 }\n",
    )
    .unwrap();
    git(d, &["add", "a.rs"]);
    git(d, &["commit", "-q", "-m", "init"]);
    tmp
}

/// Repo plus an index kept outside it (in its own tempdir).
fn indexed() -> (tempfile::TempDir, tempfile::TempDir) {
    let repo = committed_repo();
    let work = tempfile::tempdir().unwrap();
    let db = work.path().join("idx.db");
    let out = bin(&[
        "index",
        "--repo",
        repo.path().to_str().unwrap(),
        "--out",
        db.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "index failed");
    (repo, work)
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn budget_zero_is_rejected() {
    let repo = committed_repo();
    let out = bin(&[
        "query",
        "--repo",
        repo.path().to_str().unwrap(),
        "--task",
        "x",
        "--budget",
        "0",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("--budget must be"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn budget_non_numeric_or_overflow_is_rejected() {
    let repo = committed_repo();
    for v in ["abc", "99999999999999999999999"] {
        let out = bin(&[
            "query",
            "--repo",
            repo.path().to_str().unwrap(),
            "--task",
            "x",
            "--budget",
            v,
        ]);
        assert_eq!(out.status.code(), Some(2), "budget {v}");
        assert!(
            stderr(&out).contains("is not a valid integer"),
            "budget {v}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn blank_task_is_rejected() {
    let repo = committed_repo();
    let out = bin(&[
        "query",
        "--repo",
        repo.path().to_str().unwrap(),
        "--task",
        "   ",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("must not be empty"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn build_without_index_fails_and_writes_nothing() {
    let repo = committed_repo();
    let work = tempfile::tempdir().unwrap();
    let bundle = work.path().join("bundle");
    let out = bin(&[
        "build",
        "--repo",
        repo.path().to_str().unwrap(),
        "--task",
        "alpha_widget",
        "--out",
        bundle.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("Index not found"), "{}", stderr(&out));
    assert!(!bundle.exists());
}

#[test]
fn query_with_missing_explicit_index_path_fails() {
    let repo = committed_repo();
    let work = tempfile::tempdir().unwrap();
    let missing = work.path().join("nope.db");
    let out = bin(&[
        "query",
        "--repo",
        repo.path().to_str().unwrap(),
        "--task",
        "alpha_widget",
        "--index",
        missing.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stderr(&out).contains("Index not found"), "{}", stderr(&out));
}

#[test]
fn unmatched_task_reports_no_matching_files() {
    let (repo, work) = indexed();
    let db = work.path().join("idx.db");
    for sub in [["query"], ["build"]] {
        let bundle = work.path().join("b");
        let mut args = vec![
            sub[0],
            "--repo",
            repo.path().to_str().unwrap(),
            "--task",
            "zzzqqq",
            "--index",
            db.to_str().unwrap(),
        ];
        if sub[0] == "build" {
            args.extend(["--out", bundle.to_str().unwrap()]);
        }
        let out = bin(&args);
        assert_eq!(out.status.code(), Some(1), "{}", sub[0]);
        assert!(
            stderr(&out).contains("No matching files found"),
            "{}",
            stderr(&out)
        );
        assert!(!bundle.exists(), "no bundle for an empty selection");
    }
}

#[test]
fn build_writes_bundle_with_explain_and_diff() {
    let (repo, work) = indexed();
    let db = work.path().join("idx.db");
    let bundle = work.path().join("out/bundle");
    let out = bin(&[
        "build",
        "--repo",
        repo.path().to_str().unwrap(),
        "--task",
        "alpha_widget",
        "--index",
        db.to_str().unwrap(),
        "--out",
        bundle.to_str().unwrap(),
        "--explain",
        "--diff-commits",
        "1",
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).starts_with("bundle: 1 files"),
        "{}",
        stdout(&out)
    );
    let task_md = std::fs::read_to_string(bundle.join("task.md")).unwrap();
    assert!(task_md.contains("`a.rs`"), "{task_md}");
    assert!(
        task_md.contains("score="),
        "--explain adds score: {task_md}"
    );
    assert!(task_md.contains("## Recent Diff"), "{task_md}");
    assert!(task_md.contains("+pub fn alpha_widget"), "{task_md}");
    assert!(bundle.join("citations.json").is_file());
    assert!(bundle.join("relevant-code").is_dir());
    let code = std::fs::read_dir(bundle.join("relevant-code"))
        .unwrap()
        .count();
    assert_eq!(code, 1);
}

#[test]
fn build_without_explain_has_no_scores() {
    let (repo, work) = indexed();
    let db = work.path().join("idx.db");
    let bundle = work.path().join("bundle");
    let out = bin(&[
        "build",
        "--repo",
        repo.path().to_str().unwrap(),
        "--task",
        "alpha_widget",
        "--index",
        db.to_str().unwrap(),
        "--out",
        bundle.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let task_md = std::fs::read_to_string(bundle.join("task.md")).unwrap();
    assert!(!task_md.contains("score="), "{task_md}");
}

#[test]
fn query_format_md_prints_summary_without_diff() {
    let (repo, work) = indexed();
    let db = work.path().join("idx.db");
    let base = [
        "query",
        "--repo",
        repo.path().to_str().unwrap(),
        "--task",
        "alpha_widget",
        "--index",
        db.to_str().unwrap(),
        "--format",
        "md",
    ];
    let out = bin(&base);
    assert!(out.status.success(), "{}", stderr(&out));
    let md = stdout(&out);
    assert!(md.starts_with("# Context Bundle"), "{md}");
    assert!(md.contains("`a.rs`"), "{md}");
    assert!(!md.contains("score="), "{md}");
    assert!(!md.contains("## Recent Diff"), "{md}");

    let mut explained = base.to_vec();
    explained.push("--explain");
    let out = bin(&explained);
    assert!(out.status.success());
    assert!(stdout(&out).contains("score="), "{}", stdout(&out));
}
