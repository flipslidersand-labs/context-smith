use std::path::PathBuf;

use context_smith::index_builder::{build_index, IndexDb};
use context_smith::GitRepo;

/// Build a full index over a temp git repo and return (tempdir, IndexDb, db path).
/// The tempdir must be kept alive for the DB paths to remain valid.
fn build_index_for(files: &[(&str, &str)]) -> (tempfile::TempDir, IndexDb, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    make_git_repo(&repo_dir, files);

    let repo = GitRepo::new(&repo_dir).unwrap();
    let db_path = tmp.path().join("index.db");
    let db = IndexDb::open(&db_path).unwrap();
    build_index(&repo, &db, false).unwrap();
    (tmp, db, db_path)
}

/// Return (from_path, to_path) dependency edges resolved into the deps table.
fn dep_edges(db_path: &std::path::Path) -> Vec<(String, String)> {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT f.path, t.path FROM deps d
             JOIN files f ON f.id = d.from_file
             JOIN files t ON t.id = d.to_file",
        )
        .unwrap();
    stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

fn make_git_repo(dir: &std::path::Path, files: &[(&str, &str)]) -> PathBuf {
    let repo = git2::Repository::init(dir).unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();

    for (name, content) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
    }

    let mut index = repo.index().unwrap();
    for (name, _) in files {
        index.add_path(std::path::Path::new(name)).unwrap();
    }
    index.write().unwrap();

    let oid = index.write_tree().unwrap();
    let tree = repo.find_tree(oid).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();

    dir.to_owned()
}

/// #27: ES import specifiers resolve to the imported file (with extension inference
/// and directory index files).
#[test]
fn ts_es_import_resolves_edge() {
    let (_tmp, _db, db_path) = build_index_for(&[
        (
            "src/index.ts",
            "import { greet } from './util';\nimport { CONST } from './config/index';\ngreet();\n",
        ),
        ("src/util.ts", "export function greet() {}\n"),
        ("src/config/index.ts", "export const CONST = 1;\n"),
    ]);

    let edges = dep_edges(&db_path);
    assert!(
        edges.contains(&("src/index.ts".into(), "src/util.ts".into())),
        "expected index.ts -> util.ts, got {edges:?}"
    );
    assert!(
        edges.contains(&("src/index.ts".into(), "src/config/index.ts".into())),
        "expected index.ts -> config/index.ts, got {edges:?}"
    );
}

/// #27: CommonJS require() with a relative parent path resolves correctly.
#[test]
fn js_require_parent_resolves_edge() {
    let (_tmp, _db, db_path) = build_index_for(&[
        ("src/app.js", "const lib = require('../lib/helper');\n"),
        ("lib/helper.js", "module.exports = {};\n"),
    ]);

    let edges = dep_edges(&db_path);
    assert!(
        edges.contains(&("src/app.js".into(), "lib/helper.js".into())),
        "expected app.js -> lib/helper.js, got {edges:?}"
    );
}

/// #190: `export ... from` re-exports in barrel files resolve to edges; bare packages do not.
#[test]
fn ecma_export_from_barrel_resolves_edges() {
    let barrel = "export * from './a';\nexport { b } from './b';\nexport * as ns from './c';\nexport * from '../up/x';\nexport * from 'pkg';\n";
    for ext in ["ts", "js"] {
        let barrel_path = format!("src/index.{ext}");
        let (_tmp, _db, db_path) = build_index_for(&[
            (barrel_path.as_str(), barrel),
            (format!("src/a.{ext}").as_str(), "export const a = 1;\n"),
            (format!("src/b.{ext}").as_str(), "export const b = 1;\n"),
            (format!("src/c.{ext}").as_str(), "export const c = 1;\n"),
            (format!("up/x.{ext}").as_str(), "export const x = 1;\n"),
        ]);
        let edges = dep_edges(&db_path);
        for to in ["src/a", "src/b", "src/c", "up/x"] {
            let to = format!("{to}.{ext}");
            assert!(
                edges.contains(&(barrel_path.clone(), to.clone())),
                "expected {barrel_path} -> {to}, got {edges:?}"
            );
        }
        assert_eq!(edges.len(), 4, "pkg must add no edge: {edges:?}");
    }
}

/// #28: `query` prints machine-readable JSON to stdout and writes nothing to disk.
#[test]
fn query_json_stdout_no_files_written() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    make_git_repo(
        &repo_dir,
        &[(
            "src/budget.rs",
            "pub fn allocate_budget(tokens: usize) -> usize { tokens }\n",
        )],
    );

    let db_path = tmp.path().join("index.db");
    let repo = GitRepo::new(&repo_dir).unwrap();
    let db = IndexDb::open(&db_path).unwrap();
    build_index(&repo, &db, false).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_contextsmith"))
        .args([
            "query",
            "--repo",
            repo_dir.to_str().unwrap(),
            "--task",
            "budget tokens",
            "--budget",
            "5000",
            "--index",
            db_path.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .expect("run contextsmith query");

    assert!(
        out.status.success(),
        "query exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&stdout).expect("stdout must be valid JSON");
    assert_eq!(doc["budget"], 5000);
    let files = doc["files"].as_array().expect("files array");
    assert!(!files.is_empty(), "expected at least one selected file");
    assert_eq!(files[0]["path"], "src/budget.rs");
    assert!(files[0]["tokens"].as_u64().unwrap() > 0);
    assert_eq!(files[0]["truncated"], false);

    // query must not create a bundle directory anywhere.
    assert!(
        !repo_dir.join("context.bundle").exists(),
        "query must not write a bundle to disk"
    );
}

/// #201: a file cut to fit the budget is reported with `"truncated": true` in query JSON.
#[test]
fn query_json_marks_truncated_file() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    let big = format!(
        "pub fn allocate_budget(tokens: usize) -> usize {{ tokens }}\n{}",
        "// budget tokens padding line\n".repeat(200)
    );
    make_git_repo(&repo_dir, &[("src/budget.rs", big.as_str())]);

    let db_path = tmp.path().join("index.db");
    let repo = GitRepo::new(&repo_dir).unwrap();
    let db = IndexDb::open(&db_path).unwrap();
    build_index(&repo, &db, false).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_contextsmith"))
        .args([
            "query",
            "--repo",
            repo_dir.to_str().unwrap(),
            "--task",
            "budget tokens",
            "--budget",
            "100",
            "--index",
            db_path.to_str().unwrap(),
            "--format",
            "json",
        ])
        .output()
        .expect("run contextsmith query");
    assert!(out.status.success());
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["files"][0]["truncated"], true);
}

#[test]
fn index_command_creates_db() {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();

    make_git_repo(
        &repo_dir,
        &[
            (
                "src/lib.rs",
                "pub fn hello() -> &'static str { \"hello\" }\n",
            ),
            ("main.go", "package main\nfunc main() {}\n"),
            ("script.py", "def run(): pass\n"),
        ],
    );

    let repo = GitRepo::new(&repo_dir).unwrap();
    let db_path = tmp.path().join("index.db");

    use context_smith::index_builder::{build_index, IndexDb};
    let db = IndexDb::open(&db_path).unwrap();
    let stats = build_index(&repo, &db, false).unwrap();

    assert!(db_path.exists(), "index.db should be created");
    assert_eq!(stats.files_total, 3);
    assert_eq!(stats.files_indexed, 3);
    assert!(
        stats.symbols_total > 0,
        "should extract at least one symbol"
    );
}

/// #14: Python multi-dot relative imports resolve to the correct parent package.
/// `from ..foo import bar` in pkg/sub/consumer.py must resolve to pkg/foo.py,
/// not pkg/sub/foo.py.
#[test]
fn regression_14_python_multidot_relative_import() {
    let (_tmp, _db, db_path) = build_index_for(&[
        ("pkg/__init__.py", "\n"),
        ("pkg/foo.py", "def bar():\n    pass\n"),
        ("pkg/sub/__init__.py", "\n"),
        ("pkg/sub/consumer.py", "from ..foo import bar\n"),
    ]);

    let edges = dep_edges(&db_path);
    assert!(
        edges.contains(&("pkg/sub/consumer.py".into(), "pkg/foo.py".into())),
        "expected consumer.py -> pkg/foo.py, got {edges:?}"
    );
}

/// #16: Go imports must not match a same-named package in a different directory.
/// Importing "mymod/a/util" links a/util/*.go but NOT b/util/*.go.
#[test]
fn regression_16_go_samename_dir_no_false_edge() {
    let (_tmp, _db, db_path) = build_index_for(&[
        (
            "consumer/main.go",
            "package main\nimport \"mymod/a/util\"\nfunc main() {}\n",
        ),
        ("a/util/helper.go", "package util\nfunc A() {}\n"),
        ("b/util/helper.go", "package util\nfunc B() {}\n"),
    ]);

    let edges = dep_edges(&db_path);
    let has_to = |p: &str| edges.iter().any(|(_, to)| to == p);
    assert!(
        has_to("a/util/helper.go"),
        "expected edge to a/util/helper.go, got {edges:?}"
    );
    assert!(
        !has_to("b/util/helper.go"),
        "must NOT link same-named b/util/helper.go, got {edges:?}"
    );
}
