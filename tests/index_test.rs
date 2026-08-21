use std::path::PathBuf;

use context_smith::index_builder::{build_index, IndexDb};
use context_smith::{GitRepo, Language, SymbolExtractor, SymbolKind};

/// Build a full index over a temp git repo and return (tempdir, IndexDb).
/// The tempdir must be kept alive for the DB paths to remain valid.
fn build_index_for(files: &[(&str, &str)]) -> (tempfile::TempDir, IndexDb) {
    let tmp = tempfile::tempdir().unwrap();
    let repo_dir = tmp.path().join("repo");
    std::fs::create_dir_all(&repo_dir).unwrap();
    make_git_repo(&repo_dir, files);

    let repo = GitRepo::new(&repo_dir).unwrap();
    let db = IndexDb::open(&tmp.path().join("index.db")).unwrap();
    build_index(&repo, &db).unwrap();
    (tmp, db)
}

/// Return (from_path, to_path) dependency edges resolved into the deps table.
fn dep_edges(db: &IndexDb) -> Vec<(String, String)> {
    let conn = db.connection();
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

#[test]
fn rust_symbols_extracted() {
    let src = r#"
use std::collections::HashMap;

pub struct MyStruct {
    value: i32,
}

impl MyStruct {
    pub fn new(v: i32) -> Self { MyStruct { value: v } }
}

pub fn add(a: i32, b: i32) -> i32 { a + b }
"#;
    let syms =
        SymbolExtractor::extract(std::path::Path::new("test.rs"), src, Language::Rust).unwrap();

    let names: Vec<&str> = syms.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(names.contains(&"MyStruct"), "expected struct MyStruct");
    assert!(names.contains(&"new"), "expected fn new");
    assert!(names.contains(&"add"), "expected fn add");

    let kinds: Vec<&SymbolKind> = syms.iter().map(|(_, k, _)| k).collect();
    assert!(kinds.contains(&&SymbolKind::Import), "expected use import");
    assert!(kinds.contains(&&SymbolKind::Struct));
    assert!(kinds.contains(&&SymbolKind::Function));
}

#[test]
fn go_symbols_extracted() {
    let src = r#"package main

import "fmt"

type Point struct {
    X, Y int
}

func NewPoint(x, y int) Point {
    return Point{X: x, Y: y}
}

func main() {
    fmt.Println(NewPoint(1, 2))
}
"#;
    let syms =
        SymbolExtractor::extract(std::path::Path::new("main.go"), src, Language::Go).unwrap();

    let names: Vec<&str> = syms.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(names.contains(&"NewPoint"), "expected func NewPoint");
    assert!(names.contains(&"main"), "expected func main");
    assert!(names.contains(&"Point"), "expected type Point");

    let kinds: Vec<&SymbolKind> = syms.iter().map(|(_, k, _)| k).collect();
    assert!(kinds.contains(&&SymbolKind::Import));
    assert!(kinds.contains(&&SymbolKind::Function));
}

#[test]
fn python_symbols_extracted() {
    let src = r#"import os
from pathlib import Path

class MyClass:
    def method(self):
        pass

def standalone():
    pass
"#;
    let syms =
        SymbolExtractor::extract(std::path::Path::new("test.py"), src, Language::Python).unwrap();

    let names: Vec<&str> = syms.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(names.contains(&"MyClass"), "expected class MyClass");
    assert!(names.contains(&"method"), "expected def method");
    assert!(names.contains(&"standalone"), "expected def standalone");
}

#[test]
fn typescript_symbols_extracted() {
    let src = r#"import { readFile } from "fs";

export interface User {
    id: number;
    name: string;
}

type Id = number;

export class Service {
    fetch(id: Id): User {
        return { id, name: "x" };
    }
}

export function standalone(): void {}

const arrow = (x: number): number => x + 1;
"#;
    let syms = SymbolExtractor::extract(std::path::Path::new("svc.ts"), src, Language::TypeScript)
        .unwrap();

    let names: Vec<&str> = syms.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(names.contains(&"User"), "expected interface User");
    assert!(names.contains(&"Id"), "expected type alias Id");
    assert!(names.contains(&"Service"), "expected class Service");
    assert!(names.contains(&"fetch"), "expected method fetch");
    assert!(names.contains(&"standalone"), "expected fn standalone");
    assert!(names.contains(&"arrow"), "expected arrow-fn const arrow");

    let kinds: Vec<&SymbolKind> = syms.iter().map(|(_, k, _)| k).collect();
    assert!(kinds.contains(&&SymbolKind::Import), "expected es import");
    assert!(kinds.contains(&&SymbolKind::Class));
    assert!(kinds.contains(&&SymbolKind::Struct), "interface -> struct");
}

#[test]
fn javascript_symbols_extracted() {
    let src = r#"const path = require("path");

export class Widget {
    render() {}
}

function main() {}

const handler = () => {};
"#;
    let syms = SymbolExtractor::extract(std::path::Path::new("app.js"), src, Language::JavaScript)
        .unwrap();

    let names: Vec<&str> = syms.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(names.contains(&"Widget"), "expected class Widget");
    assert!(names.contains(&"render"), "expected method render");
    assert!(names.contains(&"main"), "expected fn main");
    assert!(names.contains(&"handler"), "expected arrow-fn handler");

    let kinds: Vec<&SymbolKind> = syms.iter().map(|(_, k, _)| k).collect();
    assert!(
        kinds.contains(&&SymbolKind::Import),
        "expected require import"
    );
}

/// #27: ES import specifiers resolve to the imported file (with extension inference
/// and directory index files).
#[test]
fn ts_es_import_resolves_edge() {
    let (_tmp, db) = build_index_for(&[
        (
            "src/index.ts",
            "import { greet } from './util';\nimport { CONST } from './config/index';\ngreet();\n",
        ),
        ("src/util.ts", "export function greet() {}\n"),
        ("src/config/index.ts", "export const CONST = 1;\n"),
    ]);

    let edges = dep_edges(&db);
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
    let (_tmp, db) = build_index_for(&[
        ("src/app.js", "const lib = require('../lib/helper');\n"),
        ("lib/helper.js", "module.exports = {};\n"),
    ]);

    let edges = dep_edges(&db);
    assert!(
        edges.contains(&("src/app.js".into(), "lib/helper.js".into())),
        "expected app.js -> lib/helper.js, got {edges:?}"
    );
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
    build_index(&repo, &db).unwrap();

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

    // query must not create a bundle directory anywhere.
    assert!(
        !repo_dir.join("context.bundle").exists(),
        "query must not write a bundle to disk"
    );
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
    let stats = build_index(&repo, &db).unwrap();

    assert!(db_path.exists(), "index.db should be created");
    assert_eq!(stats.files_total, 3);
    assert_eq!(stats.files_indexed, 3);
    assert!(
        stats.symbols_total > 0,
        "should extract at least one symbol"
    );
}

// --- Regression tests for the batch fixes #11-#20 (issue #21) ---

/// #11: FTS body truncation must land on a UTF-8 char boundary.
/// A file larger than the 512 KiB cap whose byte-512Ki offset falls mid-character
/// used to panic on `&content[..512*1024]`. "あ" is 3 bytes, and 512*1024 is not a
/// multiple of 3, so the cut lands inside a character — exercising the guard.
#[test]
fn regression_11_multibyte_body_no_panic() {
    // 175_000 * 3 bytes ≈ 525 KiB > 512 KiB cap. Reaching this line at all proves
    // build_index() truncated the body without panicking on the mid-char boundary.
    let big = "あ".repeat(175_000);
    let (_tmp, db) = build_index_for(&[("notes.py", &big)]);

    // The oversized file is present in the index and the FTS body was populated.
    let file_count: i64 = db
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM files WHERE path = 'notes.py'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(file_count, 1, "oversized multibyte file should be indexed");
    let body_rows: i64 = db
        .connection()
        .query_row("SELECT COUNT(*) FROM fts_body", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        body_rows, 1,
        "fts_body should contain the truncated content"
    );
}

/// #12: FTS5 MATCH special characters must be sanitized, not passed through raw.
/// Queries containing `()`, `"`, `*`, `-` (and friends) previously caused a
/// "fts5: syntax error" instead of returning results.
#[test]
fn regression_12_fts_special_chars_no_error() {
    let (_tmp, db) = build_index_for(&[(
        "lib.rs",
        "pub fn parse(input: &str) -> bool { input.is_empty() }\n",
    )]);

    for q in [
        "parse()",
        "foo-bar",
        "\"quoted\"",
        "wild*card",
        "a AND (b OR c)",
        "col:val ^boost ~tilde !bang {brace}",
    ] {
        let res = context_smith::search_index::search_bm25(db.connection(), q, 10);
        assert!(res.is_ok(), "query {q:?} should not error: {:?}", res.err());
    }
}

/// #14: Python multi-dot relative imports resolve to the correct parent package.
/// `from ..foo import bar` in pkg/sub/consumer.py must resolve to pkg/foo.py,
/// not pkg/sub/foo.py.
#[test]
fn regression_14_python_multidot_relative_import() {
    let (_tmp, db) = build_index_for(&[
        ("pkg/__init__.py", "\n"),
        ("pkg/foo.py", "def bar():\n    pass\n"),
        ("pkg/sub/__init__.py", "\n"),
        ("pkg/sub/consumer.py", "from ..foo import bar\n"),
    ]);

    let edges = dep_edges(&db);
    assert!(
        edges.contains(&("pkg/sub/consumer.py".into(), "pkg/foo.py".into())),
        "expected consumer.py -> pkg/foo.py, got {edges:?}"
    );
}

/// #16: Go imports must not match a same-named package in a different directory.
/// Importing "mymod/a/util" links a/util/*.go but NOT b/util/*.go.
#[test]
fn regression_16_go_samename_dir_no_false_edge() {
    let (_tmp, db) = build_index_for(&[
        (
            "consumer/main.go",
            "package main\nimport \"mymod/a/util\"\nfunc main() {}\n",
        ),
        ("a/util/helper.go", "package util\nfunc A() {}\n"),
        ("b/util/helper.go", "package util\nfunc B() {}\n"),
    ]);

    let edges = dep_edges(&db);
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

/// #17: files whose content is very short (< 4 chars) must still count as ≥1 token
/// so they are not silently dropped from a bundle that has budget to spare.
#[test]
fn regression_17_short_file_selected() {
    use context_smith::budget::{allocate, Candidate};
    let tiny = Candidate {
        file_id: 1,
        path: PathBuf::from("tiny.rs"),
        score: 1.0,
        content: "ok".into(), // 2 chars -> tokens() == 0 without the max(1) guard
    };
    let selected = allocate(vec![tiny], 100);
    assert_eq!(
        selected.len(),
        1,
        "short-but-nonempty file must be selected"
    );
    assert_eq!(selected[0].path, PathBuf::from("tiny.rs"));
}

/// #20: identical file paths selected more than once must get distinct output slugs
/// so the second write does not clobber the first.
#[test]
fn regression_20_slug_collision_distinct_files() {
    use context_smith::budget::Candidate;
    use context_smith::bundle_writer::write_bundle;

    let dup = |content: &str| Candidate {
        file_id: 1,
        path: PathBuf::from("src/a.rs"),
        score: 1.0,
        content: content.into(),
    };
    let selected = vec![dup("first version"), dup("second version")];

    let out = tempfile::tempdir().unwrap();
    write_bundle("t", 1000, &selected, "", out.path(), false).unwrap();

    let code_dir = out.path().join("relevant-code");
    let names: Vec<String> = std::fs::read_dir(&code_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        names.len(),
        2,
        "two same-path candidates must yield two distinct files, got {names:?}"
    );
}

/// Rust extractor now covers enum, trait, and type alias in addition to struct/fn/impl.
#[test]
fn rust_enum_trait_type_extracted() {
    let src = r#"
pub enum Color { Red, Green, Blue }

pub trait Shape {
    fn area(&self) -> f64;
}

pub type Result<T> = std::result::Result<T, String>;
"#;
    let syms =
        SymbolExtractor::extract(std::path::Path::new("test.rs"), src, Language::Rust).unwrap();

    let names: Vec<&str> = syms.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(names.contains(&"Color"), "expected enum Color");
    assert!(names.contains(&"Shape"), "expected trait Shape");
    assert!(names.contains(&"Result"), "expected type alias Result");

    let kinds: Vec<&SymbolKind> = syms.iter().map(|(_, k, _)| k).collect();
    assert!(
        kinds.contains(&&SymbolKind::Struct),
        "enum/type should be Struct kind"
    );
    assert!(
        kinds.contains(&&SymbolKind::Class),
        "trait should be Class kind"
    );
}

/// Ruby: method, class, module, and require are extracted correctly.
#[test]
fn ruby_symbols_extracted() {
    let src = r#"
require 'json'
require_relative './helper'

module Util
  class Parser
    def initialize(input)
      @input = input
    end

    def parse
      JSON.parse(@input)
    end
  end
end
"#;
    let syms =
        SymbolExtractor::extract(std::path::Path::new("parser.rb"), src, Language::Ruby).unwrap();

    let names: Vec<&str> = syms.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(names.contains(&"Parser"), "expected class Parser");
    assert!(names.contains(&"Util"), "expected module Util");
    assert!(names.contains(&"initialize"), "expected method initialize");
    assert!(names.contains(&"parse"), "expected method parse");

    let kinds: Vec<&SymbolKind> = syms.iter().map(|(_, k, _)| k).collect();
    assert!(
        kinds.contains(&&SymbolKind::Import),
        "expected require as Import"
    );
    assert!(kinds.contains(&&SymbolKind::Class));
    assert!(
        kinds.contains(&&SymbolKind::Struct),
        "module should be Struct kind"
    );
    assert!(kinds.contains(&&SymbolKind::Function));
}

/// Path-based BM25 boost: a file whose name matches the task query should rank
/// higher than a file whose path does not match, even if both contain the keyword.
#[test]
fn bm25_path_weight_boosts_matching_filename() {
    use context_smith::search_index::search_bm25;

    let (_tmp, db) = build_index_for(&[
        // auth.rs — path matches "auth"; also contains the word
        (
            "src/auth.rs",
            "pub fn authenticate(token: &str) -> bool { true }\n",
        ),
        // util.rs — path does NOT match "auth"; but content contains "authenticate"
        ("src/util.rs", "pub fn helper_authenticate_call() {}\n"),
    ]);

    let results = search_bm25(db.connection(), "auth", 10).unwrap();
    assert!(
        !results.is_empty(),
        "expected at least one BM25 result for 'auth'"
    );

    // Find scores for each file
    let conn = db.connection();
    let auth_id: i64 = conn
        .query_row("SELECT id FROM files WHERE path = 'src/auth.rs'", [], |r| {
            r.get(0)
        })
        .unwrap();
    let util_id: i64 = conn
        .query_row("SELECT id FROM files WHERE path = 'src/util.rs'", [], |r| {
            r.get(0)
        })
        .unwrap();

    let score_of = |id: i64| {
        results
            .iter()
            .find(|(fid, _)| *fid == id)
            .map(|(_, s)| *s)
            .unwrap_or(0.0)
    };

    let auth_score = score_of(auth_id);
    let util_score = score_of(util_id);
    assert!(
        auth_score > util_score,
        "auth.rs (path match) should outscore util.rs: {auth_score} vs {util_score}"
    );
}
