use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::dep_builder;
use crate::{GitRepo, Language, Symbol, SymbolExtractor};

pub struct IndexDb {
    conn: Connection,
}

impl IndexDb {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open SQLite at {:?}", path))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;",
        )
        .with_context(|| "Failed to apply SQLite PRAGMAs")?;

        // Verify the DB is not corrupt after an abnormal WAL termination.
        let integrity: String = conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .with_context(|| "Failed to run PRAGMA quick_check")?;
        if integrity != "ok" {
            anyhow::bail!(
                "Index DB is corrupt. Delete {} and re-run 'contextsmith index'.",
                path.display()
            );
        }

        Ok(IndexDb { conn })
    }

    /// Open an in-memory database; intended for tests only.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn =
            Connection::open_in_memory().with_context(|| "Failed to open in-memory SQLite")?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .with_context(|| "Failed to apply SQLite PRAGMAs")?;
        Ok(IndexDb { conn })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                id          INTEGER PRIMARY KEY,
                path        TEXT NOT NULL UNIQUE,
                lang        TEXT NOT NULL,
                blob_sha    TEXT NOT NULL,
                indexed_sha TEXT,
                size        INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS symbols (
                id      INTEGER PRIMARY KEY,
                file_id INTEGER NOT NULL REFERENCES files(id),
                name    TEXT NOT NULL,
                kind    TEXT NOT NULL,
                line    INTEGER NOT NULL,
                snippet TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS deps (
                from_file INTEGER NOT NULL REFERENCES files(id),
                to_file   INTEGER NOT NULL REFERENCES files(id),
                PRIMARY KEY (from_file, to_file)
            );
            CREATE TABLE IF NOT EXISTS meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS fts_path
                USING fts5(file_id UNINDEXED, path);
            CREATE VIRTUAL TABLE IF NOT EXISTS fts_symbols
                USING fts5(file_id UNINDEXED, name);
            CREATE VIRTUAL TABLE IF NOT EXISTS fts_body
                USING fts5(file_id UNINDEXED, content);
            CREATE TABLE IF NOT EXISTS embeddings (
                file_id INTEGER PRIMARY KEY REFERENCES files(id),
                vector  BLOB NOT NULL
            );",
        )?;
        // Migration: add indexed_sha column to existing databases (idempotent).
        // SQLite does not support IF NOT EXISTS on ADD COLUMN, so we check first.
        let has_indexed_sha: bool = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('files') WHERE name = 'indexed_sha'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_indexed_sha {
            self.conn
                .execute_batch("ALTER TABLE files ADD COLUMN indexed_sha TEXT;")?;
        }

        // Migration: add content_sha column to embeddings table (idempotent).
        // Tracks the blob_sha at embed time so we can skip re-embedding unchanged files.
        let has_content_sha: bool = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('embeddings') WHERE name = 'content_sha'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !has_content_sha {
            self.conn
                .execute_batch("ALTER TABLE embeddings ADD COLUMN content_sha TEXT;")?;
        }

        Ok(())
    }

    /// Return `(file_id, path, blob_sha)` for files that need (re-)embedding:
    /// those where no embedding exists yet, or where `content_sha != blob_sha`.
    pub fn files_needing_embed(&self) -> Result<Vec<(i64, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.id, f.path, f.blob_sha
             FROM files f
             LEFT JOIN embeddings e ON e.file_id = f.id
             WHERE f.lang != 'unknown'
               AND (e.file_id IS NULL OR e.content_sha IS NULL OR e.content_sha != f.blob_sha)",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Upsert a file record and return `(file_id, sha_changed)`.
    /// `sha_changed` is true when `indexed_sha IS NULL OR indexed_sha != blob_sha`,
    /// meaning the file needs to be re-indexed.
    pub fn upsert_file(
        &self,
        path: &Path,
        lang: Language,
        blob_sha: &str,
        size: i64,
    ) -> Result<(i64, bool)> {
        let path_str = path.to_string_lossy();
        // Insert or update blob_sha/size/lang; do NOT touch indexed_sha so we can
        // compare the previous value against the new blob_sha.
        self.conn.execute(
            "INSERT INTO files (path, lang, blob_sha, size, indexed_sha)
             VALUES (?1, ?2, ?3, ?4, NULL)
             ON CONFLICT(path) DO UPDATE
               SET blob_sha = excluded.blob_sha,
                   size     = excluded.size,
                   lang     = excluded.lang",
            params![path_str.as_ref(), lang.as_str(), blob_sha, size],
        )?;
        let (id, changed): (i64, bool) = self.conn.query_row(
            "SELECT id,
                    CASE WHEN indexed_sha IS NULL OR indexed_sha != blob_sha THEN 1 ELSE 0 END
             FROM files WHERE path = ?1",
            params![path_str.as_ref()],
            |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0)),
        )?;
        Ok((id, changed))
    }

    /// Mark a file as fully indexed by setting indexed_sha = blob_sha.
    pub fn mark_indexed(&self, file_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET indexed_sha = blob_sha WHERE id = ?1",
            params![file_id],
        )?;
        Ok(())
    }

    pub fn delete_symbols_for(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM symbols WHERE file_id = ?1", params![file_id])?;
        Ok(())
    }

    pub fn insert_symbol(&self, sym: &Symbol) -> Result<()> {
        self.conn.execute(
            "INSERT INTO symbols (file_id, name, kind, line, snippet) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                sym.file_id,
                sym.name,
                sym.kind.as_str(),
                sym.line,
                sym.snippet
            ],
        )?;
        Ok(())
    }

    pub fn delete_deps_from(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM deps WHERE from_file = ?1", params![file_id])?;
        Ok(())
    }

    pub fn upsert_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Delete all index records for a file (FTS, symbols, deps, files row).
    fn delete_file_records(&self, file_id: i64) -> Result<()> {
        self.conn.execute(
            "DELETE FROM fts_path    WHERE file_id = ?1",
            params![file_id],
        )?;
        self.conn.execute(
            "DELETE FROM fts_symbols WHERE file_id = ?1",
            params![file_id],
        )?;
        self.conn.execute(
            "DELETE FROM fts_body    WHERE file_id = ?1",
            params![file_id],
        )?;
        self.conn.execute(
            "DELETE FROM symbols     WHERE file_id = ?1",
            params![file_id],
        )?;
        self.conn.execute(
            "DELETE FROM deps WHERE from_file = ?1 OR to_file = ?1",
            params![file_id],
        )?;
        self.conn
            .execute("DELETE FROM files WHERE id = ?1", params![file_id])?;
        Ok(())
    }

    /// Reset indexed_sha to NULL for all files, forcing a full re-index.
    fn reset_indexed_sha(&self) -> Result<()> {
        self.conn
            .execute_batch("UPDATE files SET indexed_sha = NULL;")?;
        Ok(())
    }

    /// Return all (file_id, path) pairs currently in the DB.
    fn all_file_ids_and_paths(&self) -> Result<Vec<(i64, String)>> {
        let mut stmt = self.conn.prepare("SELECT id, path FROM files")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

pub fn build_index(repo: &GitRepo, db: &IndexDb, force: bool) -> Result<IndexStats> {
    db.init_schema()?;

    if force {
        db.reset_indexed_sha()?;
    }

    let files = repo.scan()?;
    let mut stats = IndexStats::default();

    // Build a set of paths currently tracked in git.
    let git_paths: HashSet<String> = files
        .iter()
        .map(|(p, _, _, _)| p.to_string_lossy().to_string())
        .collect();

    // --- Begin a single transaction covering both cleanup and Pass 1 ---
    // Moving BEGIN IMMEDIATE before the stale-file deletion ensures that
    // delete_file_records() and the Pass-1 upserts/FTS updates are atomic.
    // If the process crashes between cleanup and COMMIT the WAL is simply
    // rolled back on next open, leaving the DB in its previous consistent state.
    db.connection()
        .execute_batch("BEGIN IMMEDIATE")
        .context("Failed to begin cleanup+Pass-1 transaction")?;

    // --- Cleanup: remove DB entries for files no longer in git ---
    let db_files = db.all_file_ids_and_paths()?;
    for (file_id, path) in &db_files {
        if !git_paths.contains(path) {
            db.delete_file_records(*file_id)?;
            stats.files_deleted += 1;
        }
    }

    // --- Pass 1: upsert files + incremental symbol / FTS update ---
    let mut file_paths: HashMap<i64, std::path::PathBuf> = HashMap::new();

    // Pass 1: upsert files + symbols — part of the same transaction started
    // above (covers cleanup + upserts to avoid N+1 fsync and SQLITE_BUSY risk).
    let pass1_result = (|| -> Result<()> {
        for (rel_path, lang, blob_sha, size) in &files {
            stats.files_total += 1;
            let (file_id, sha_changed) = db.upsert_file(rel_path, *lang, blob_sha, *size)?;
            file_paths.insert(file_id, rel_path.clone());

            if !sha_changed {
                stats.files_skipped += 1;
                continue;
            }

            // File is new or changed — re-index symbols and FTS.
            if *lang == Language::Unknown {
                // Still mark as indexed so we don't re-check every run.
                db.mark_indexed(file_id)?;
                stats.files_indexed += 1;
                continue;
            }

            let abs_path = repo.root().join(rel_path);
            let source = match std::fs::read_to_string(&abs_path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("warn: skip {:?}: {e}", abs_path);
                    stats.files_skipped += 1;
                    continue;
                }
            };

            // (a) Clear stale symbol rows.
            db.delete_symbols_for(file_id)?;

            // (b) Extract and insert new symbols.
            let raw_syms = SymbolExtractor::extract(rel_path, &source, *lang)?;
            let lines: Vec<&str> = source.lines().collect();
            for (name, kind, line) in raw_syms {
                let lo = line.saturating_sub(3) as usize;
                let hi = (line as usize + 2).min(lines.len());
                let snippet = lines[lo..hi].join("\n");
                let sym = Symbol {
                    file_id,
                    name,
                    kind,
                    line,
                    snippet,
                };
                db.insert_symbol(&sym)?;
                stats.symbols_total += 1;
            }

            // (c) Replace FTS rows for this file.
            db.connection()
                .execute("DELETE FROM fts_path WHERE file_id = ?1", params![file_id])?;
            let tokenized_path = crate::tokenizer::tokenize_path(&rel_path.to_string_lossy());
            db.connection().execute(
                "INSERT INTO fts_path (file_id, path) VALUES (?1, ?2)",
                params![file_id, tokenized_path],
            )?;

            db.connection().execute(
                "DELETE FROM fts_symbols WHERE file_id = ?1",
                params![file_id],
            )?;
            {
                let mut sym_stmt = db
                    .connection()
                    .prepare("SELECT name FROM symbols WHERE file_id = ?1 AND kind != 'import'")?;
                let sym_names: Vec<String> = sym_stmt
                    .query_map(params![file_id], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for name in sym_names {
                    db.connection().execute(
                        "INSERT INTO fts_symbols (file_id, name) VALUES (?1, ?2)",
                        params![file_id, crate::tokenizer::tokenize_code(&name)],
                    )?;
                }
            }

            db.connection()
                .execute("DELETE FROM fts_body WHERE file_id = ?1", params![file_id])?;
            let truncated = if source.len() > 512 * 1024 {
                let mut end = 512 * 1024;
                while !source.is_char_boundary(end) {
                    end -= 1;
                }
                &source[..end]
            } else {
                &source
            };
            db.connection().execute(
                "INSERT INTO fts_body (file_id, content) VALUES (?1, ?2)",
                params![file_id, truncated],
            )?;

            // (d) Stamp indexed_sha so this file is skipped next time.
            db.mark_indexed(file_id)?;
            stats.files_indexed += 1;
        }
        Ok(())
    })();
    match pass1_result {
        Ok(()) => db
            .connection()
            .execute_batch("COMMIT")
            .context("Failed to commit cleanup+Pass-1 transaction")?,
        Err(e) => {
            // Roll back both the stale-file deletions and any Pass-1 writes so
            // the DB is never left in a half-cleaned state.
            let _ = db.connection().execute_batch("ROLLBACK");
            return Err(e);
        }
    }

    // --- Pass 2: resolve imports → deps table (has its own transaction) ---
    dep_builder::build_deps(db.connection(), &file_paths)?;
    stats.deps_total = db
        .connection()
        .query_row("SELECT COUNT(*) FROM deps", [], |row| row.get(0))?;

    // --- Meta ---
    let repo_root = repo.root().to_string_lossy().to_string();
    db.upsert_meta("repo_root", &repo_root)?;
    let indexed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();
    db.upsert_meta("indexed_at", &indexed_at)?;

    Ok(stats)
}

#[derive(Debug, Default)]
pub struct IndexStats {
    pub files_total: usize,
    pub files_indexed: usize, // SHA changed → re-indexed
    pub files_skipped: usize, // SHA unchanged → skipped
    pub files_deleted: usize, // removed from git → cleaned from DB
    pub symbols_total: usize,
    pub deps_total: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::commit_files;
    use crate::SymbolKind;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn count(db: &IndexDb, sql: &str) -> i64 {
        db.connection().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn mem_db() -> IndexDb {
        let db = IndexDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        db
    }

    fn sample_symbol(file_id: i64, name: &str) -> Symbol {
        Symbol {
            file_id,
            name: name.to_string(),
            kind: SymbolKind::Function,
            line: 3,
            snippet: "fn x() {}".to_string(),
        }
    }

    // ---- IndexDb::open ----

    #[test]
    fn open_creates_file_db_in_wal_mode() {
        let tmp = TempDir::new().unwrap();
        let db = IndexDb::open(&tmp.path().join("idx.db")).unwrap();
        let mode: String = db
            .connection()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let fk: i64 = db
            .connection()
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn open_rejects_non_sqlite_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("garbage.db");
        std::fs::write(
            &path,
            "this is definitely not a sqlite database".repeat(100),
        )
        .unwrap();
        assert!(IndexDb::open(&path).is_err());
    }

    #[test]
    fn open_fails_when_parent_dir_missing() {
        let tmp = TempDir::new().unwrap();
        let err = IndexDb::open(&tmp.path().join("no/such/dir/idx.db"))
            .err()
            .unwrap();
        assert!(err.to_string().contains("Failed to open SQLite"), "{err}");
    }

    // ---- init_schema ----

    #[test]
    fn init_schema_is_idempotent_and_creates_tables() {
        let db = mem_db();
        db.init_schema().unwrap();
        for table in [
            "files",
            "symbols",
            "deps",
            "meta",
            "fts_path",
            "fts_symbols",
            "fts_body",
            "embeddings",
        ] {
            let n = count(
                &db,
                &format!("SELECT COUNT(*) FROM sqlite_master WHERE name = '{table}'"),
            );
            assert_eq!(n, 1, "table {table} missing");
        }
    }

    #[test]
    fn init_schema_migrates_legacy_tables() {
        let db = IndexDb::open_in_memory().unwrap();
        db.connection()
            .execute_batch(
                "CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE,
                    lang TEXT NOT NULL, blob_sha TEXT NOT NULL, size INTEGER NOT NULL);
                 CREATE TABLE embeddings (file_id INTEGER PRIMARY KEY, vector BLOB NOT NULL);",
            )
            .unwrap();
        db.init_schema().unwrap();
        let sha_cols = count(
            &db,
            "SELECT COUNT(*) FROM pragma_table_info('files') WHERE name='indexed_sha'",
        );
        let content_cols = count(
            &db,
            "SELECT COUNT(*) FROM pragma_table_info('embeddings') WHERE name='content_sha'",
        );
        assert_eq!((sha_cols, content_cols), (1, 1));
        // Running again must not fail on the already-migrated columns.
        db.init_schema().unwrap();
    }

    // ---- upsert_file / mark_indexed ----

    #[test]
    fn upsert_file_new_file_is_changed() {
        let db = mem_db();
        let (id, changed) = db
            .upsert_file(Path::new("a.rs"), Language::Rust, "sha1", 10)
            .unwrap();
        assert!(id > 0);
        assert!(changed);
    }

    #[test]
    fn upsert_file_after_mark_indexed_is_unchanged() {
        let db = mem_db();
        let (id, _) = db
            .upsert_file(Path::new("a.rs"), Language::Rust, "sha1", 10)
            .unwrap();
        db.mark_indexed(id).unwrap();
        let (id2, changed) = db
            .upsert_file(Path::new("a.rs"), Language::Rust, "sha1", 10)
            .unwrap();
        assert_eq!(id, id2);
        assert!(!changed);
    }

    #[test]
    fn upsert_file_with_new_sha_is_changed_and_updates_row() {
        let db = mem_db();
        let (id, _) = db
            .upsert_file(Path::new("a.rs"), Language::Rust, "sha1", 10)
            .unwrap();
        db.mark_indexed(id).unwrap();
        let (_, changed) = db
            .upsert_file(Path::new("a.rs"), Language::Python, "sha2", 99)
            .unwrap();
        assert!(changed);
        let (lang, sha, size): (String, String, i64) = db
            .connection()
            .query_row(
                "SELECT lang, blob_sha, size FROM files WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((lang.as_str(), sha.as_str(), size), ("python", "sha2", 99));
    }

    // ---- symbols / deps / meta ----

    #[test]
    fn insert_and_delete_symbols() {
        let db = mem_db();
        let (id, _) = db
            .upsert_file(Path::new("a.rs"), Language::Rust, "s", 1)
            .unwrap();
        db.insert_symbol(&sample_symbol(id, "alpha")).unwrap();
        db.insert_symbol(&sample_symbol(id, "beta")).unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM symbols"), 2);
        let kind: String = db
            .connection()
            .query_row("SELECT kind FROM symbols WHERE name='alpha'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(kind, "function");
        db.delete_symbols_for(id).unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM symbols"), 0);
    }

    #[test]
    fn insert_symbol_with_unknown_file_violates_foreign_key() {
        let db = mem_db();
        assert!(db.insert_symbol(&sample_symbol(9999, "orphan")).is_err());
    }

    #[test]
    fn delete_deps_from_removes_only_outgoing_edges() {
        let db = mem_db();
        let (a, _) = db
            .upsert_file(Path::new("a.rs"), Language::Rust, "a", 1)
            .unwrap();
        let (b, _) = db
            .upsert_file(Path::new("b.rs"), Language::Rust, "b", 1)
            .unwrap();
        db.connection()
            .execute("INSERT INTO deps VALUES (?1, ?2)", params![a, b])
            .unwrap();
        db.connection()
            .execute("INSERT INTO deps VALUES (?1, ?2)", params![b, a])
            .unwrap();
        db.delete_deps_from(a).unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM deps"), 1);
        assert_eq!(count(&db, "SELECT from_file FROM deps"), b);
    }

    #[test]
    fn upsert_meta_inserts_then_overwrites() {
        let db = mem_db();
        db.upsert_meta("k", "v1").unwrap();
        db.upsert_meta("k", "v2").unwrap();
        let v: String = db
            .connection()
            .query_row("SELECT value FROM meta WHERE key='k'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, "v2");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM meta"), 1);
    }

    // ---- files_needing_embed ----

    #[test]
    fn files_needing_embed_selects_missing_stale_and_skips_unknown() {
        let db = mem_db();
        let (none, _) = db
            .upsert_file(Path::new("none.rs"), Language::Rust, "s1", 1)
            .unwrap();
        let (fresh, _) = db
            .upsert_file(Path::new("fresh.rs"), Language::Rust, "s2", 1)
            .unwrap();
        let (stale, _) = db
            .upsert_file(Path::new("stale.rs"), Language::Rust, "s3", 1)
            .unwrap();
        let (legacy, _) = db
            .upsert_file(Path::new("legacy.rs"), Language::Rust, "s4", 1)
            .unwrap();
        db.upsert_file(Path::new("data.bin"), Language::Unknown, "s5", 1)
            .unwrap();
        let ins = "INSERT INTO embeddings (file_id, vector, content_sha) VALUES (?1, x'00', ?2)";
        db.connection().execute(ins, params![fresh, "s2"]).unwrap();
        db.connection().execute(ins, params![stale, "old"]).unwrap();
        db.connection()
            .execute(ins, params![legacy, None::<String>])
            .unwrap();

        let mut ids: Vec<i64> = db
            .files_needing_embed()
            .unwrap()
            .into_iter()
            .map(|(id, _, _)| id)
            .collect();
        ids.sort();
        let mut expected = vec![none, stale, legacy];
        expected.sort();
        assert_eq!(ids, expected);
    }

    #[test]
    fn files_needing_embed_empty_db() {
        assert!(mem_db().files_needing_embed().unwrap().is_empty());
    }

    // ---- build_index ----

    fn repo_with(files: &[(&str, &str)]) -> (TempDir, GitRepo) {
        let tmp = TempDir::new().unwrap();
        commit_files(tmp.path(), files, &[]);
        let repo = GitRepo::new(tmp.path()).unwrap();
        (tmp, repo)
    }

    const RS_SRC: &str = "pub fn compute_total() {}\nstruct Widget;\n";

    #[test]
    fn build_index_indexes_files_symbols_fts_and_meta() {
        let (_tmp, repo) = repo_with(&[("src/calc.rs", RS_SRC), ("data.bin", "xx")]);
        let db = mem_db();
        let stats = build_index(&repo, &db, false).unwrap();

        assert_eq!(stats.files_total, 2);
        assert_eq!(stats.files_indexed, 2);
        assert_eq!(stats.files_skipped, 0);
        assert_eq!(stats.files_deleted, 0);
        assert_eq!(stats.symbols_total, 2);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM fts_path"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM fts_body"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM fts_symbols"), 2);
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM files WHERE indexed_sha IS NULL"),
            0
        );
        let root: String = db
            .connection()
            .query_row("SELECT value FROM meta WHERE key='repo_root'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(PathBuf::from(root), repo.root());
        let at: String = db
            .connection()
            .query_row("SELECT value FROM meta WHERE key='indexed_at'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(at.parse::<u64>().unwrap() > 0);
    }

    #[test]
    fn build_index_snippet_covers_surrounding_lines() {
        let src = "l1\nl2\nl3\nfn target() {}\nl5\nl6\nl7\nl8\n";
        let (_tmp, repo) = repo_with(&[("a.rs", src)]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        let snippet: String = db
            .connection()
            .query_row("SELECT snippet FROM symbols WHERE name='target'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(snippet, "l2\nl3\nfn target() {}\nl5\nl6");
    }

    #[test]
    fn build_index_second_run_skips_unchanged_files() {
        let (_tmp, repo) = repo_with(&[("a.rs", RS_SRC), ("b.py", "def f():\n    pass\n")]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_total, 2);
        assert_eq!(stats.files_indexed, 0);
        assert_eq!(stats.files_skipped, 2);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM symbols"), 3);
    }

    #[test]
    fn build_index_force_reindexes_without_duplicating_rows() {
        let (_tmp, repo) = repo_with(&[("a.rs", RS_SRC)]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        let stats = build_index(&repo, &db, true).unwrap();
        assert_eq!(stats.files_indexed, 1);
        assert_eq!(stats.files_skipped, 0);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM symbols"), 2);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM fts_path"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM fts_body"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM fts_symbols"), 2);
    }

    #[test]
    fn build_index_changed_file_replaces_old_symbols() {
        let (tmp, repo) = repo_with(&[("a.rs", "fn old_name() {}\n"), ("b.rs", "fn keep() {}\n")]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        commit_files(tmp.path(), &[("a.rs", "fn new_name() {}\n")], &[]);
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_indexed, 1);
        assert_eq!(stats.files_skipped, 1);
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM symbols WHERE name='old_name'"),
            0
        );
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM symbols WHERE name='new_name'"),
            1
        );
        assert_eq!(count(&db, "SELECT COUNT(*) FROM fts_body"), 2);
    }

    #[test]
    fn build_index_removes_records_for_files_deleted_from_git() {
        let (tmp, repo) = repo_with(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        commit_files(tmp.path(), &[], &["b.rs"]);
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(stats.files_total, 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM files"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM symbols WHERE name='b'"), 0);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM fts_path"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM fts_symbols"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM fts_body"), 1);
    }

    #[test]
    fn build_index_skips_tracked_file_missing_from_worktree() {
        let (tmp, repo) = repo_with(&[("gone.rs", "fn gone() {}\n"), ("ok.rs", "fn ok() {}\n")]);
        std::fs::remove_file(tmp.path().join("gone.rs")).unwrap();
        let db = mem_db();
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_total, 2);
        assert_eq!(stats.files_indexed, 1);
        assert_eq!(stats.files_skipped, 1);
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM symbols WHERE name='gone'"),
            0
        );
    }

    #[test]
    fn build_index_truncates_large_body_on_char_boundary() {
        // 3-byte chars: 512 KiB is not a multiple of 3, so the cut must back up.
        let big = format!("# {}\n", "あ".repeat(200_000));
        let (_tmp, repo) = repo_with(&[("big.py", big.as_str())]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        let bytes = count(&db, "SELECT length(CAST(content AS BLOB)) FROM fts_body");
        assert!(bytes <= 512 * 1024, "stored {bytes} bytes");
        assert!(bytes > 512 * 1024 - 3);
    }

    #[test]
    fn build_index_import_symbols_excluded_from_fts_symbols() {
        let (_tmp, repo) = repo_with(&[("m.py", "import os\n\ndef work():\n    pass\n")]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        assert_eq!(count(&db, "SELECT COUNT(*) FROM symbols"), 2);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM fts_symbols"), 1);
    }

    #[test]
    fn build_index_resolves_deps_between_files() {
        let (_tmp, repo) = repo_with(&[
            ("pkg/main.py", "from pkg import helper\n"),
            ("pkg/helper.py", "def h():\n    pass\n"),
            ("pkg/__init__.py", ""),
        ]);
        let db = mem_db();
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(
            stats.deps_total as i64,
            count(&db, "SELECT COUNT(*) FROM deps")
        );
    }

    #[test]
    fn build_index_without_commits_errors() {
        let tmp = TempDir::new().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        let db = mem_db();
        assert!(build_index(&repo, &db, false).is_err());
    }

    #[test]
    fn build_index_works_with_file_backed_db() {
        let (tmp, repo) = repo_with(&[("a.rs", RS_SRC)]);
        let db_path = tmp.path().join(".git").join("ctx-index.db");
        let db = IndexDb::open(&db_path).unwrap();
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_indexed, 1);
        assert!(db_path.exists());
    }

    #[test]
    fn index_stats_default_is_zeroed() {
        let s = IndexStats::default();
        assert_eq!(
            (
                s.files_total,
                s.files_indexed,
                s.files_skipped,
                s.files_deleted,
                s.symbols_total,
                s.deps_total
            ),
            (0, 0, 0, 0, 0, 0)
        );
    }
}
