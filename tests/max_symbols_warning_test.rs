//! #632: indexing a file with more than 10,000 symbols keeps the first 10,000 and warns on
//! stderr; a file with exactly 10,000 symbols is indexed in full without a warning.
//! Runs the real binary against a throwaway git repo; the index lives in a tempdir.

use std::path::Path;
use std::process::{Command, Output};

/// Mirrors `MAX_SYMBOLS_PER_FILE` in `src/index_builder.rs` (private there).
const LIMIT: usize = 10_000;

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

/// Commit one `big.rs` holding `n` one-line functions; run `contextsmith index` on it.
fn index_file_with_fns(n: usize) -> Output {
    let repo = tempfile::tempdir().unwrap();
    let d = repo.path();
    git(d, &["init", "-q"]);
    let src: String = (0..n).map(|i| format!("fn f{i}() {{}}\n")).collect();
    std::fs::write(d.join("big.rs"), src).unwrap();
    git(d, &["add", "big.rs"]);
    git(d, &["commit", "-q", "-m", "init"]);

    let work = tempfile::tempdir().unwrap();
    let db = work.path().join("idx.db");
    let out = Command::new(env!("CARGO_BIN_EXE_contextsmith"))
        .args(["index", "--repo", d.to_str().unwrap(), "--out"])
        .arg(&db)
        .env_remove("EMBEDDING_SVC_URL")
        .env_remove("EMBEDDING_API_KEY")
        .output()
        .expect("run contextsmith");
    assert!(
        out.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

#[test]
fn index_warns_when_file_exceeds_symbol_limit() {
    let out = index_file_with_fns(LIMIT + 1);
    let err = String::from_utf8_lossy(&out.stderr);
    let want = format!("has {} symbols; keeping the first {LIMIT}", LIMIT + 1);
    assert!(err.contains("warn:"), "{err}");
    assert!(err.contains("big.rs"), "{err}");
    assert!(err.contains(&want), "{err}");
}

#[test]
fn index_does_not_warn_when_file_has_exactly_the_symbol_limit() {
    let out = index_file_with_fns(LIMIT);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("keeping the first"), "{err}");
}
