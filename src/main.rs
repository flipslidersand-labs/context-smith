use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Clap value_parser: reject empty strings early with a clear error message.
fn non_empty_string(s: &str) -> Result<String, String> {
    if s.trim().is_empty() {
        Err("value must not be empty".to_owned())
    } else {
        Ok(s.to_owned())
    }
}

/// Clap value_parser: reject zero budget with a clear error message.
fn positive_usize(s: &str) -> Result<usize, String> {
    let v: usize = s
        .parse()
        .map_err(|_| format!("'{}' is not a valid integer", s))?;
    if v == 0 {
        Err("--budget must be \u{2265} 1".to_owned())
    } else {
        Ok(v)
    }
}

use context_smith::{
    build_index, render_task_md, select_candidates, write_bundle, Candidate, GitRepo, IndexDb,
    SeedMode, SelectOptions,
};

#[derive(Parser)]
#[command(
    name = "contextsmith",
    version,
    about = "AI context compiler for Git repositories"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build index for a repository (saved to {repo}/.contextsmith/index.db)
    Index {
        /// Path to the target Git repository
        #[arg(long, value_parser = non_empty_string)]
        repo: String,
        /// Override the default output path (.contextsmith/index.db inside the repo)
        #[arg(long, value_parser = non_empty_string)]
        out: Option<String>,
        /// Force a full re-index even for files whose content has not changed
        #[arg(long)]
        force: bool,
    },
    /// Build a context bundle for a task
    Build {
        /// Path to the target Git repository
        #[arg(long, value_parser = non_empty_string)]
        repo: String,
        /// Task description. May start with `-` (e.g. an issue title such as "--files is slow").
        #[arg(long, value_parser = non_empty_string, allow_hyphen_values = true)]
        task: String,
        #[arg(long, default_value_t = 30000, value_parser = positive_usize)]
        budget: usize,
        #[arg(long, default_value = "context.bundle", value_parser = non_empty_string)]
        out: String,
        /// Path to index db (default: {repo}/.contextsmith/index.db); use when index was built with --out
        #[arg(long)]
        index: Option<String>,
        /// Include relevance score annotations in task.md
        #[arg(long)]
        explain: bool,
        /// Number of recent commits to include as diff context
        #[arg(long, default_value_t = 3)]
        diff_commits: usize,
        /// Disable semantic (embedding) seeds even when the index has them; BM25 only
        #[arg(long)]
        no_embed: bool,
    },
    /// Select task-relevant files and print to stdout (no files written).
    /// Designed for scripts and pipelines: `--format json` for machine-readable
    /// output, `--format md` for the task.md-style summary.
    Query {
        /// Path to the target Git repository
        #[arg(long, value_parser = non_empty_string)]
        repo: String,
        /// Task description. May start with `-` (e.g. an issue title such as "--files is slow").
        #[arg(long, value_parser = non_empty_string, allow_hyphen_values = true)]
        task: String,
        #[arg(long, default_value_t = 30000, value_parser = positive_usize)]
        budget: usize,
        /// Path to index db (default: {repo}/.contextsmith/index.db)
        #[arg(long)]
        index: Option<String>,
        /// Output format
        #[arg(long, value_enum, default_value_t = QueryFormat::Json)]
        format: QueryFormat,
        /// Include relevance score annotations (md format only)
        #[arg(long)]
        explain: bool,
        /// Disable semantic (embedding) seeds even when the index has them; BM25 only
        #[arg(long)]
        no_embed: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum QueryFormat {
    Json,
    Md,
}

fn opts(budget: usize, no_embed: bool) -> SelectOptions {
    let mut o = SelectOptions::new(budget);
    o.no_embed = no_embed;
    o
}

/// Print the selection diagnostics to stderr (same wording as before they were returned).
fn report_selection(seed_mode: &SeedMode, skipped: &[(PathBuf, String)]) {
    match seed_mode {
        SeedMode::Bm25 => {}
        SeedMode::Fused => eprintln!("semantic: RRF-fused BM25 with embedding seeds"),
        SeedMode::NoUsableEmbeddings => {
            eprintln!("semantic: no usable embeddings; using BM25 seeds only")
        }
        SeedMode::FallbackBm25(e) => {
            eprintln!("semantic: embedding failed ({e}); falling back to BM25")
        }
        _ => {}
    }
    for (abs, e) in skipped {
        eprintln!("warn: skip {:?}: {e}", abs);
    }
}

fn select_and_report(
    repo: &GitRepo,
    db: &IndexDb,
    task: &str,
    o: &SelectOptions,
) -> anyhow::Result<Vec<Candidate>> {
    match select_candidates(repo, db, task, o) {
        Ok(sel) => {
            report_selection(&sel.seed_mode, &sel.skipped);
            Ok(sel.candidates)
        }
        Err(e) => {
            // Same stderr order as before diagnostics were returned: seed message, skips, error.
            report_selection(&e.seed_mode, &e.skipped);
            Err(e.into())
        }
    }
}

/// Warn when the index was built for a different repository than `repo`
/// (`meta.repo_root`, written by `index`). Only a warning: a shipped or moved
/// index is a supported use, and the git-tracked check still gates every path.
fn warn_if_foreign_index(repo: &GitRepo, db: &IndexDb) {
    let stored = db.recorded_repo_root();
    let Some(stored) = stored else { return };
    let canon = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    if canon(std::path::Path::new(&stored)) != canon(repo.root()) {
        eprintln!("{}", foreign_index_warning(&stored, repo.root()));
    }
}

/// The text of the foreign-index warning. Both paths are printed with `{:?}` because
/// `meta.repo_root` comes from the index file and may hold terminal control characters.
fn foreign_index_warning(stored: &str, root: &std::path::Path) -> String {
    format!(
        "warn: index was built for {stored:?}, not {root:?}; results may not match this repository"
    )
}

/// Resolve the index db path for a repo, erroring if it does not exist.
fn resolve_index_path(repo: &GitRepo, index: Option<String>) -> anyhow::Result<PathBuf> {
    let db_path = match index {
        Some(p) => PathBuf::from(p),
        None => repo.root().join(".contextsmith").join("index.db"),
    };
    if !db_path.exists() {
        anyhow::bail!(
            "Index not found at {}. Run `contextsmith index --repo <path>` first.",
            db_path.display()
        );
    }
    Ok(db_path)
}

/// Bail if `path` itself is a symlink (dangling or not).
fn refuse_symlink(path: &std::path::Path) -> anyhow::Result<()> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "{} is a symlink; refusing to write the index (use --out to choose a location)",
                path.display()
            );
        }
    }
    Ok(())
}

/// Refuse if the db file or its `-wal`/`-shm` sidecars is a symlink, since
/// SQLite would write through it to wherever it points.
fn refuse_symlinked_index_files(db_path: &std::path::Path) -> anyhow::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut name = db_path.as_os_str().to_owned();
        name.push(suffix);
        refuse_symlink(&PathBuf::from(name))?;
    }
    Ok(())
}

/// Resolve the index db path for `index`, rejecting symlinked targets and
/// creating the parent directory. `out` is an explicit db path (kept outside
/// the repo, e.g. in a cache dir); otherwise `<repo>/.contextsmith/index.db`.
fn resolve_index_output(repo: &GitRepo, out: Option<&str>) -> anyhow::Result<PathBuf> {
    match out {
        Some(p) => {
            let p = PathBuf::from(p);
            refuse_symlinked_index_files(&p)?;
            if let Some(parent) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent)?;
            }
            Ok(p)
        }
        None => {
            let dir = repo.root().join(".contextsmith");
            // A committed symlink here would put index.db/-wal/-shm outside the repo.
            refuse_symlink(&dir)?;
            let db_path = dir.join("index.db");
            refuse_symlinked_index_files(&db_path)?;
            std::fs::create_dir_all(&dir)?;
            Ok(db_path)
        }
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Index { repo, out, force } => {
            let repo = GitRepo::new(&repo)?;
            let db_path = resolve_index_output(&repo, out.as_deref())?;
            let db = IndexDb::open(&db_path)?;
            let stats = build_index(&repo, &db, force)?;
            println!(
                "indexed: {} files ({} re-indexed, {} skipped, {} deleted), {} symbols, {} deps → {}",
                stats.files_total,
                stats.files_indexed,
                stats.files_skipped,
                stats.files_deleted,
                stats.symbols_total,
                stats.deps_total,
                db_path.display(),
            );

            // Optional: generate semantic embeddings via the remote embedding
            // service. Non-fatal — a failure here leaves the BM25 index usable.
            #[cfg(feature = "remote-embed")]
            match context_smith::embed_index(&repo, &db) {
                Some(Ok(n)) => println!("embeddings: {n} files (remote-embed)"),
                Some(Err(e)) => eprintln!("warning: embedding generation failed: {e}"),
                None => eprintln!(
                    "note: remote-embed built but EMBEDDING_SVC_URL unset; skipping embeddings"
                ),
            }
        }

        Command::Build {
            repo,
            task,
            budget,
            out,
            index,
            explain,
            diff_commits,
            no_embed,
        } => {
            let repo = GitRepo::new(&repo)?;
            let db_path = resolve_index_path(&repo, index)?;
            let db = IndexDb::open_for_read(&db_path)?;
            warn_if_foreign_index(&repo, &db);

            let selected = select_and_report(&repo, &db, &task, &opts(budget, no_embed))?;

            // Recent diff, then write the bundle to disk.
            let diff_summary = match repo.recent_diff(diff_commits) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("warn: recent_diff failed ({e}); omitting diff from bundle");
                    String::new()
                }
            };
            let out_path = PathBuf::from(&out);
            let stats = write_bundle(&task, budget, &selected, &diff_summary, &out_path, explain)?;

            println!(
                "bundle: {} files, {}/{} tokens → {}",
                stats.files,
                stats.used,
                budget,
                out_path.display(),
            );
        }

        Command::Query {
            repo,
            task,
            budget,
            index,
            format,
            explain,
            no_embed,
        } => {
            let repo = GitRepo::new(&repo)?;
            let db_path = resolve_index_path(&repo, index)?;
            let db = IndexDb::open_for_read(&db_path)?;
            warn_if_foreign_index(&repo, &db);

            let selected = select_and_report(&repo, &db, &task, &opts(budget, no_embed))?;
            let used: usize = selected.iter().map(|c| c.tokens()).sum();

            match format {
                QueryFormat::Json => {
                    let files: Vec<serde_json::Value> = selected
                        .iter()
                        .map(|c| {
                            serde_json::json!({
                                "path": c.path.to_string_lossy(),
                                "score": c.score,
                                "tokens": c.tokens(),
                                "truncated": c.truncated,
                            })
                        })
                        .collect();
                    let doc = serde_json::json!({
                        "task": task,
                        "budget": budget,
                        "used_tokens": used,
                        "files": files,
                    });
                    println!("{}", serde_json::to_string_pretty(&doc)?);
                }
                QueryFormat::Md => {
                    // No diff in query output — keep it focused on the selection.
                    print!("{}", render_task_md(&task, budget, &selected, "", explain));
                }
            }
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn foreign_index_warning_has_no_raw_control_characters() {
        let stored = "/r\u{1b}]0;PWNED\u{7}\u{1b}[2K\u{7f}\u{85}\u{9b}\u{2028}\u{2029}";
        let root = Path::new("/x\u{1b}[31m");
        let msg = foreign_index_warning(stored, root);
        assert!(
            msg.chars().all(|c| !c.is_control()) && !msg.contains(['\u{2028}', '\u{2029}']),
            "{msg:?}"
        );
        assert!(msg.starts_with("warn: index was built for \"/r"), "{msg}");
        assert!(msg.contains("PWNED"), "{msg}");
    }

    fn empty_repo() -> (tempfile::TempDir, GitRepo) {
        let tmp = tempfile::TempDir::new().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        (tmp, repo)
    }

    #[test]
    fn resolve_index_output_default_creates_dir() {
        let (_tmp, repo) = empty_repo();
        let p = resolve_index_output(&repo, None).unwrap();
        assert_eq!(p, repo.root().join(".contextsmith").join("index.db"));
        assert!(repo.root().join(".contextsmith").is_dir());
    }

    #[test]
    fn resolve_index_output_default_rejects_symlinked_dir() {
        let (_tmp, repo) = empty_repo();
        let outside = tempfile::TempDir::new().unwrap();
        let dir = repo.root().join(".contextsmith");
        std::os::unix::fs::symlink(outside.path(), &dir).unwrap();
        let err = resolve_index_output(&repo, None).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "{} is a symlink; refusing to write the index (use --out to choose a location)",
                dir.display()
            )
        );
    }

    #[test]
    fn resolve_index_output_rejects_symlinked_db_and_sidecars() {
        let outside = tempfile::TempDir::new().unwrap();
        for suffix in ["", "-wal", "-shm"] {
            // default branch
            let (_tmp, repo) = empty_repo();
            let dir = repo.root().join(".contextsmith");
            std::fs::create_dir(&dir).unwrap();
            let link = dir.join(format!("index.db{suffix}"));
            std::os::unix::fs::symlink(outside.path().join("x"), &link).unwrap();
            let err = resolve_index_output(&repo, None).unwrap_err().to_string();
            assert!(err.starts_with(&format!("{} is a symlink; refusing", link.display())));

            // --out branch
            let (_tmp2, repo2) = empty_repo();
            let d = tempfile::TempDir::new().unwrap();
            let db = d.path().join("c.db");
            let link = d.path().join(format!("c.db{suffix}"));
            std::os::unix::fs::symlink(outside.path().join("y"), &link).unwrap();
            let err = resolve_index_output(&repo2, db.to_str())
                .unwrap_err()
                .to_string();
            assert!(err.ends_with("(use --out to choose a location)"), "{err}");
            assert!(err.starts_with(&format!("{} is a symlink", link.display())));
        }
    }

    #[test]
    fn resolve_index_output_out_creates_parent_and_keeps_path() {
        let (_tmp, repo) = empty_repo();
        let d = tempfile::TempDir::new().unwrap();
        let db = d.path().join("a").join("b").join("i.db");
        let p = resolve_index_output(&repo, db.to_str()).unwrap();
        assert_eq!(p, db);
        assert!(db.parent().unwrap().is_dir());
        assert!(!db.exists());
        // No repo-local dir is created when --out is used.
        assert!(!repo.root().join(".contextsmith").exists());
    }

    #[test]
    fn select_candidates_ignores_file_swapped_for_external_symlink_after_index() {
        let outside = tempfile::TempDir::new().unwrap();
        let ext = outside.path().join("s.rs");
        std::fs::write(&ext, "pub fn alpha_widget() {}\n// EXTSECRET\n").unwrap();

        let tmp = tempfile::TempDir::new().unwrap();
        let git = git2::Repository::init(tmp.path()).unwrap();
        std::fs::write(tmp.path().join("a.rs"), "pub fn alpha_widget() {}\n").unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new("a.rs")).unwrap();
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let repo = GitRepo::new(tmp.path()).unwrap();
        let dbdir = tempfile::TempDir::new().unwrap();
        let db = IndexDb::open(&dbdir.path().join("i.db")).unwrap();
        build_index(&repo, &db, false).unwrap();

        // Swap after indexing: query time must not follow the new symlink.
        std::fs::remove_file(tmp.path().join("a.rs")).unwrap();
        std::os::unix::fs::symlink(&ext, tmp.path().join("a.rs")).unwrap();

        if let Ok(sel) = select_candidates(&repo, &db, "alpha_widget", &opts(10_000, true)) {
            assert!(sel
                .candidates
                .iter()
                .all(|c| !c.content.contains("EXTSECRET")));
        }
    }

    fn indexed_repo(files: &[&str]) -> (tempfile::TempDir, GitRepo, tempfile::TempDir, IndexDb) {
        let tmp = tempfile::TempDir::new().unwrap();
        let git = git2::Repository::init(tmp.path()).unwrap();
        let mut index = git.index().unwrap();
        for f in files {
            std::fs::write(tmp.path().join(f), "pub fn alpha_widget() {}\n").unwrap();
            index.add_path(Path::new(f)).unwrap();
        }
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        let dbdir = tempfile::TempDir::new().unwrap();
        let db = IndexDb::open(&dbdir.path().join("i.db")).unwrap();
        build_index(&repo, &db, false).unwrap();
        (tmp, repo, dbdir, db)
    }

    fn conn_of(dbdir: &tempfile::TempDir) -> rusqlite::Connection {
        rusqlite::Connection::open(dbdir.path().join("i.db")).unwrap()
    }

    #[test]
    fn select_candidates_propagates_files_query_error() {
        let (_t, repo, d, db) = indexed_repo(&["a.rs"]);
        conn_of(&d)
            .execute_batch("UPDATE files SET path = CAST(path AS BLOB)")
            .unwrap();
        let res = select_candidates(&repo, &db, "alpha_widget", &opts(10_000, true));
        assert!(res.is_err(), "non-NoRows DB error must not be swallowed");
    }

    #[test]
    fn select_candidates_rejects_zero_seed_count() {
        let (_t, repo, _d, db) = indexed_repo(&["a.rs"]);
        let mut o = opts(10_000, true);
        o.seed_count = 0;
        let err = select_candidates(&repo, &db, "alpha_widget", &o).unwrap_err();
        assert!(err.to_string().contains("seed_count must be >= 1"), "{err}");
    }

    #[test]
    fn select_candidates_clamps_huge_seed_count() {
        let (_t, repo, _d, db) = indexed_repo(&["a.rs"]);
        let mut o = opts(10_000, true);
        o.seed_count = usize::MAX / 2;
        let sel = select_candidates(&repo, &db, "alpha_widget", &o)
            .unwrap()
            .candidates;
        assert_eq!(sel.len(), 1);
    }

    #[test]
    fn select_candidates_skips_candidate_with_no_files_row() {
        let (_t, repo, d, db) = indexed_repo(&["a.rs", "b.rs"]);
        conn_of(&d)
            .execute_batch("PRAGMA foreign_keys=OFF; DELETE FROM files WHERE path = 'b.rs'")
            .unwrap();
        let sel = select_candidates(&repo, &db, "alpha_widget", &opts(10_000, true))
            .unwrap()
            .candidates;
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].path, PathBuf::from("a.rs"));
    }

    #[test]
    fn select_candidates_excludes_unknown_lang_files() {
        let (_t, repo, d, db) = indexed_repo(&["a.rs", "b.rs"]);
        let n = conn_of(&d)
            .execute("UPDATE files SET lang = 'unknown' WHERE path = 'b.rs'", [])
            .unwrap();
        assert_eq!(n, 1);
        let sel = select_candidates(&repo, &db, "alpha_widget", &opts(10_000, true))
            .unwrap()
            .candidates;
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].path, PathBuf::from("a.rs"));
    }

    #[test]
    fn select_candidates_errors_when_all_matches_unreadable() {
        let (t, repo, _d, db) = indexed_repo(&["a.rs"]);
        std::fs::remove_file(t.path().join("a.rs")).unwrap();
        let err = select_candidates(&repo, &db, "alpha_widget", &opts(10_000, true)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("could not be read"), "msg: {msg}");
        assert!(msg.contains("a.rs"), "msg: {msg}");
        // Diagnostics gathered before the bail still reach the caller.
        assert_eq!(err.seed_mode, SeedMode::Bm25);
        assert_eq!(err.skipped.len(), 1);
        assert_eq!(err.skipped[0].0, repo.root().join("a.rs"));
    }

    #[test]
    fn select_error_escapes_control_characters_in_unreadable_paths() {
        let name = "a\u{1b}]0;PWNED\u{7}\u{9b}\u{2028}.rs";
        let (t, repo, _d, db) = indexed_repo(&[name]);
        std::fs::remove_file(t.path().join(name)).unwrap();
        let err = select_candidates(&repo, &db, "alpha_widget", &opts(10_000, true)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("could not be read"), "msg: {msg:?}");
        assert!(msg.contains("\\x1b]0;PWNED\\x07"), "msg: {msg:?}");
        assert!(
            !msg.chars().any(|c| c.is_control() || c == '\u{2028}'),
            "msg: {msg:?}"
        );
    }

    #[test]
    fn select_candidates_skips_unreadable_file_and_keeps_others() {
        let (t, repo, _d, db) = indexed_repo(&["a.rs", "b.rs"]);
        // Vanishes after indexing: the read fails and only b.rs is skipped.
        std::fs::remove_file(t.path().join("b.rs")).unwrap();
        let sel = select_candidates(&repo, &db, "alpha_widget", &opts(10_000, true))
            .unwrap()
            .candidates;
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].path, PathBuf::from("a.rs"));
    }

    #[test]
    fn select_candidates_returns_skipped_files_and_bm25_seed_mode() {
        let (t, repo, _d, db) = indexed_repo(&["a.rs", "b.rs"]);
        std::fs::remove_file(t.path().join("b.rs")).unwrap();
        let sel = select_candidates(&repo, &db, "alpha_widget", &opts(10_000, true)).unwrap();
        assert_eq!(sel.seed_mode, SeedMode::Bm25);
        assert_eq!(sel.skipped.len(), 1);
        assert_eq!(sel.skipped[0].0, repo.root().join("b.rs"));
        assert!(!sel.skipped[0].1.is_empty());
    }

    /// Commit `files` (path, content) plus `links` (path, target) as tracked symlinks.
    fn indexed_repo_with_links(
        files: &[(&str, &str)],
        links: &[(&str, &str)],
    ) -> (tempfile::TempDir, GitRepo, tempfile::TempDir, IndexDb) {
        let tmp = tempfile::TempDir::new().unwrap();
        let git = git2::Repository::init(tmp.path()).unwrap();
        let mut index = git.index().unwrap();
        for (f, c) in files {
            std::fs::write(tmp.path().join(f), c).unwrap();
            index.add_path(Path::new(f)).unwrap();
        }
        for (l, t) in links {
            std::os::unix::fs::symlink(t, tmp.path().join(l)).unwrap();
            index.add_path(Path::new(l)).unwrap();
        }
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        let dbdir = tempfile::TempDir::new().unwrap();
        let db = IndexDb::open(&dbdir.path().join("i.db")).unwrap();
        build_index(&repo, &db, false).unwrap();
        (tmp, repo, dbdir, db)
    }

    #[test]
    fn select_candidates_errors_when_all_matches_are_empty() {
        // An empty __init__.py is indexed by path; matching only it must not be a silent success.
        let (_t, repo, _d, db) = indexed_repo_with_links(&[("zzqx_pkg.py", "")], &[]);
        let err = select_candidates(&repo, &db, "zzqx_pkg", &opts(10_000, true)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("No matching files"), "msg: {msg}");
        assert!(msg.contains("empty"), "msg: {msg}");
        assert!(msg.contains("zzqx_pkg.py"), "msg: {msg}");
    }

    #[test]
    fn select_candidates_keeps_nonempty_when_some_matches_are_empty() {
        let (_t, repo, _d, db) = indexed_repo_with_links(
            &[("zzqx_a.py", ""), ("zzqx_b.py", "def zzqx_b(): pass\n")],
            &[],
        );
        let sel = select_candidates(&repo, &db, "zzqx", &opts(10_000, true))
            .unwrap()
            .candidates;
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].path, PathBuf::from("zzqx_b.py"));
    }

    #[test]
    fn select_candidates_dedups_in_repo_symlink_keeping_real_file() {
        // "a_link.rs" sorts before "real.rs"; the real file must still be the one kept.
        let (_t, repo, _d, db) = indexed_repo_with_links(
            &[("real.rs", "pub fn alpha_widget() {}\n")],
            &[("a_link.rs", "real.rs")],
        );
        let sel = select_candidates(&repo, &db, "alpha_widget", &opts(10_000, true))
            .unwrap()
            .candidates;
        let paths: Vec<_> = sel.iter().map(|c| c.path.clone()).collect();
        assert_eq!(paths, vec![PathBuf::from("real.rs")]);
    }

    #[test]
    fn select_candidates_dedups_symlinks_without_scored_target() {
        let (_t, repo, _d, db) = indexed_repo_with_links(
            &[("zz_real.rs", "pub fn alpha_widget() {}\n")],
            &[("alpha_widget_link.rs", "zz_real.rs")],
        );
        let sel = select_candidates(&repo, &db, "alpha_widget", &opts(10_000, true))
            .unwrap()
            .candidates;
        assert_eq!(sel.len(), 1);
    }
}
