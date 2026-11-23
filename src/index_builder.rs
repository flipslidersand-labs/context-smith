use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::dep_builder;
use crate::{GitRepo, Language, Symbol, SymbolExtractor};

/// Message shown when the index DB is locked by another process.
pub(crate) fn locked_message(path: &Path, detail: &str) -> String {
    // A temporary or in-memory DB has no path; don't print "Index DB  is locked".
    let name = if path.as_os_str().is_empty() {
        String::new()
    } else {
        format!(" {}", path.display())
    };
    format!(
        "Index DB{name} is locked by another process (an 'index' may be running). \
         Wait for it to finish and retry. ({detail})"
    )
}

/// True if any error in the chain is SQLITE_BUSY / SQLITE_LOCKED (by code).
pub(crate) fn is_lock_error(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        matches!(
            e.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(f, _))
                if matches!(
                    f.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
    })
}

/// True if any error in the chain is SQLITE_READONLY (any extended code), or
/// SQLITE_CANTOPEN (some SQLite builds report that when -shm/-wal cannot be
/// created in a read-only directory). Callers must rule out a missing file.
fn is_readonly_error(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        matches!(
            e.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(f, _))
                if matches!(
                    f.code,
                    rusqlite::ErrorCode::ReadOnly | rusqlite::ErrorCode::CannotOpen
                )
        )
    })
}

/// Percent-escape bytes for a SQLite `file:` URI path (keeps `/ . - _` and alphanumerics).
fn escape_uri_bytes(bytes: &[u8]) -> String {
    let mut uri = String::new();
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_') {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

/// Build the `file:...?immutable=1` URI for `path`. `None` if the path cannot
/// be represented (non-UTF-8 on non-Unix).
fn immutable_uri(path: &Path) -> Option<String> {
    #[cfg(unix)]
    let escaped = {
        use std::os::unix::ffi::OsStrExt;
        escape_uri_bytes(path.as_os_str().as_bytes())
    };
    #[cfg(not(unix))]
    let escaped = escape_uri_bytes(path.to_str()?.as_bytes());
    Some(format!("file:{escaped}?immutable=1"))
}

/// Table names `init_schema` creates with `CREATE ... IF NOT EXISTS`.
const KNOWN_TABLES: [&str; 8] = [
    "files",
    "symbols",
    "deps",
    "meta",
    "embeddings",
    "fts_path",
    "fts_symbols",
    "fts_body",
];

/// An index.db may come from an untrusted clone. Right after opening, turn off
/// `trusted_schema` and enable DEFENSIVE, then refuse a DB whose schema holds a
/// trigger or view, or a known table name that is not a table (a view named
/// `files` would slip past `CREATE TABLE IF NOT EXISTS`). Never recreates the DB.
fn harden_and_check(conn: &Connection, path: &Path) -> Result<()> {
    use rusqlite::config::DbConfig;
    // Best effort: the schema check below is what rejects a bad DB.
    if conn
        .set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)
        .is_err()
    {
        let _ = conn.execute_batch("PRAGMA trusted_schema=OFF;");
    }
    let _ = conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true);

    let names = KNOWN_TABLES
        .iter()
        .map(|n| format!("'{n}'"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT type, name FROM sqlite_master
         WHERE type IN ('trigger','view')
            OR (lower(name) IN ({names}) AND type != 'table')
         ORDER BY rowid LIMIT 1"
    );
    // Reading sqlite_master also forces a lazy (read-only) open to touch the file.
    let found = conn
        .query_row(&sql, [], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .optional()
        .map_err(|e| {
            let e = anyhow::Error::new(e);
            if is_lock_error(&e) {
                anyhow::anyhow!(locked_message(path, &format!("{e:#}")))
            } else {
                e.context(format!("Failed to read SQLite schema at {:?}", path))
            }
        })?;
    if let Some((kind, name)) = found {
        anyhow::bail!(
            "Index DB has unexpected schema object ({kind} `{name}`). Delete {} and re-run 'contextsmith index'.",
            path.display()
        );
    }
    Ok(())
}

#[non_exhaustive]
pub struct IndexDb {
    conn: Connection,
}

/// Maximum bytes of a file body stored in `fts_body`.
const FTS_BODY_MAX_BYTES: usize = 512 * 1024;

/// Most symbols stored per file; the rest are dropped with a warning (#567).
/// Provisional (about 25x the largest file seen in real repos).
const MAX_SYMBOLS_PER_FILE: usize = 10_000;

/// Keep at most `max` items of `v` (the first ones).
fn cap_symbols<T>(mut v: Vec<T>, max: usize) -> Vec<T> {
    v.truncate(max);
    v
}

/// Cut `content` to at most `FTS_BODY_MAX_BYTES`, backing up to a UTF-8 char boundary.
pub(crate) fn truncate_fts_body(content: &str) -> &str {
    if content.len() <= FTS_BODY_MAX_BYTES {
        return content;
    }
    let mut end = FTS_BODY_MAX_BYTES;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    &content[..end]
}

impl IndexDb {
    /// Open for read-only use (query/select). Uses `SQLITE_OPEN_READ_ONLY` and
    /// does not run `journal_mode`/`synchronous` PRAGMAs, so an index shipped
    /// on a read-only file or directory can still be searched. Skips the
    /// integrity check, which can misreport corruption while another process
    /// holds a write transaction. Writes through this handle fail; use
    /// [`IndexDb::open`] before writing.
    pub fn open_for_read(path: &Path) -> Result<Self> {
        match Self::open_read_only(path, false) {
            Ok(db) => Ok(db),
            // A WAL-mode DB in a read-only directory needs -shm/-wal that cannot
            // be created (SQLITE_READONLY). Nobody can be writing through this
            // directory, so fall back to an immutable (no-locking) read.
            // A missing file also yields CANTOPEN; retrying immutable would be
            // pointless, so only retry when the file exists.
            Err(e) if is_readonly_error(&e) && path.exists() => {
                eprintln!(
                    "warning: could not open {} normally ({e:#}); reopening read-only with immutable=1 (no locking)",
                    path.display()
                );
                Self::open_read_only(path, true).or(Err(e))
            }
            Err(e) => Err(e),
        }
    }

    fn open_read_only(path: &Path, immutable: bool) -> Result<Self> {
        use rusqlite::OpenFlags;
        let mut flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = if immutable {
            match immutable_uri(path) {
                Some(uri) => {
                    flags |= OpenFlags::SQLITE_OPEN_URI;
                    Connection::open_with_flags(uri, flags)
                }
                None => Err(rusqlite::Error::InvalidPath(path.to_path_buf())),
            }
        } else {
            Connection::open_with_flags(path, flags)
        }
        .with_context(|| format!("Failed to open SQLite at {:?}", path))?;
        conn.execute_batch(
            "PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;",
        )
        .with_context(|| "Failed to apply SQLite PRAGMAs")?;
        // Also touches the schema, so a non-SQLite file fails here.
        harden_and_check(&conn, path)?;
        Ok(IndexDb { conn })
    }

    /// Read-write open without the integrity check; tests only (to exercise
    /// lock handling while another connection holds a write transaction).
    #[cfg(test)]
    pub(crate) fn open_unchecked_for_test(path: &Path) -> Result<Self> {
        Self::open_inner(path, false)
    }

    pub fn open(path: &Path) -> Result<Self> {
        Self::open_inner(path, true)
    }

    fn open_inner(path: &Path, check: bool) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open SQLite at {:?}", path))?;
        // Before any write (journal_mode=WAL) so a planted schema cannot run.
        harden_and_check(&conn, path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             PRAGMA foreign_keys=ON;
             PRAGMA busy_timeout=5000;",
        )
        .with_context(|| "Failed to apply SQLite PRAGMAs")?;

        if !check {
            return Ok(IndexDb { conn });
        }

        // Verify the DB is not corrupt after an abnormal WAL termination.
        let integrity: String = conn
            .query_row("PRAGMA quick_check", [], |row| row.get(0))
            .with_context(|| "Failed to run PRAGMA quick_check")?;
        if integrity != "ok" {
            if integrity.contains("database is locked") || integrity.contains("busy") {
                anyhow::bail!(locked_message(path, &integrity));
            }
            anyhow::bail!(
                "Index DB is corrupt ({integrity}). Delete {} and re-run 'contextsmith index'.",
                path.display()
            );
        }

        Ok(IndexDb { conn })
    }

    /// Open an in-memory database; intended for tests only.
    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self> {
        let conn =
            Connection::open_in_memory().with_context(|| "Failed to open in-memory SQLite")?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .with_context(|| "Failed to apply SQLite PRAGMAs")?;
        Ok(IndexDb { conn })
    }

    /// The repository root recorded when the index was built (`meta.repo_root`),
    /// or `None` if absent (older or hand-made index).
    pub fn recorded_repo_root(&self) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key='repo_root'", [], |r| {
                r.get(0)
            })
            .ok()
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }

    pub(crate) fn init_schema(&self) -> Result<()> {
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
            );
            CREATE INDEX IF NOT EXISTS idx_symbols_file_id ON symbols(file_id);
            CREATE INDEX IF NOT EXISTS idx_deps_to_file ON deps(to_file);",
        )?;
        // Migration: add indexed_sha column to existing databases (idempotent).
        // SQLite does not support IF NOT EXISTS on ADD COLUMN, so we check first.
        self.add_column_if_missing("files", "indexed_sha")?;

        // Migration: add content_sha column to embeddings table (idempotent).
        // Tracks the blob_sha at embed time so we can skip re-embedding unchanged files.
        self.add_column_if_missing("embeddings", "content_sha")?;

        Ok(())
    }

    /// Add a nullable TEXT column to `table` unless it already exists.
    fn add_column_if_missing(&self, table: &str, column: &str) -> Result<()> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
                params![table, column],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;
        if !exists {
            self.conn
                .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} TEXT;"))?;
        }
        Ok(())
    }

    /// Return `(file_id, path, blob_sha)` for files that need (re-)embedding:
    /// those where no embedding exists yet, or where `content_sha != blob_sha`.
    pub(crate) fn files_needing_embed(&self) -> Result<Vec<(i64, String, String)>> {
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
    pub(crate) fn upsert_file(
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
                   indexed_sha = CASE WHEN lang != excluded.lang THEN NULL ELSE indexed_sha END,
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
    pub(crate) fn mark_indexed(&self, file_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET indexed_sha = blob_sha WHERE id = ?1",
            params![file_id],
        )?;
        Ok(())
    }

    pub(crate) fn delete_symbols_for(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM symbols WHERE file_id = ?1", params![file_id])?;
        Ok(())
    }

    /// Insert a symbol and return its new rowid.
    pub(crate) fn insert_symbol(&self, sym: &Symbol) -> Result<i64> {
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
        Ok(self.conn.last_insert_rowid())
    }

    #[cfg(test)]
    pub(crate) fn delete_deps_from(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM deps WHERE from_file = ?1", params![file_id])?;
        Ok(())
    }

    pub(crate) fn upsert_meta(&self, key: &str, value: &str) -> Result<()> {
        Self::upsert_meta_on(&self.conn, key, value)
    }

    /// Upsert a `meta` row on any connection (usable inside a transaction).
    pub(crate) fn upsert_meta_on(
        conn: &rusqlite::Connection,
        key: &str,
        value: &str,
    ) -> Result<()> {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Read a `meta` value on any connection.
    pub(crate) fn get_meta_on(conn: &rusqlite::Connection, key: &str) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        Ok(conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()?)
    }

    /// Delete everything derived from a file's content (symbols, symbol/body
    /// FTS, deps, embeddings) but keep its files row and path FTS row.
    fn delete_content_records(&self, file_id: i64) -> Result<()> {
        for sql in [
            "DELETE FROM fts_symbols WHERE rowid IN (SELECT id FROM symbols WHERE file_id = ?1)",
            "DELETE FROM fts_body    WHERE rowid = ?1",
            "DELETE FROM symbols     WHERE file_id = ?1",
            "DELETE FROM deps WHERE from_file = ?1 OR to_file = ?1",
            "DELETE FROM embeddings WHERE file_id = ?1",
        ] {
            self.conn.execute(sql, params![file_id])?;
        }
        Ok(())
    }

    /// Replace the path FTS row for a file with the tokenized `rel_path`.
    fn replace_fts_path(&self, file_id: i64, rel_path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM fts_path WHERE rowid = ?1", params![file_id])?;
        self.conn.execute(
            "INSERT INTO fts_path (rowid, file_id, path) VALUES (?1, ?1, ?2)",
            params![file_id, crate::tokenizer::tokenize_path(rel_path)],
        )?;
        Ok(())
    }

    /// Delete all index records for a file (FTS, symbols, deps, files row).
    fn delete_file_records(&self, file_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM fts_path    WHERE rowid = ?1", params![file_id])?;
        self.delete_content_records(file_id)?;
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

/// Version of the extraction logic and index format. Bump by hand whenever a
/// change alters what is stored: symbols/deps/FTS extraction or the index
/// format. An index built with a different value (or none) is rebuilt in full.
/// Deliberately not the crate version, so unrelated releases keep the index.
pub const EXTRACT_VERSION: u32 = 10;

const EXTRACT_VERSION_KEY: &str = "extract_version";

pub fn build_index(repo: &GitRepo, db: &IndexDb, force: bool) -> Result<IndexStats> {
    build_index_inner(repo, db, force).map_err(|e| {
        if is_lock_error(&e) {
            let path = db.connection().path().unwrap_or("").to_string();
            anyhow::anyhow!(locked_message(Path::new(&path), &format!("{e:#}")))
        } else {
            e
        }
    })
}

/// Stored extractor version. Only a missing row means "no version"; other errors
/// (SQLITE_BUSY, ...) propagate instead of silently forcing a full re-index.
fn stored_extract_version(conn: &Connection) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![EXTRACT_VERSION_KEY],
            |r| r.get(0),
        )
        .optional()?)
}

fn build_index_inner(repo: &GitRepo, db: &IndexDb, force: bool) -> Result<IndexStats> {
    db.init_schema()?;

    // An index built by a different extractor version (or a pre-versioning
    // one with no record) is rebuilt through the same path as `--force`.
    let mut force = force;
    let mut wipe_fts = false;
    if force {
        // `--force` also recomputes embeddings: mark them stale (vectors stay
        // usable until a new embedding pass replaces them).
        db.connection()
            .execute_batch("UPDATE embeddings SET content_sha = NULL;")?;
    }
    {
        let stored = stored_extract_version(db.connection())?;
        let has_files: i64 = db
            .connection()
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?;
        if has_files > 0 && stored.as_deref() != Some(EXTRACT_VERSION.to_string().as_str()) {
            if !force {
                eprintln!("note: index built by an older extractor; rebuilding");
            }
            force = true;
            // Older indexes keyed FTS rows by auto rowid; rowid-based deletes
            // would miss them, so drop them inside the rebuild transaction
            // (#480: a failed rebuild must keep the old FTS rows).
            wipe_fts = true;
        }
    }

    if force {
        db.reset_indexed_sha()?;
    }

    let (files, path_skipped) = repo.scan_with_skipped()?;
    let mut stats = IndexStats::default();
    // Non-UTF-8 paths cannot be stored; they count as seen-but-skipped (#308).
    stats.files_total += path_skipped;
    stats.files_skipped += path_skipped;

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
    let tx = rusqlite::Transaction::new_unchecked(
        db.connection(),
        rusqlite::TransactionBehavior::Immediate,
    )
    .context("Failed to begin cleanup+Pass-1 transaction")?;

    let mut file_paths: HashMap<i64, std::path::PathBuf> = HashMap::new();

    // Cleanup + Pass 1 run inside this closure so that every `?` is followed
    // by a rollback when `tx` is dropped (covers cleanup + upserts to avoid N+1 fsync and
    // SQLITE_BUSY risk).
    let pass1_result = (|| -> Result<()> {
        if wipe_fts {
            db.connection().execute_batch(
                "DELETE FROM fts_path; DELETE FROM fts_symbols; DELETE FROM fts_body;",
            )?;
        }
        // --- Cleanup: remove DB entries for files no longer in git ---
        let db_files = db.all_file_ids_and_paths()?;
        for (file_id, path) in &db_files {
            if !git_paths.contains(path) {
                db.delete_file_records(*file_id)?;
                stats.files_deleted += 1;
            }
        }

        // --- Pass 1: upsert files + incremental symbol / FTS update ---
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
                // Drop stale rows left from a previous known language (lang
                // change), keeping the files row and path FTS.
                db.delete_content_records(file_id)?;
                // Still mark as indexed so we don't re-check every run.
                db.mark_indexed(file_id)?;
                stats.files_indexed += 1;
                continue;
            }

            let abs_path = repo.root().join(rel_path);
            let source = match repo.read_tracked(rel_path) {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                    // Content is not UTF-8: drop stale symbol/body/dep/embedding
                    // rows (#286) but keep the files row and a path FTS row so
                    // the file stays findable by name (#308). indexed_sha is
                    // reset to NULL so a later restore to the same blob is
                    // re-indexed instead of skipped (#448).
                    eprintln!("warn: unreadable content {:?}: {e}", abs_path);
                    db.delete_content_records(file_id)?;
                    db.connection().execute(
                        "UPDATE files SET indexed_sha = NULL WHERE id = ?1",
                        params![file_id],
                    )?;
                    db.replace_fts_path(file_id, &rel_path.to_string_lossy())?;
                    file_paths.remove(&file_id);
                    stats.files_skipped += 1;
                    continue;
                }
                Err(e) if e.kind() == std::io::ErrorKind::FileTooLarge => {
                    // Same handling as non-UTF-8 content: keep the files row
                    // and path FTS so the file stays findable by name, drop
                    // content-derived rows, and retry on the next run (#567).
                    eprintln!("warn: skipping content of {:?}: {e}", abs_path);
                    db.delete_content_records(file_id)?;
                    db.connection().execute(
                        "UPDATE files SET indexed_sha = NULL WHERE id = ?1",
                        params![file_id],
                    )?;
                    db.replace_fts_path(file_id, &rel_path.to_string_lossy())?;
                    file_paths.remove(&file_id);
                    stats.files_skipped += 1;
                    continue;
                }
                Err(e) => {
                    eprintln!("warn: skip {:?}: {e}", abs_path);
                    // Drop stale rows (symbols/FTS/deps/embeddings/files) so
                    // search never returns content we can no longer read; the
                    // file is re-attempted on the next run (#286).
                    db.delete_file_records(file_id)?;
                    file_paths.remove(&file_id);
                    stats.files_skipped += 1;
                    continue;
                }
            };

            // (a) Clear stale symbol rows (fts_symbols rows are keyed by
            // symbols.id, so they must go first).
            db.connection().execute(
                "DELETE FROM fts_symbols WHERE rowid IN (SELECT id FROM symbols WHERE file_id = ?1)",
                params![file_id],
            )?;
            db.delete_symbols_for(file_id)?;

            // (b) Extract and insert new symbols.
            let mut raw_syms = SymbolExtractor::extract(rel_path, &source, *lang)?;
            if raw_syms.len() > MAX_SYMBOLS_PER_FILE {
                eprintln!(
                    "warn: {:?} has {} symbols; keeping the first {MAX_SYMBOLS_PER_FILE}",
                    abs_path,
                    raw_syms.len()
                );
                raw_syms = cap_symbols(raw_syms, MAX_SYMBOLS_PER_FILE);
            }
            let lines: Vec<&str> = source.lines().collect();
            // (id, name) of non-import symbols, in insertion order, for fts_symbols.
            let mut fts_syms: Vec<(i64, String)> = Vec::new();
            for (name, kind, line) in raw_syms {
                let lo = line.saturating_sub(3) as usize;
                let hi = (line as usize + 2).min(lines.len());
                let snippet = lines[lo..hi].join("\n");
                let is_import = kind.as_str() == "import";
                let sym = Symbol {
                    file_id,
                    name,
                    kind,
                    line,
                    snippet,
                };
                let sym_id = db.insert_symbol(&sym)?;
                if !is_import {
                    fts_syms.push((sym_id, sym.name));
                }
                stats.symbols_total += 1;
            }

            // (c) Replace FTS rows for this file.
            db.replace_fts_path(file_id, &rel_path.to_string_lossy())?;

            for (sym_id, name) in fts_syms {
                db.connection().execute(
                    "INSERT INTO fts_symbols (rowid, file_id, name) VALUES (?1, ?2, ?3)",
                    params![sym_id, file_id, crate::tokenizer::tokenize_code(&name)],
                )?;
            }

            db.connection()
                .execute("DELETE FROM fts_body WHERE rowid = ?1", params![file_id])?;
            let truncated = truncate_fts_body(&source);
            db.connection().execute(
                "INSERT INTO fts_body (rowid, file_id, content) VALUES (?1, ?1, ?2)",
                params![file_id, truncated],
            )?;

            // (d) Stamp indexed_sha so this file is skipped next time.
            db.mark_indexed(file_id)?;
            stats.files_indexed += 1;
        }
        // --- Pass 2: resolve imports → deps table, in the same transaction so
        // a failure never leaves new symbols with stale deps (#494). ---
        let go_modules = dep_builder::read_go_modules(repo.root(), &file_paths, &git_paths);
        dep_builder::build_deps_in_tx(db.connection(), &file_paths, &go_modules)?;
        Ok(())
    })();
    // On error, returning drops `tx`, rolling back both the stale-file deletions
    // and any Pass-1 writes so the DB is never left in a half-cleaned state.
    pass1_result?;
    tx.commit()
        .context("Failed to commit cleanup+Pass-1 transaction")?;

    stats.deps_total = db
        .connection()
        .query_row("SELECT COUNT(*) FROM deps", [], |row| row.get::<_, i64>(0))?
        as usize;

    // --- Meta ---
    let repo_root = repo.root().to_string_lossy().to_string();
    db.upsert_meta("repo_root", &repo_root)?;
    let indexed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();
    db.upsert_meta("indexed_at", &indexed_at)?;
    db.upsert_meta(EXTRACT_VERSION_KEY, &EXTRACT_VERSION.to_string())?;

    Ok(stats)
}

#[non_exhaustive]
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

    /// Hold an uncommitted write transaction on a second connection.
    fn hold_write_txn(path: &std::path::Path) -> Connection {
        let c = Connection::open(path).unwrap();
        c.execute_batch(
            "BEGIN IMMEDIATE;
             INSERT INTO files(path, lang, blob_sha, size) VALUES ('held.rs', 'rust', 'x', 1);",
        )
        .unwrap();
        c
    }

    fn seeded_db(path: &std::path::Path) {
        let db = IndexDb::open(path).unwrap();
        db.init_schema().unwrap();
    }

    #[test]
    fn open_for_read_succeeds_while_another_connection_writes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.db");
        seeded_db(&path);
        let _writer = hold_write_txn(&path);
        let db = IndexDb::open_for_read(&path);
        assert!(db.is_ok(), "{:?}", db.err());
    }

    #[test]
    fn open_does_not_report_corruption_while_another_connection_writes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.db");
        seeded_db(&path);
        let _writer = hold_write_txn(&path);
        // WAL + system SQLite では読み側が書き込み Tx に塞がれず open が成功する環境がある。
        // 失敗する場合だけ、破損ではなくロックとして報告されることを確認する。
        if let Err(e) = IndexDb::open(&path) {
            let msg = e.to_string();
            assert!(!msg.contains("corrupt"), "{msg}");
            assert!(msg.contains("locked by another process"), "{msg}");
        }
    }

    #[test]
    fn open_for_read_rejects_non_sqlite_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("garbage.db");
        std::fs::write(&path, "not sqlite".repeat(1000)).unwrap();
        assert!(IndexDb::open_for_read(&path).is_err());
    }

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
    fn upsert_file_lang_change_with_same_sha_is_changed() {
        let db = mem_db();
        let (id, _) = db
            .upsert_file(Path::new("a.rb"), Language::Unknown, "sha1", 10)
            .unwrap();
        db.mark_indexed(id).unwrap();
        let (id2, changed) = db
            .upsert_file(Path::new("a.rb"), Language::Rust, "sha1", 10)
            .unwrap();
        assert_eq!(id, id2);
        assert!(changed, "lang change must trigger re-index");
        db.mark_indexed(id2).unwrap();
        let (_, changed) = db
            .upsert_file(Path::new("a.rb"), Language::Rust, "sha1", 10)
            .unwrap();
        assert!(!changed, "same lang and sha stays unchanged");
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
    fn stored_extract_version_missing_row_is_none_but_errors_propagate() {
        let db = mem_db();
        assert_eq!(stored_extract_version(db.connection()).unwrap(), None);
        db.upsert_meta(EXTRACT_VERSION_KEY, "7").unwrap();
        assert_eq!(
            stored_extract_version(db.connection()).unwrap().as_deref(),
            Some("7")
        );
        // A query failure (here: no meta table) must not read as "no version".
        db.connection().execute_batch("DROP TABLE meta").unwrap();
        assert!(stored_extract_version(db.connection()).is_err());
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

    // ---- read-only open ----

    /// Build a WAL index for `a.rs` at `path`, then drop the connection.
    fn build_wal_index(path: &Path) {
        let (_tmp, repo) = repo_with(&[("a.rs", "pub fn readonly_marker() {}\n")]);
        let db = IndexDb::open(path).unwrap();
        build_index(&repo, &db, false).unwrap();
    }

    #[cfg(unix)]
    fn query_marker(path: &Path) -> anyhow::Result<usize> {
        let (_tmp, repo) = repo_with(&[("a.rs", "pub fn readonly_marker() {}\n")]);
        let db = IndexDb::open_for_read(path)?;
        let opts = crate::SelectOptions::new(10_000);
        Ok(
            crate::select_candidates(&repo, &db, "readonly_marker", &opts)?
                .candidates
                .len(),
        )
    }

    #[cfg(unix)]
    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// True if chmod restrictions are not enforced (e.g. uid 0): a file can be
    /// created inside a 0555 directory.
    #[cfg(unix)]
    fn permissions_not_enforced() -> bool {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("probe");
        std::fs::create_dir(&dir).unwrap();
        chmod(&dir, 0o555);
        let writable = std::fs::write(dir.join("x"), b"x").is_ok();
        chmod(&dir, 0o755);
        writable
    }

    #[cfg(unix)]
    fn skip_if_perms_not_enforced() -> bool {
        let skip = permissions_not_enforced();
        if skip {
            eprintln!("skipping: file permissions are not enforced (running as root?)");
        }
        skip
    }

    #[test]
    fn is_readonly_error_matches_readonly_and_cantopen_only() {
        let mk = |code| {
            anyhow::Error::new(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ))
        };
        assert!(is_readonly_error(&mk(rusqlite::ffi::SQLITE_READONLY)));
        assert!(is_readonly_error(&mk(
            rusqlite::ffi::SQLITE_READONLY_DBMOVED
        )));
        assert!(is_readonly_error(&mk(rusqlite::ffi::SQLITE_CANTOPEN)));
        assert!(!is_readonly_error(&mk(rusqlite::ffi::SQLITE_BUSY)));
        // found through the context chain too
        let wrapped = mk(rusqlite::ffi::SQLITE_CANTOPEN).context("open failed");
        assert!(is_readonly_error(&wrapped));
        assert!(!is_readonly_error(&anyhow::anyhow!("other")));
    }

    #[test]
    fn escape_uri_bytes_windows_path_rule() {
        // Non-Unix builds feed the path bytes through this; pin the rule here
        // since the Windows branch cannot run in Linux CI.
        assert_eq!(escape_uri_bytes(br"C:\a b\x.db"), "C%3A%5Ca%20b%5Cx.db");
    }

    #[test]
    fn immutable_uri_escapes_special_chars() {
        let uri = immutable_uri(Path::new("/tmp/a b/i#d?x%.db")).unwrap();
        assert_eq!(uri, "file:/tmp/a%20b/i%23d%3Fx%25.db?immutable=1");
    }

    #[test]
    fn open_for_read_reports_lock_not_raw_error() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.db");
        // rollback-journal DB; EXCLUSIVE blocks readers.
        let c = Connection::open(&path).unwrap();
        c.execute_batch("CREATE TABLE t(a); BEGIN EXCLUSIVE; INSERT INTO t VALUES (1);")
            .unwrap();
        let err = IndexDb::open_for_read(&path).err().expect("must fail");
        let msg = err.to_string();
        assert!(msg.contains("locked by another process"), "{msg}");
        drop(c);
    }

    #[test]
    fn open_for_read_missing_file_errors() {
        let tmp = TempDir::new().unwrap();
        assert!(IndexDb::open_for_read(&tmp.path().join("nope.db")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn open_for_read_works_on_read_only_file() {
        if skip_if_perms_not_enforced() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.db");
        build_wal_index(&path);
        chmod(&path, 0o444);
        assert_eq!(query_marker(&path).unwrap(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn open_for_read_works_on_read_only_dir_and_file() {
        if skip_if_perms_not_enforced() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("ro");
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("idx.db");
        build_wal_index(&path);
        chmod(&path, 0o444);
        chmod(&dir, 0o555);
        let res = query_marker(&path);
        chmod(&dir, 0o755); // let TempDir clean up
        assert_eq!(res.unwrap(), 1);
    }

    #[test]
    fn open_for_read_rejects_writes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.db");
        build_wal_index(&path);
        let db = IndexDb::open_for_read(&path).unwrap();
        assert!(db.upsert_meta("k", "v").is_err());
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
    fn build_index_survives_deeply_nested_file() {
        let deep = format!(
            "def top():\n    pass\nx = {}1{}\n",
            "[".repeat(30_000),
            "]".repeat(30_000)
        );
        let (_tmp, repo) = repo_with(&[("src/calc.rs", RS_SRC), ("deep.py", deep.as_str())]);
        let db = mem_db();
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_indexed, 2);
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM symbols WHERE name = 'compute_total'"
            ),
            1
        );
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
    fn build_index_force_marks_embeddings_stale() {
        let (_tmp, repo) = repo_with(&[("a.py", "def a():\n    pass\n")]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        db.connection()
            .execute(
                "INSERT INTO embeddings (file_id, vector, content_sha)
                 SELECT id, x'00', blob_sha FROM files",
                [],
            )
            .unwrap();
        build_index(&repo, &db, false).unwrap();
        assert!(db.files_needing_embed().unwrap().is_empty());
        build_index(&repo, &db, true).unwrap();
        assert_eq!(db.files_needing_embed().unwrap().len(), 1);
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

    fn stale_extractor_index_is_rebuilt(set_old: Option<&str>) {
        let (_tmp, repo) = repo_with(&[
            ("pkg/main.py", "from pkg import helper\n"),
            ("pkg/helper.py", "def h():\n    pass\n"),
            ("pkg/__init__.py", ""),
        ]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        assert!(count(&db, "SELECT COUNT(*) FROM deps") > 0);
        // Simulate an index made by an older extractor that missed the deps.
        db.connection().execute("DELETE FROM deps", []).unwrap();
        match set_old {
            Some(v) => db.upsert_meta("extract_version", v).unwrap(),
            None => {
                db.connection()
                    .execute("DELETE FROM meta WHERE key='extract_version'", [])
                    .unwrap();
            }
        }
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_skipped, 0);
        assert!(count(&db, "SELECT COUNT(*) FROM deps") > 0);
        let v: String = db
            .connection()
            .query_row(
                "SELECT value FROM meta WHERE key='extract_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, EXTRACT_VERSION.to_string());
        // Same version again: incremental, nothing re-indexed.
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_indexed, 0);
    }

    #[test]
    fn build_index_rebuilds_when_extract_version_is_older() {
        stale_extractor_index_is_rebuilt(Some("0"));
    }

    #[test]
    fn build_index_rebuilds_when_extract_version_is_missing() {
        stale_extractor_index_is_rebuilt(None);
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
    fn build_index_removes_deps_of_files_deleted_from_git() {
        let (tmp, repo) = repo_with(&[
            ("pkg/main.py", "from pkg import helper\n"),
            ("pkg/helper.py", "def h():\n    pass\n"),
            ("pkg/__init__.py", ""),
        ]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        let touching = "SELECT COUNT(*) FROM deps WHERE to_file = \
             (SELECT id FROM files WHERE path='pkg/helper.py') \
             OR from_file = (SELECT id FROM files WHERE path='pkg/helper.py')";
        assert!(count(&db, touching) > 0, "precondition: edge to helper.py");
        commit_files(tmp.path(), &[], &["pkg/helper.py"]);
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM deps WHERE to_file NOT IN (SELECT id FROM files) \
                 OR from_file NOT IN (SELECT id FROM files)"
            ),
            0
        );
    }

    #[test]
    fn locked_message_omits_an_empty_path() {
        let named = locked_message(Path::new("/x/index.db"), "busy");
        assert!(
            named.starts_with("Index DB /x/index.db is locked"),
            "{named}"
        );
        let unnamed = locked_message(Path::new(""), "busy");
        assert!(unnamed.starts_with("Index DB is locked"), "{unnamed}");
        assert!(!unnamed.contains("  "), "{unnamed}");
    }

    #[test]
    fn build_index_reports_lock_when_another_index_holds_write_txn() {
        let (_tmp, repo) = repo_with(&[("a.rs", "fn a() {}\n")]);
        let dbdir = TempDir::new().unwrap();
        let path = dbdir.path().join("idx.db");
        seeded_db(&path);
        let _writer = hold_write_txn(&path);
        let db = IndexDb::open_unchecked_for_test(&path).unwrap();
        db.connection()
            .busy_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        let err = build_index(&repo, &db, false).unwrap_err().to_string();
        assert!(err.contains("locked by another process"), "{err}");
        assert!(!err.contains("corrupt"), "{err}");
        // Not left inside a transaction.
        assert!(db.connection().is_autocommit());
    }

    #[test]
    fn build_index_reports_lock_with_force() {
        let (_tmp, repo) = repo_with(&[("a.rs", "fn a() {}\n")]);
        let dbdir = TempDir::new().unwrap();
        let path = dbdir.path().join("idx.db");
        seeded_db(&path);
        let _writer = hold_write_txn(&path);
        let db = IndexDb::open_unchecked_for_test(&path).unwrap();
        db.connection()
            .busy_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        let err = build_index(&repo, &db, true).unwrap_err().to_string();
        assert!(err.contains("locked by another process"), "{err}");
    }

    /// #231: a failing COMMIT (deferred FK violation raised by a trigger) must
    /// not leave the transaction open.
    #[test]
    fn build_index_rolls_back_when_commit_fails() {
        let (_tmp, repo) = repo_with(&[("a.rs", "fn a() {}\n")]);
        let db = mem_db();
        db.connection()
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE fk_parent (id INTEGER PRIMARY KEY);
                 CREATE TABLE fk_child (r INTEGER REFERENCES fk_parent(id) DEFERRABLE INITIALLY DEFERRED);
                 CREATE TRIGGER defer_fk AFTER INSERT ON files
                 BEGIN INSERT INTO fk_child VALUES (999); END;",
            )
            .unwrap();
        let err = build_index(&repo, &db, false).unwrap_err().to_string();
        assert!(!err.contains("locked by another process"), "{err}");
        assert!(db.connection().is_autocommit());
    }

    #[test]
    fn build_index_cleanup_failure_rolls_back_transaction() {
        let (tmp, repo) = repo_with(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        commit_files(tmp.path(), &[], &["b.rs"]);
        let tables = [
            "files",
            "symbols",
            "fts_path",
            "fts_symbols",
            "fts_body",
            "deps",
        ];
        let before: Vec<i64> = tables
            .iter()
            .map(|t| count(&db, &format!("SELECT COUNT(*) FROM {t}")))
            .collect();
        db.connection()
            .execute_batch(
                "CREATE TRIGGER fail_files_delete BEFORE DELETE ON files
                 BEGIN SELECT RAISE(ABORT, 'forced failure'); END;",
            )
            .unwrap();
        assert!(build_index(&repo, &db, false).is_err());
        assert!(db.connection().is_autocommit());
        // Nothing from the failed run was kept, and the connection is reusable.
        let after: Vec<i64> = tables
            .iter()
            .map(|t| count(&db, &format!("SELECT COUNT(*) FROM {t}")))
            .collect();
        assert_eq!(before, after, "row counts must be unchanged: {tables:?}");
        assert_eq!(count(&db, "SELECT COUNT(*) FROM files"), 2);
        db.connection()
            .execute_batch("DROP TRIGGER fail_files_delete")
            .unwrap();
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM files"), 1);
    }

    /// #480: a failed migration rebuild must not leave the FTS tables wiped.
    #[test]
    fn build_index_failed_version_rebuild_keeps_old_fts() {
        let (_tmp, repo) = repo_with(&[("a.rs", "fn alpha() {}\n"), ("b.rs", "fn beta() {}\n")]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        let fts = ["fts_path", "fts_symbols", "fts_body"];
        let before: Vec<i64> = fts
            .iter()
            .map(|t| count(&db, &format!("SELECT COUNT(*) FROM {t}")))
            .collect();
        assert!(before.iter().all(|n| *n > 0));
        db.upsert_meta(EXTRACT_VERSION_KEY, "7").unwrap();
        db.connection()
            .execute_batch(
                "CREATE TRIGGER fail_sym_insert BEFORE INSERT ON symbols
                 BEGIN SELECT RAISE(ABORT, 'forced failure'); END;",
            )
            .unwrap();
        assert!(build_index(&repo, &db, false).is_err());
        assert!(db.connection().is_autocommit());
        let after: Vec<i64> = fts
            .iter()
            .map(|t| count(&db, &format!("SELECT COUNT(*) FROM {t}")))
            .collect();
        assert_eq!(before, after, "old FTS rows must survive: {fts:?}");
        // Recovery: next run rebuilds and stamps the current version.
        db.connection()
            .execute_batch("DROP TRIGGER fail_sym_insert")
            .unwrap();
        build_index(&repo, &db, false).unwrap();
        let rebuilt: Vec<i64> = fts
            .iter()
            .map(|t| count(&db, &format!("SELECT COUNT(*) FROM {t}")))
            .collect();
        assert_eq!(before, rebuilt);
    }

    #[test]
    fn build_index_reflects_uncommitted_edit() {
        let (tmp, repo) = repo_with(&[("a.rs", "fn alpha() {}\n")]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn alpha() {}\nfn zzmarker() {}\n").unwrap();
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_indexed, 1);
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM symbols WHERE name='zzmarker'"),
            1
        );
    }

    #[test]
    fn build_index_drops_edit_after_it_is_reverted() {
        let (tmp, repo) = repo_with(&[("a.rs", "fn alpha() {}\n")]);
        let db = mem_db();
        std::fs::write(tmp.path().join("a.rs"), "fn alpha() {}\nfn yymarker() {}\n").unwrap();
        build_index(&repo, &db, false).unwrap();
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM symbols WHERE name='yymarker'"),
            1
        );
        std::fs::write(tmp.path().join("a.rs"), "fn alpha() {}\n").unwrap();
        build_index(&repo, &db, false).unwrap();
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM symbols WHERE name='yymarker'"),
            0
        );
    }

    #[test]
    fn build_index_skips_unchanged_files_with_head_blob_sha() {
        let (_tmp, repo) = repo_with(&[("a.rs", "fn alpha() {}\n")]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_skipped, 1);
        assert_eq!(stats.files_indexed, 0);
    }

    #[test]
    fn build_index_removes_embeddings_of_files_deleted_from_git() {
        let (tmp, repo) = repo_with(&[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        db.connection()
            .execute(
                "INSERT INTO embeddings (file_id, vector, content_sha) \
                 SELECT id, x'00000000', 'x' FROM files WHERE path='b.rs'",
                [],
            )
            .unwrap();
        commit_files(tmp.path(), &[], &["b.rs"]);
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_deleted, 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM files"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM embeddings"), 0);
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

    /// Commit the given already-present worktree paths (may be symlinks).
    #[cfg(unix)]
    fn commit_existing(dir: &Path, rels: &[&str]) {
        let git = git2::Repository::open(dir).unwrap();
        let mut index = git.index().unwrap();
        for r in rels {
            index.add_path(Path::new(r)).unwrap();
        }
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        let parent = git.head().unwrap().peel_to_commit().unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "more", &tree, &[&parent])
            .unwrap();
    }

    #[cfg(unix)]
    fn assert_no_leak(db: &IndexDb, needle: &str) {
        assert_eq!(
            count(
                db,
                &format!("SELECT COUNT(*) FROM symbols WHERE name='{needle}'")
            ),
            0
        );
        assert_eq!(
            count(
                db,
                &format!("SELECT COUNT(*) FROM fts_body WHERE fts_body MATCH '{needle}'")
            ),
            0
        );
        assert_eq!(
            count(
                db,
                &format!("SELECT COUNT(*) FROM fts_symbols WHERE fts_symbols MATCH '{needle}'")
            ),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn build_index_ignores_regular_file_swapped_for_external_symlink() {
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("s.rs"), "pub fn swapleak() {}\n").unwrap();
        let (tmp, repo) = repo_with(&[("a.rs", "fn a() {}\n")]);
        std::fs::remove_file(tmp.path().join("a.rs")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("s.rs"), tmp.path().join("a.rs")).unwrap();
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        assert_no_leak(&db, "swapleak");
        assert!(repo.read_tracked(Path::new("a.rs")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn build_index_ignores_file_behind_external_directory_symlink() {
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("x.rs"), "pub fn dirleak() {}\n").unwrap();
        let (tmp, repo) = repo_with(&[("d/x.rs", "fn x() {}\n")]);
        std::fs::remove_dir_all(tmp.path().join("d")).unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("d")).unwrap();
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        assert_no_leak(&db, "dirleak");
    }

    #[cfg(unix)]
    #[test]
    fn build_index_ignores_symlink_into_dot_git() {
        let (tmp, repo) = repo_with(&[("ok.rs", "fn ok() {}\n")]);
        std::fs::write(
            tmp.path().join(".git").join("secret.rs"),
            "pub fn gitleak() {}\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(".git/secret.rs", tmp.path().join("cfg.rs")).unwrap();
        commit_existing(tmp.path(), &["cfg.rs"]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        assert_no_leak(&db, "gitleak");
        assert!(repo.read_tracked(Path::new("cfg.rs")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn build_index_ignores_chained_and_dotdot_symlinks_leaving_repo() {
        let outside = TempDir::new().unwrap();
        let ext = outside.path().join("e.rs");
        std::fs::write(&ext, "pub fn chainleak() {}\n").unwrap();
        let (tmp, repo) = repo_with(&[("ok.rs", "fn ok() {}\n")]);
        // inner link -> external; chain -> inner link
        std::os::unix::fs::symlink(&ext, tmp.path().join("hop.rs")).unwrap();
        std::os::unix::fs::symlink("hop.rs", tmp.path().join("chain.rs")).unwrap();
        // relative `..` link escaping the repo
        let rel = format!(
            "../{}/e.rs",
            outside.path().file_name().unwrap().to_string_lossy()
        );
        std::os::unix::fs::symlink(&rel, tmp.path().join("up.rs")).unwrap();
        commit_existing(tmp.path(), &["hop.rs", "chain.rs", "up.rs"]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        assert_no_leak(&db, "chainleak");
    }

    #[cfg(unix)]
    #[test]
    fn build_index_keeps_unreadable_regular_file() {
        use std::os::unix::fs::PermissionsExt;
        let (tmp, repo) = repo_with(&[("ok.rs", "fn ok() {}\n")]);
        let p = tmp.path().join("ok.rs");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&p).is_ok() {
            return; // running as root: chmod does not restrict reads
        }
        let files = repo.scan().unwrap();
        assert!(files.iter().any(|(rel, ..)| rel == Path::new("ok.rs")));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn build_index_drops_stale_rows_when_tracked_file_deleted_uncommitted() {
        let (tmp, repo) = repo_with(&[
            ("a.rs", "pub fn gone_marker() {}\n"),
            ("ok.rs", "fn ok() {}\n"),
        ]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM symbols WHERE name='gone_marker'"),
            1
        );
        std::fs::remove_file(tmp.path().join("a.rs")).unwrap();
        build_index(&repo, &db, false).unwrap();
        assert_no_leak(&db, "gone_marker");
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM files WHERE path='a.rs'"),
            0
        );
    }

    #[test]
    fn build_index_drops_stale_rows_when_file_becomes_non_utf8() {
        let (tmp, repo) = repo_with(&[
            ("a.rs", "pub fn stale_marker() {}\n"),
            ("ok.rs", "fn ok() {}\n"),
        ]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM symbols WHERE name='stale_marker'"
            ),
            1
        );
        db.connection()
            .execute(
                "INSERT INTO embeddings (file_id, vector, content_sha) \
                 SELECT id, x'00000000', 'x' FROM files WHERE path='a.rs'",
                [],
            )
            .unwrap();
        std::fs::write(tmp.path().join("a.rs"), [0xff, 0xfe, 0xfd, b'\n']).unwrap();
        commit_existing(tmp.path(), &["a.rs"]);
        build_index(&repo, &db, false).unwrap();
        assert_no_leak(&db, "stale_marker");
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM fts_path WHERE file_id NOT IN (SELECT id FROM files)"
            ),
            0
        );
        assert_eq!(count(&db, "SELECT COUNT(*) FROM embeddings"), 0);
        // The files row (and its path FTS row) is kept so the file stays
        // findable by name (#308); only content-derived rows are dropped.
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM files WHERE path='a.rs'"),
            1
        );
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM files WHERE path='ok.rs'"),
            1
        );
    }

    #[test]
    fn cap_symbols_truncates_to_max() {
        let v = vec![1, 2, 3, 4, 5];
        assert_eq!(cap_symbols(v.clone(), 2), vec![1, 2]);
        assert_eq!(cap_symbols(v.clone(), 5), v);
        assert_eq!(cap_symbols(v.clone(), 9), v);
    }

    /// #567: a file that grows past MAX_FILE_BYTES keeps its files / path-FTS
    /// rows but loses every content-derived row, and is retried next run.
    #[test]
    fn build_index_drops_content_rows_when_file_exceeds_size_cap() {
        let (tmp, repo) = repo_with(&[
            ("a.rs", "pub fn stale_marker() {}\n"),
            ("ok.rs", "fn ok() {}\n"),
        ]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM symbols WHERE name='stale_marker'"
            ),
            1
        );
        // Sparse file: no real data is written.
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(tmp.path().join("a.rs"))
            .unwrap();
        f.set_len(crate::MAX_FILE_BYTES + 1).unwrap();
        drop(f);
        build_index(&repo, &db, false).unwrap();
        assert_no_leak(&db, "stale_marker");
        let a = "SELECT COUNT(*) FROM files WHERE path='a.rs'";
        assert_eq!(count(&db, a), 1);
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM symbols WHERE file_id IN (SELECT id FROM files WHERE path='a.rs')"
            ),
            0
        );
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM fts_body WHERE file_id IN (SELECT id FROM files WHERE path='a.rs')"
            ),
            0
        );
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM fts_path WHERE file_id IN (SELECT id FROM files WHERE path='a.rs')"
            ),
            1
        );
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM files WHERE path='a.rs' AND indexed_sha IS NULL"
            ),
            1
        );
        // Other files are untouched.
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM files WHERE path='ok.rs' AND indexed_sha IS NOT NULL"
            ),
            1
        );
    }

    #[test]
    fn build_index_reindexes_file_restored_from_non_utf8() {
        let (tmp, repo) = repo_with(&[("a.rs", "pub fn restored_marker() {}\n")]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        std::fs::write(tmp.path().join("a.rs"), [0xff, 0xfe, 0xfd, b'\n']).unwrap();
        commit_existing(tmp.path(), &["a.rs"]);
        build_index(&repo, &db, false).unwrap();
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM symbols WHERE name='restored_marker'"
            ),
            0
        );
        std::fs::write(tmp.path().join("a.rs"), "pub fn restored_marker() {}\n").unwrap();
        commit_existing(tmp.path(), &["a.rs"]);
        build_index(&repo, &db, false).unwrap();
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM symbols WHERE name='restored_marker'"
            ),
            1
        );
    }

    #[test]
    fn build_index_removes_deps_of_file_that_becomes_non_utf8() {
        let (tmp, repo) = repo_with(&[
            ("pkg/main.py", "from pkg import helper\n"),
            ("pkg/helper.py", "def h():\n    pass\n"),
            ("pkg/__init__.py", ""),
        ]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        let touching = "SELECT COUNT(*) FROM deps WHERE to_file = \
             (SELECT id FROM files WHERE path='pkg/helper.py') \
             OR from_file = (SELECT id FROM files WHERE path='pkg/helper.py')";
        assert!(count(&db, touching) > 0, "precondition: edge to helper.py");
        std::fs::write(tmp.path().join("pkg/helper.py"), [0xff, 0xfe, b'\n']).unwrap();
        commit_existing(tmp.path(), &["pkg/helper.py"]);
        build_index(&repo, &db, false).unwrap();
        assert_eq!(count(&db, touching), 0);
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM deps WHERE to_file NOT IN (SELECT id FROM files) \
                 OR from_file NOT IN (SELECT id FROM files)"
            ),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn read_tracked_refuses_nested_dot_git() {
        let (tmp, repo) = repo_with(&[("ok.rs", "fn ok() {}\n")]);
        let nested = tmp.path().join("sub/.git");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("config"), "token\n").unwrap();
        let e = repo.read_tracked(Path::new("sub/.git/config")).unwrap_err();
        assert!(crate::is_path_escape(&e));
    }

    #[cfg(unix)]
    #[test]
    fn read_tracked_still_reads_in_repo_symlink() {
        let (tmp, repo) = repo_with(&[("inner.rs", "fn inner_ok() {}\n")]);
        std::os::unix::fs::symlink("inner.rs", tmp.path().join("l.rs")).unwrap();
        assert!(repo
            .read_tracked(Path::new("l.rs"))
            .unwrap()
            .contains("inner_ok"));
    }

    #[cfg(unix)]
    #[test]
    fn build_index_ignores_symlink_pointing_outside_repo() {
        let outside = TempDir::new().unwrap();
        let secret = outside.path().join("secret.rs");
        std::fs::write(&secret, "pub fn leaked_secret_fn() {}\n").unwrap();

        let (tmp, repo) = repo_with(&[("ok.rs", "fn ok() {}\n"), ("inner.rs", "fn inner() {}\n")]);
        std::os::unix::fs::symlink(&secret, tmp.path().join("link.rs")).unwrap();
        // In-repo symlink keeps working.
        std::os::unix::fs::symlink("inner.rs", tmp.path().join("inner_link.rs")).unwrap();
        let git = git2::Repository::open(tmp.path()).unwrap();
        let mut index = git.index().unwrap();
        index.add_path(Path::new("link.rs")).unwrap();
        index.add_path(Path::new("inner_link.rs")).unwrap();
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        let parent = git.head().unwrap().peel_to_commit().unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "add links", &tree, &[&parent])
            .unwrap();

        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM symbols WHERE name='leaked_secret_fn'"
            ),
            0
        );
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM fts_body WHERE fts_body MATCH 'leaked_secret_fn'"
            ),
            0
        );
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM files WHERE path='link.rs'"),
            0
        );
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM files WHERE path='inner_link.rs'"),
            1
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

    /// #283: an existing index.db (no file_id indexes, FTS rows keyed by auto
    /// rowid, older extractor version) gets the new indexes and is rebuilt
    /// without leaving stale FTS rows.
    #[test]
    fn init_schema_adds_file_id_indexes_to_existing_db() {
        let (_tmp, repo) = repo_with(&[("a.rs", "pub fn alpha_marker() {}\n")]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        let conn = db.connection();
        // Simulate a pre-#283 database.
        conn.execute_batch(
            "DROP INDEX idx_symbols_file_id; DROP INDEX idx_deps_to_file;
             DELETE FROM fts_path; DELETE FROM fts_symbols; DELETE FROM fts_body;
             INSERT INTO fts_path (rowid, file_id, path) VALUES (900, 1, 'stale');
             INSERT INTO fts_symbols (rowid, file_id, name) VALUES (901, 1, 'stale');
             INSERT INTO fts_body (rowid, file_id, content) VALUES (902, 1, 'stale');
             UPDATE meta SET value = '1' WHERE key = 'extract_version';",
        )
        .unwrap();
        build_index(&repo, &db, false).unwrap();
        let idx = "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name = ";
        assert_eq!(count(&db, &format!("{idx}'idx_symbols_file_id'")), 1);
        assert_eq!(count(&db, &format!("{idx}'idx_deps_to_file'")), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM fts_path"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM fts_symbols"), 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM fts_body"), 1);
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM fts_body WHERE content = 'stale'"),
            0
        );
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

    // ---- non-UTF-8 paths / contents (#308) ----

    /// Commit files whose path / content are raw bytes (unix only).
    #[cfg(unix)]
    fn repo_with_raw(files: &[(&[u8], &[u8])]) -> (TempDir, GitRepo) {
        use std::os::unix::ffi::OsStrExt;
        let tmp = TempDir::new().unwrap();
        let git = git2::Repository::init(tmp.path()).unwrap();
        let mut index = git.index().unwrap();
        for (rel, content) in files {
            let rel = Path::new(std::ffi::OsStr::from_bytes(rel));
            let abs = tmp.path().join(rel);
            std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
            std::fs::write(&abs, content).unwrap();
            index.add_path(rel).unwrap();
        }
        index.write().unwrap();
        let tree = git.find_tree(index.write_tree().unwrap()).unwrap();
        let sig = git2::Signature::now("tester", "tester@example.com").unwrap();
        git.commit(Some("HEAD"), &sig, &sig, "raw", &tree, &[])
            .unwrap();
        let repo = GitRepo::new(tmp.path()).unwrap();
        (tmp, repo)
    }

    #[cfg(unix)]
    #[test]
    fn build_index_survives_non_utf8_directory_name() {
        let (_tmp, repo) = repo_with_raw(&[
            (b"ok.rs", b"fn ok() {}\n"),
            (b"bad\xffdir/inner.rs", b"fn inner() {}\n"),
        ]);
        let db = mem_db();
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_total, 2);
        assert_eq!(stats.files_skipped, 1);
        assert_eq!(stats.files_indexed, 1);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM files"), 1);
    }

    #[cfg(unix)]
    #[test]
    fn build_index_does_not_collapse_non_utf8_file_names() {
        let (_tmp, repo) = repo_with_raw(&[
            (b"ok.rs", b"fn ok() {}\n"),
            (b"src/a\xff.rs", b"fn one() {}\n"),
            (b"src/b\xfe.rs", b"fn two() {}\n"),
        ]);
        let db = mem_db();
        let stats = build_index(&repo, &db, false).unwrap();
        assert_eq!(stats.files_total, 3);
        assert_eq!(stats.files_skipped, 2);
        let rows = count(&db, "SELECT COUNT(*) FROM files");
        assert_eq!(rows, 1);
        assert_eq!(rows as usize, stats.files_total - stats.files_skipped);
        assert_eq!(count(&db, "SELECT COUNT(*) FROM files WHERE path='src'"), 0);
    }

    #[cfg(unix)]
    #[test]
    fn build_index_registers_path_fts_for_non_utf8_content() {
        let (_tmp, repo) = repo_with_raw(&[
            (b"ok.rs", b"fn ok() {}\n"),
            (b"zzquuxnotes.rs", b"fn gone() {}\n"),
        ]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        // Shift_JIS-ish bytes make the content unreadable as UTF-8.
        std::fs::write(
            repo.root().join("zzquuxnotes.rs"),
            [0x83, 0x65, 0x83, 0x58, 0x83, 0x67, b'\n'],
        )
        .unwrap();
        commit_existing(repo.root(), &["zzquuxnotes.rs"]);
        build_index(&repo, &db, false).unwrap();
        assert_no_leak(&db, "gone");
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM fts_path WHERE fts_path MATCH 'zzquuxnotes'"
            ),
            1
        );
        assert_eq!(
            count(&db, "SELECT COUNT(*) FROM symbols WHERE file_id IN (SELECT id FROM files WHERE path='zzquuxnotes.rs')"),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn build_index_registers_path_fts_for_new_non_utf8_content_file() {
        let (_tmp, repo) = repo_with_raw(&[
            (b"ok.rs", b"fn ok() {}\n"),
            (b"zzfreshnotes.rs", &[0x83, 0x65, 0x83, 0x58, b'\n']),
        ]);
        let db = mem_db();
        build_index(&repo, &db, false).unwrap();
        assert_eq!(
            count(
                &db,
                "SELECT COUNT(*) FROM fts_path WHERE fts_path MATCH 'zzfreshnotes'"
            ),
            1
        );
    }

    // ---- #541: untrusted index.db hardening ----

    fn trusted_schema_on(db: &IndexDb) -> bool {
        use rusqlite::config::DbConfig;
        db.connection()
            .db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA)
            .unwrap()
    }

    /// A valid index at `<tmp>/index.db`, closed again.
    fn valid_index_file() -> (TempDir, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("index.db");
        let db = IndexDb::open(&path).unwrap();
        db.init_schema().unwrap();
        drop(db);
        (tmp, path)
    }

    #[test]
    fn open_rejects_db_with_planted_trigger() {
        let (_tmp, path) = valid_index_file();
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER planted AFTER INSERT ON files
                 BEGIN INSERT OR REPLACE INTO meta(key, value) VALUES ('pwned', '1'); END;",
            )
            .unwrap();
        for result in [IndexDb::open(&path), IndexDb::open_for_read(&path)] {
            let err = result.err().expect("planted trigger must be rejected");
            let msg = format!("{err:#}");
            assert!(msg.contains("trigger `planted`"), "{msg}");
            assert!(msg.contains("Delete"), "{msg}");
        }
        let conn = Connection::open(&path).unwrap();
        let pwned: i64 = conn
            .query_row("SELECT count(*) FROM meta WHERE key='pwned'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(pwned, 0);
    }

    #[test]
    fn open_for_read_rejects_view_shadowing_a_known_table() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("index.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE real_files (id INTEGER); CREATE VIEW files AS SELECT * FROM real_files;")
            .unwrap();
        for result in [IndexDb::open_for_read(&path), IndexDb::open(&path)] {
            let err = result.err().expect("view must be rejected");
            let msg = format!("{err:#}");
            assert!(msg.contains("view `files`"), "{msg}");
            assert!(msg.contains("Delete"), "{msg}");
        }
    }

    #[test]
    fn open_rejects_known_table_name_that_is_not_a_table() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("index.db");
        Connection::open(&path)
            .unwrap()
            .execute_batch("CREATE TABLE t (a); CREATE INDEX symbols ON t(a);")
            .unwrap();
        let msg = format!("{:#}", IndexDb::open(&path).err().expect("rejected"));
        assert!(msg.contains("index `symbols`"), "{msg}");
    }

    #[test]
    fn valid_index_opens_hardened_in_both_modes() {
        let (_tmp, path) = valid_index_file();
        let rw = IndexDb::open(&path).unwrap();
        assert!(!trusted_schema_on(&rw));
        drop(rw);
        let ro = IndexDb::open_for_read(&path).unwrap();
        assert!(!trusted_schema_on(&ro));
    }
}
