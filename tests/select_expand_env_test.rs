//! #531: `CONTEXTSMITH_EXPAND` is validated before the search, so a bad value is reported even
//! when nothing matches. This file holds a single test because it mutates the process
//! environment; its own test binary keeps it from racing other tests.

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

#[test]
fn expand_env_is_validated_up_front_and_blank_means_default() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    commit_files(&dir, &[("lib.rs", "pub fn alpha_unique() {}\n")]);
    let repo = GitRepo::new(&dir).unwrap();
    let db = IndexDb::open(&tmp.path().join("index.db")).unwrap();
    build_index(&repo, &db, false).unwrap();
    let opts = SelectOptions::new(100_000);

    // Invalid value + zero hits: the config error must win over "No matching files".
    std::env::set_var("CONTEXTSMITH_EXPAND", "nope");
    let e = select_candidates(&repo, &db, "zzzqqqnomatch", &opts)
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("CONTEXTSMITH_EXPAND") && e.contains("nope"),
        "{e}"
    );

    // Invalid value + hits: still an error.
    let e = select_candidates(&repo, &db, "alpha_unique", &opts)
        .unwrap_err()
        .to_string();
    assert!(e.contains("CONTEXTSMITH_EXPAND"), "{e}");

    // Set but empty (or blank) behaves like unset.
    for v in ["", "   "] {
        std::env::set_var("CONTEXTSMITH_EXPAND", v);
        let sel = select_candidates(&repo, &db, "alpha_unique", &opts).unwrap();
        assert_eq!(sel.candidates.len(), 1, "value {v:?}");
    }

    // A valid value with zero hits is still "No matching files".
    std::env::set_var("CONTEXTSMITH_EXPAND", "bfs");
    let e = select_candidates(&repo, &db, "zzzqqqnomatch", &opts)
        .unwrap_err()
        .to_string();
    assert!(e.contains("No matching files found"), "{e}");
    std::env::remove_var("CONTEXTSMITH_EXPAND");
}
