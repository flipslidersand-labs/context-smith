//! #398: branch coverage for `select_candidates` (and, through it, `dedup_by_real_path`).

use std::path::Path;

use context_smith::{build_index, select_candidates, GitRepo, IndexDb, SeedMode, SelectOptions};

fn commit_files(dir: &Path, files: &[(&str, &str)]) {
    let repo = git2::Repository::init(dir).unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    for (name, content) in files {
        let p = dir.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
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

/// Build a repo from `files` and index it.
fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, GitRepo, IndexDb, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    commit_files(&dir, files);
    let repo = GitRepo::new(&dir).unwrap();
    let db_path = tmp.path().join("index.db");
    let db = IndexDb::open(&db_path).unwrap();
    build_index(&repo, &db, false).unwrap();
    (tmp, repo, db, db_path)
}

fn opts() -> SelectOptions {
    SelectOptions::new(100_000)
}

#[test]
fn no_seed_reports_no_matching_files() {
    let (_t, repo, db, _p) = setup(&[("lib.rs", "pub fn alpha() {}\n")]);
    let e = select_candidates(&repo, &db, "zzzqqqnomatch", &opts()).unwrap_err();
    assert!(e.to_string().contains("No matching files found"), "{e}");
}

#[test]
fn all_matches_unreadable_reports_could_not_be_read() {
    let (_t, repo, db, _p) = setup(&[("lib.rs", "pub fn alpha_unique() {}\n")]);
    std::fs::remove_file(repo.root().join("lib.rs")).unwrap();
    let e = select_candidates(&repo, &db, "alpha_unique", &opts()).unwrap_err();
    let m = e.to_string();
    assert!(m.contains("matching files could not be read"), "{m}");
    assert!(m.contains("lib.rs"), "{m}");
}

#[test]
fn all_empty_matches_report_no_matching_files_with_content() {
    // Empty files match on path only.
    let (_t, repo, db, _p) = setup(&[("emptyzeta.rs", ""), ("emptyzeta2.rs", "")]);
    let e = select_candidates(&repo, &db, "emptyzeta", &opts()).unwrap_err();
    assert!(
        e.to_string().contains("No matching files with content"),
        "{e}"
    );
}

#[test]
fn unknown_lang_and_missing_row_are_skipped() {
    let (_t, repo, db, db_path) = setup(&[
        ("a.rs", "pub fn gamma_shared() {}\n"),
        ("b.rs", "pub fn gamma_shared() {}\n"),
    ]);
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    // b.rs becomes unknown-lang; the SQL filter yields no row for it (continue).
    conn.execute("UPDATE files SET lang = 'unknown' WHERE path = 'b.rs'", [])
        .unwrap();
    let c = select_candidates(&repo, &db, "gamma_shared", &opts())
        .unwrap()
        .candidates;
    let paths: Vec<_> = c.iter().map(|c| c.path.to_str().unwrap()).collect();
    assert_eq!(paths, vec!["a.rs"]);

    // Every row unknown: all seeds skipped, nothing unreadable -> empty success.
    conn.execute("UPDATE files SET lang = 'unknown'", [])
        .unwrap();
    let c = select_candidates(&repo, &db, "gamma_shared", &opts())
        .unwrap()
        .candidates;
    assert!(c.is_empty());

    // Row deleted entirely (no row) is skipped the same way.
    conn.execute("UPDATE files SET lang = 'rust'", []).unwrap();
    conn.execute("PRAGMA foreign_keys = OFF", []).unwrap();
    conn.execute("DELETE FROM files WHERE path = 'b.rs'", [])
        .unwrap();
    let c = select_candidates(&repo, &db, "gamma_shared", &opts())
        .unwrap()
        .candidates;
    let paths: Vec<_> = c.iter().map(|c| c.path.to_str().unwrap()).collect();
    assert_eq!(paths, vec!["a.rs"]);
}

#[test]
fn test_path_score_is_halved() {
    let body = "pub fn delta_shared_thing() {}\n";
    let (_t, repo, db, _p) = setup(&[("src/m.rs", body), ("tests/m.rs", body)]);
    let c = select_candidates(&repo, &db, "delta_shared_thing", &opts())
        .unwrap()
        .candidates;
    let s = |p: &str| c.iter().find(|c| c.path == Path::new(p)).unwrap().score;
    assert!(s("src/m.rs") > 0.0);
    let ratio = s("tests/m.rs") / s("src/m.rs");
    assert!((ratio - 0.5).abs() < 1e-4, "ratio {ratio}");
}

#[cfg(unix)]
mod symlink {
    use super::*;
    use std::os::unix::fs::symlink;

    fn setup_link(real: &str, link: &str) -> (tempfile::TempDir, GitRepo, IndexDb) {
        setup_links(real, &[link], true)
    }

    /// `real` is written to disk; tracked in git only if `track_real`.
    fn setup_links(
        real: &str,
        links: &[&str],
        track_real: bool,
    ) -> (tempfile::TempDir, GitRepo, IndexDb) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        std::fs::create_dir_all(&dir).unwrap();
        let body = "pub fn epsilon_linked() {}\n";
        std::fs::write(dir.join(real), body).unwrap();
        for link in links {
            symlink(real, dir.join(link)).unwrap();
        }
        let r = git2::Repository::init(&dir).unwrap();
        let sig = git2::Signature::now("t", "t@t.com").unwrap();
        let mut idx = r.index().unwrap();
        if track_real {
            idx.add_path(Path::new(real)).unwrap();
        }
        for link in links {
            idx.add_path(Path::new(link)).unwrap();
        }
        idx.write().unwrap();
        let tree = r.find_tree(idx.write_tree().unwrap()).unwrap();
        r.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        let repo = GitRepo::new(&dir).unwrap();
        let db = IndexDb::open(&tmp.path().join("index.db")).unwrap();
        build_index(&repo, &db, false).unwrap();
        (tmp, repo, db)
    }

    #[test]
    fn symlink_and_target_dedup_keeps_real_path() {
        // "a_link.rs" sorts before "z_real.rs", so the real-path preference (not the
        // smaller-path tie-break) is what keeps z_real.rs.
        let (_t, repo, db) = setup_link("z_real.rs", "a_link.rs");
        let c = select_candidates(&repo, &db, "epsilon_linked", &opts())
            .unwrap()
            .candidates;
        let paths: Vec<_> = c.iter().map(|c| c.path.to_str().unwrap()).collect();
        assert_eq!(paths, vec!["z_real.rs"]);
    }

    #[test]
    fn two_symlinks_to_unindexed_target_keep_smaller_path() {
        // The target (.txt) is not a candidate; neither symlink is at the real path, so the score/path tie-break decides.
        let (_t, repo, db) = setup_links("real.txt", &["b_link.rs", "a_link.rs"], true);
        let c = select_candidates(&repo, &db, "epsilon_linked", &opts())
            .unwrap()
            .candidates;
        let paths: Vec<_> = c.iter().map(|c| c.path.to_str().unwrap()).collect();
        assert_eq!(paths.len(), 1, "{paths:?}");
        assert_eq!(paths, vec!["a_link.rs"]);
    }
}

#[test]
fn select_options_defaults_are_20_and_2() {
    let o = SelectOptions::new(1);
    assert_eq!(o.seed_count, 20);
    assert_eq!(o.bfs_depth, 2);
}

#[test]
fn seed_count_limits_seeds() {
    let files: Vec<(String, String)> = (0..5)
        .map(|i| {
            (
                format!("f{i}.rs"),
                format!("pub fn zeta_common_{i}() {{}}\n// zeta_common\n"),
            )
        })
        .collect();
    let refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let (_t, repo, db, _p) = setup(&refs);
    let all = select_candidates(&repo, &db, "zeta_common", &opts())
        .unwrap()
        .candidates;
    let mut o = opts();
    o.seed_count = 1;
    let one = select_candidates(&repo, &db, "zeta_common", &o)
        .unwrap()
        .candidates;
    assert_eq!(all.len(), 5);
    assert_eq!(one.len(), 1);
}

#[test]
fn bfs_depth_zero_skips_dependency_expansion() {
    let (_t, repo, db, _p) = setup(&[
        (
            "src/main.py",
            "from helper import run\n\ndef eta_entry():\n    run()\n",
        ),
        ("src/helper.py", "def run():\n    pass\n"),
    ]);
    let n = |d: u8| {
        let mut o = opts();
        o.bfs_depth = d;
        select_candidates(&repo, &db, "eta_entry", &o)
            .unwrap()
            .candidates
            .len()
    };
    assert_eq!(n(0), 1);
    assert!(n(2) > n(0), "depth 2 should pull in the dependency");
}

#[test]
fn bm25_only_selection_reports_seed_mode_and_no_skips() {
    let (_t, repo, db, _p) = setup(&[("a.rs", "pub fn theta_only() {}\n")]);
    let mut o = opts();
    o.no_embed = true;
    let sel = select_candidates(&repo, &db, "theta_only", &o).unwrap();
    assert_eq!(sel.seed_mode, SeedMode::Bm25);
    assert!(sel.skipped.is_empty());
    assert_eq!(sel.candidates.len(), 1);
}

/// #465: a non-UTF-8 file of a known language is indexed by path but cannot be read; it must not
/// occupy one of the `seed_count` seed slots.
#[test]
fn unreadable_seed_is_refilled_to_seed_count() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    let git = git2::Repository::init(&dir).unwrap();
    let names = ["parser.py", "parser/a.rs", "parser/b.rs", "parser/c.rs"];
    std::fs::write(dir.join("parser.py"), b"# caf\xe9 latin-1\nx = 1\n").unwrap();
    for n in &names[1..] {
        std::fs::create_dir_all(dir.join(n).parent().unwrap()).unwrap();
        std::fs::write(dir.join(n), "pub fn unrelated() {}\n").unwrap();
    }
    let mut index = git.index().unwrap();
    for n in names {
        index.add_path(Path::new(n)).unwrap();
    }
    index.write().unwrap();
    let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    git.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();
    let repo = GitRepo::new(&dir).unwrap();
    let db = IndexDb::open(&tmp.path().join("index.db")).unwrap();
    build_index(&repo, &db, false).unwrap();

    let mut o = opts();
    o.seed_count = 2;
    o.bfs_depth = 0;
    let sel = select_candidates(&repo, &db, "parser", &o).unwrap();
    assert_eq!(sel.candidates.len(), 2, "{:?}", sel.candidates);
    assert!(sel.candidates.iter().all(|c| !c.content.is_empty()));
    let n = sel
        .skipped
        .iter()
        .filter(|(p, _)| p.ends_with("parser.py"))
        .count();
    assert_eq!(n, 1, "unreadable file reported once: {:?}", sel.skipped);
}

/// #536: empty files must not occupy seed slots; a file with a body further down the BM25 ranking
/// still becomes a seed.
#[test]
fn empty_files_do_not_fill_seed_slots() {
    let (_tmp, repo, db, _) = setup(&[
        ("parser/a.rs", ""),
        ("parser/b.rs", "  \n"),
        ("src/lib.rs", "pub fn parser() { parser(); }\n"),
    ]);
    let mut o = opts();
    o.seed_count = 2;
    o.bfs_depth = 0;
    let sel = select_candidates(&repo, &db, "parser", &o).unwrap();
    assert!(
        sel.candidates
            .iter()
            .any(|c| c.path.ends_with("src/lib.rs")),
        "{:?}",
        sel.candidates
    );
}

/// Run git in `dir` (hermetic config) and return whether it succeeded.
fn git_ok(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .expect("run git")
        .success()
}

fn git(dir: &Path, args: &[&str]) {
    assert!(git_ok(dir, args), "git {args:?} failed");
}

/// Index `dir` (already a git repo with commits) into a db outside the repo.
fn index_dir(tmp: &Path, dir: &Path) -> (GitRepo, IndexDb) {
    let repo = GitRepo::new(dir).unwrap();
    let db = IndexDb::open(&tmp.join("index.db")).unwrap();
    build_index(&repo, &db, false).unwrap();
    (repo, db)
}

/// #563: a file with an unresolved merge conflict has no stage-0 entry (only
/// stages 1-3) but is tracked, so it must still be readable and selectable.
#[test]
fn conflicted_file_is_selected() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    let widget = dir.join("widget.py");
    std::fs::write(&widget, "def widget_unique():\n    return 1\n").unwrap();
    git(&dir, &["add", "widget.py"]);
    git(&dir, &["commit", "-q", "-m", "init"]);
    git(&dir, &["checkout", "-q", "-b", "other"]);
    std::fs::write(&widget, "def widget_unique():\n    return 2\n").unwrap();
    git(&dir, &["commit", "-q", "-am", "other"]);
    git(&dir, &["checkout", "-q", "main"]);
    std::fs::write(&widget, "def widget_unique():\n    return 3\n").unwrap();
    git(&dir, &["commit", "-q", "-am", "main"]);
    assert!(
        !git_ok(&dir, &["merge", "other"]),
        "merge must conflict for this test to mean anything"
    );
    let raw = git2::Repository::open(&dir).unwrap().index().unwrap();
    assert!(raw.has_conflicts());

    let (repo, db) = index_dir(tmp.path(), &dir);
    let sel = select_candidates(&repo, &db, "widget_unique", &opts()).unwrap();
    let hit = sel
        .candidates
        .iter()
        .find(|c| c.path == Path::new("widget.py"))
        .expect("conflicted widget.py must be in the bundle");
    // The working-tree content (with conflict markers) is what gets bundled.
    assert!(hit.content.contains("<<<<<<<"), "{}", hit.content);
}

/// #563 / #395: a file removed from the git index (`git rm --cached`) is still
/// refused, and the error must not point at re-indexing as the only fix.
#[test]
fn rm_cached_file_is_refused_with_accurate_message() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("repo");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q"]);
    std::fs::write(dir.join("lib.rs"), "pub fn alpha_unique() {}\n").unwrap();
    git(&dir, &["add", "lib.rs"]);
    git(&dir, &["commit", "-q", "-m", "init"]);
    git(&dir, &["rm", "-q", "--cached", "lib.rs"]);

    let (repo, db) = index_dir(tmp.path(), &dir);
    let e = select_candidates(&repo, &db, "alpha_unique", &opts()).unwrap_err();
    let m = e.to_string();
    assert!(m.contains("could not be read (lib.rs)"), "{m}");
    assert!(m.contains("warn: skip"), "{m}");
    assert!(m.contains("only if these files changed"), "{m}");
    assert!(
        !m.contains("to refresh the index"),
        "must not suggest re-indexing as the fix: {m}"
    );
}
