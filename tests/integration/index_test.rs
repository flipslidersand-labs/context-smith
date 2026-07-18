use std::path::PathBuf;

use context_smith::{GitRepo, Language, SymbolExtractor, SymbolKind};

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
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

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
    let syms = SymbolExtractor::extract(
        std::path::Path::new("test.rs"),
        src,
        Language::Rust,
    )
    .unwrap();

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
    let syms = SymbolExtractor::extract(
        std::path::Path::new("main.go"),
        src,
        Language::Go,
    )
    .unwrap();

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
    let syms = SymbolExtractor::extract(
        std::path::Path::new("test.py"),
        src,
        Language::Python,
    )
    .unwrap();

    let names: Vec<&str> = syms.iter().map(|(n, _, _)| n.as_str()).collect();
    assert!(names.contains(&"MyClass"), "expected class MyClass");
    assert!(names.contains(&"method"), "expected def method");
    assert!(names.contains(&"standalone"), "expected def standalone");
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
            (
                "main.go",
                "package main\nfunc main() {}\n",
            ),
            (
                "script.py",
                "def run(): pass\n",
            ),
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
    assert!(stats.symbols_total > 0, "should extract at least one symbol");
}
