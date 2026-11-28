//! #648: terminal control characters must not reach stdout/stderr raw, either from names inside an
//! untrusted `index.db` (error text) or from `query --format json` (C1 and U+2028/2029, which
//! serde_json leaves as-is). Runs the real binary against throwaway repos and DBs in tempdirs.

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
        .env_remove("GIT_DIR")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("EMBEDDING_SVC_URL")
        .env_remove("EMBEDDING_API_KEY")
        .output()
        .expect("run contextsmith")
}

fn repo_with_file(name: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    git(d, &["init", "-q"]);
    std::fs::write(d.join(name), "pub fn alpha_widget() -> u32 { 1 }\n").unwrap();
    git(d, &["add", name]);
    git(d, &["commit", "-q", "-m", "init"]);
    tmp
}

/// A DB holding a trigger whose name contains ESC (an OSC title-set sequence).
fn db_with_hostile_trigger(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    let name = "evil\u{1b}]0;pwned\u{7}\u{9b}x";
    conn.execute_batch(&format!(
        "CREATE TABLE t(x);
         CREATE TRIGGER \"{name}\" AFTER INSERT ON t BEGIN SELECT 1; END;"
    ))
    .unwrap();
}

fn assert_no_raw_controls(s: &str) {
    for c in s.chars() {
        let raw = matches!(c, '\u{0}'..='\u{8}' | '\u{b}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
            || c == '\u{2028}'
            || c == '\u{2029}';
        assert!(!raw, "raw control {:?} in {s:?}", c);
    }
}

#[test]
fn schema_object_name_in_error_is_escaped_on_every_command() {
    let repo = repo_with_file("a.rs");
    let work = tempfile::tempdir().unwrap();
    let db = work.path().join("hostile.db");
    db_with_hostile_trigger(&db);
    let repo_s = repo.path().to_str().unwrap();
    let db_s = db.to_str().unwrap();

    let bundle = work.path().join("o.md");
    let runs: [&[&str]; 3] = [
        &[
            "query",
            "--repo",
            repo_s,
            "--task",
            "alpha_widget",
            "--index",
            db_s,
            "--no-embed",
        ],
        &[
            "build",
            "--repo",
            repo_s,
            "--task",
            "alpha_widget",
            "--index",
            db_s,
            "--out",
            bundle.to_str().unwrap(),
            "--no-embed",
        ],
        &["index", "--repo", repo_s, "--out", db_s],
    ];
    for args in runs {
        let out = bin(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.starts_with("Error: "), "{args:?}: {err:?}");
        assert!(
            err.contains("unexpected schema object"),
            "{args:?}: {err:?}"
        );
        assert!(
            err.contains("evil\\x1b]0;pwned\\x07\\x9bx"),
            "{args:?}: {err:?}"
        );
        assert_no_raw_controls(&err.replace('\n', ""));
    }
}

#[test]
fn plain_errors_keep_error_prefix_and_exit_code_one() {
    let repo = repo_with_file("a.rs");
    let missing = repo.path().join("nope.db");
    let out = bin(&[
        "query",
        "--repo",
        repo.path().to_str().unwrap(),
        "--task",
        "x",
        "--index",
        missing.to_str().unwrap(),
    ]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.starts_with("Error: "), "{err:?}");
    assert!(err.contains("Index not found"), "{err:?}");
}

#[test]
fn json_output_escapes_c1_and_line_separators_without_changing_the_value() {
    for (name, escaped) in [
        ("a\u{9b}b.rs", "a\\u009bb.rs"),
        ("a\u{85}b.rs", "a\\u0085b.rs"),
        ("a\u{2028}b.rs", "a\\u2028b.rs"),
        ("a\u{2029}b.rs", "a\\u2029b.rs"),
        ("a\u{1b}b.rs", "a\\u001bb.rs"),
    ] {
        let repo = repo_with_file(name);
        let work = tempfile::tempdir().unwrap();
        let db = work.path().join("idx.db");
        let idx = bin(&[
            "index",
            "--repo",
            repo.path().to_str().unwrap(),
            "--out",
            db.to_str().unwrap(),
        ]);
        assert!(idx.status.success(), "index {name:?}: {:?}", idx.stderr);
        let out = bin(&[
            "query",
            "--repo",
            repo.path().to_str().unwrap(),
            "--task",
            "alpha_widget",
            "--index",
            db.to_str().unwrap(),
            "--format",
            "json",
            "--no-embed",
        ]);
        assert!(out.status.success(), "query {name:?}: {:?}", out.stderr);
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert_no_raw_controls(&stdout.replace('\n', ""));
        assert!(stdout.contains(escaped), "{name:?}: {stdout}");
        // The value a machine reader gets back is unchanged.
        let doc: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(doc["files"][0]["path"].as_str(), Some(name), "{stdout}");
    }
}

/// #652: a `warn:` line that embeds an error's Display text (here libgit2's, which carries the
/// ref name) must be escaped too. C1 (U+009B) is a legal ref-name character; git rejects ESC.
#[test]
fn recent_diff_warning_escapes_controls_from_git_errors() {
    let repo = repo_with_file("a.rs");
    let d = repo.path();
    git(d, &["branch", "-m", "main\u{9b}x"]);
    let work = tempfile::tempdir().unwrap();
    let db = work.path().join("idx.db");
    let idx = bin(&[
        "index",
        "--repo",
        d.to_str().unwrap(),
        "--out",
        db.to_str().unwrap(),
    ]);
    assert!(idx.status.success(), "index: {:?}", idx.stderr);
    // Corrupt the ref HEAD points at, so recent_diff fails with a libgit2 error naming it.
    std::fs::write(d.join(".git/refs/heads/main\u{9b}x"), "garbage\n").unwrap();
    let bundle = work.path().join("o.md");
    let out = bin(&[
        "build",
        "--repo",
        d.to_str().unwrap(),
        "--task",
        "alpha_widget",
        "--index",
        db.to_str().unwrap(),
        "--out",
        bundle.to_str().unwrap(),
        "--no-embed",
    ]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("recent_diff failed"), "{err:?}");
    assert_no_raw_controls(&err.replace('\n', ""));
    assert!(err.contains("\\x9b"), "{err:?}");
}
