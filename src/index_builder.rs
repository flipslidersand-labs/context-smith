use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::Path;

use crate::dep_builder;
use crate::search_index;
use crate::{GitRepo, Language, Symbol, SymbolExtractor};

pub struct IndexDb {
    conn: Connection,
}

impl IndexDb {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("Failed to open SQLite at {:?}", path))?;
        Ok(IndexDb { conn })
    }

    pub fn connection(&self) -> &Connection {
        &self.conn
    }

    pub fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                id       INTEGER PRIMARY KEY,
                path     TEXT NOT NULL UNIQUE,
                lang     TEXT NOT NULL,
                blob_sha TEXT NOT NULL,
                size     INTEGER NOT NULL
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
        Ok(())
    }

    pub fn upsert_file(
        &self,
        path: &Path,
        lang: Language,
        blob_sha: &str,
        size: i64,
    ) -> Result<i64> {
        let path_str = path.to_string_lossy();
        self.conn.execute(
            "INSERT INTO files (path, lang, blob_sha, size)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path) DO UPDATE SET blob_sha=excluded.blob_sha, size=excluded.size, lang=excluded.lang",
            params![path_str.as_ref(), lang.as_str(), blob_sha, size],
        )?;
        let id = self.conn.query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![path_str.as_ref()],
            |row| row.get(0),
        )?;
        Ok(id)
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
}

pub fn build_index(repo: &GitRepo, db: &IndexDb) -> Result<IndexStats> {
    db.init_schema()?;
    let files = repo.scan()?;
    let mut stats = IndexStats::default();
    let mut file_paths: HashMap<i64, std::path::PathBuf> = HashMap::new();

    // Pass 1: upsert files + symbols
    for (rel_path, lang, blob_sha, size) in &files {
        stats.files_total += 1;
        let file_id = db.upsert_file(rel_path, *lang, blob_sha, *size)?;
        file_paths.insert(file_id, rel_path.clone());

        if *lang == Language::Unknown {
            continue;
        }

        let abs_path = repo.root().join(rel_path);
        let source = match std::fs::read_to_string(&abs_path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let raw_syms = SymbolExtractor::extract(rel_path, &source, *lang)?;
        let lines: Vec<&str> = source.lines().collect();

        db.delete_symbols_for(file_id)?;
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
        stats.files_indexed += 1;
    }

    // Pass 2: resolve imports → deps table (build_deps clears and rebuilds atomically)
    dep_builder::build_deps(db.connection(), &file_paths)?;
    stats.deps_total = db
        .connection()
        .query_row("SELECT COUNT(*) FROM deps", [], |row| row.get(0))?;

    // Pass 3: populate FTS5 tables + meta
    // fts_body reads from absolute paths; store abs paths in a temp file_paths_abs map
    let abs_file_paths: HashMap<i64, std::path::PathBuf> = file_paths
        .iter()
        .map(|(&id, rel)| (id, repo.root().join(rel)))
        .collect();
    search_index::populate_fts_with_paths(db.connection(), &file_paths, &abs_file_paths)?;

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
    pub files_indexed: usize,
    pub symbols_total: usize,
    pub deps_total: usize,
}
