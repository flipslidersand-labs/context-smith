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
        Ok(())
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

    // --- Cleanup: remove DB entries for files no longer in git ---
    let db_files = db.all_file_ids_and_paths()?;
    for (file_id, path) in db_files {
        if !git_paths.contains(&path) {
            db.delete_file_records(file_id)?;
            stats.files_deleted += 1;
        }
    }

    // --- Pass 1: upsert files + incremental symbol / FTS update ---
    let mut file_paths: HashMap<i64, std::path::PathBuf> = HashMap::new();

    // Pass 1: upsert files + symbols — wrapped in a single transaction to
    // avoid N+1 fsync calls (autocommit) and reduce SQLITE_BUSY risk.
    db.connection()
        .execute_batch("BEGIN IMMEDIATE")
        .context("Failed to begin Pass-1 transaction")?;
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
            .context("Failed to commit Pass-1 transaction")?,
        Err(e) => {
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
