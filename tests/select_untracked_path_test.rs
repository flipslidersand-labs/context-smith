//! #395: a crafted index.db whose `files.path` points at an untracked regular
//! file (e.g. `.env`) must not make selection read and emit that file.

use std::path::Path;

use context_smith::{build_index, select_candidates, GitRepo, IndexDb, SelectOptions};

fn commit_files(dir: &Path, files: &[(&str, &str)]) {
    let repo = git2::Repository::init(dir).unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    for (name, content) in files {
        std::fs::write(dir.join(name), content).unwrap();
    }
    let mut index = repo.index().unwrap();
    for (name, _) in files {
        index.add_path(Path::new(name)).unwrap();
    }
    index.write().unwrap();
    let tree = repo.find_tree(index.write_tree().unwrap()).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();
}

fn setup() -> (tempfile::TempDir, GitRepo, IndexDb, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    commit_files(
        &dir,
        &[("lib.rs", "pub fn allocate_budget() -> u32 { 1 }\n")],
    );
    std::fs::write(dir.join(".env"), "SECRET_TOKEN=hunter2\n").unwrap(); // untracked
    let repo = GitRepo::new(&dir).unwrap();
    let db_path = tmp.path().join("index.db");
    let db = IndexDb::open(&db_path).unwrap();
    build_index(&repo, &db, false).unwrap();
    (tmp, repo, db, db_path)
}

#[test]
fn select_does_not_read_untracked_path_from_db() {
    let (_tmp, repo, db, db_path) = setup();
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute("UPDATE files SET path = '.env'", []).unwrap();
    }
    match select_candidates(&repo, &db, "allocate_budget", &SelectOptions::new(10_000)) {
        Ok(c) => {
            assert!(
                c.iter().all(|c| !c.content.contains("hunter2")),
                "untracked .env content leaked"
            );
            assert!(c.is_empty(), "untracked path must be excluded");
        }
        Err(e) => assert!(!e.to_string().contains("hunter2"), "{e}"),
    }
}

#[test]
fn select_reads_tracked_file_with_uncommitted_edit() {
    let (_tmp, repo, db, _p) = setup();
    std::fs::write(
        repo.root().join("lib.rs"),
        "pub fn allocate_budget() -> u32 { 2 } // edited\n",
    )
    .unwrap();
    let c = select_candidates(&repo, &db, "allocate_budget", &SelectOptions::new(10_000)).unwrap();
    assert!(c.iter().any(|c| c.content.contains("edited")));
}
