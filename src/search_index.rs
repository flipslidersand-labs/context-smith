use anyhow::Result;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::tokenizer::{tokenize_code, tokenize_path};

// BM25 signal weights (higher = stronger signal).
// path: file-name/directory match is the strongest localization cue.
// symbols: function/type names match the task vocabulary directly.
// body: full-text fallback, broadest but noisiest.
const W_PATH: f64 = 3.0;
const W_SYMBOLS: f64 = 2.0;
const W_BODY: f64 = 1.0;

/// Strip FTS5 operator characters that would cause a syntax error in MATCH queries.
/// Returns a string that may be empty or whitespace-only; callers must guard against that.
pub(crate) fn sanitize_fts_query(query: &str) -> String {
    query.replace(['(', ')', '"', '*', '-', ':', '^', '{', '}', '~', '!'], " ")
}

/// Populate fts_path, fts_symbols and fts_body inside a single transaction.
/// Clears all three tables atomically before rebuilding; rolls back on any error.
pub fn populate_fts_with_paths(
    conn: &Connection,
    rel_file_paths: &HashMap<i64, PathBuf>,
    abs_file_paths: &HashMap<i64, PathBuf>,
) -> Result<()> {
    conn.execute_batch("BEGIN")?;
    let result = (|| -> Result<()> {
        conn.execute_batch("DELETE FROM fts_path; DELETE FROM fts_symbols; DELETE FROM fts_body;")?;

        // fts_path: tokenized file path (weight W_PATH in UNION ALL query)
        for (&file_id, rel_path) in rel_file_paths {
            let tokenized = tokenize_path(&rel_path.to_string_lossy());
            conn.execute(
                "INSERT INTO fts_path (file_id, path) VALUES (?1, ?2)",
                params![file_id, tokenized],
            )?;
        }

        // fts_symbols: pre-tokenized symbol names (weight W_SYMBOLS)
        let mut sym_stmt =
            conn.prepare("SELECT file_id, name FROM symbols WHERE kind != 'import'")?;
        let sym_rows: Vec<(i64, String)> = sym_stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        for (file_id, name) in sym_rows {
            conn.execute(
                "INSERT INTO fts_symbols (file_id, name) VALUES (?1, ?2)",
                params![file_id, tokenize_code(&name)],
            )?;
        }

        // fts_body: original source content (weight W_BODY, provides snippet())
        for (&file_id, abs_path) in abs_file_paths {
            let content = match std::fs::read_to_string(abs_path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("warn: skip {:?}: {e}", abs_path);
                    continue;
                }
            };
            // Truncate at a safe char boundary to avoid UTF-8 panics on multi-byte characters
            let truncated = if content.len() > 512 * 1024 {
                let mut end = 512 * 1024;
                while !content.is_char_boundary(end) {
                    end -= 1;
                }
                &content[..end]
            } else {
                &content
            };
            conn.execute(
                "INSERT INTO fts_body (file_id, content) VALUES (?1, ?2)",
                params![file_id, truncated],
            )?;
        }

        Ok(())
    })();

    match result {
        Ok(()) => conn.execute_batch("COMMIT").map_err(Into::into),
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// BM25 search across fts_path (×3), fts_symbols (×2) and fts_body (×1).
/// Sanitizes the query before passing to FTS5 MATCH to prevent SQL syntax errors.
/// Returns an empty list when the sanitized query is blank (e.g. `--task '()'`).
pub fn search_bm25(conn: &Connection, query: &str, top_n: usize) -> Result<Vec<(i64, f32)>> {
    let sanitized = sanitize_fts_query(query);
    if sanitized.trim().is_empty() {
        return Ok(vec![]);
    }
    let mut stmt = conn.prepare(
        "SELECT file_id, SUM(score) AS total
         FROM (
             SELECT file_id, rank * -?1 AS score FROM fts_path    WHERE path    MATCH ?4
             UNION ALL
             SELECT file_id, rank * -?2 AS score FROM fts_symbols WHERE name    MATCH ?4
             UNION ALL
             SELECT file_id, rank * -?3 AS score FROM fts_body    WHERE content MATCH ?4
         )
         GROUP BY file_id
         ORDER BY total DESC
         LIMIT ?5",
    )?;
    let rows: Vec<(i64, f32)> = stmt
        .query_map(
            params![W_PATH, W_SYMBOLS, W_BODY, sanitized, top_n as i64],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)? as f32)),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::sanitize_fts_query;

    #[test]
    fn sanitize_removes_fts5_operators() {
        let result = sanitize_fts_query("foo(bar)*baz");
        // operator chars become spaces; words survive
        assert!(result.contains("foo"));
        assert!(result.contains("bar"));
        assert!(result.contains("baz"));
        assert!(!result.contains('('));
        assert!(!result.contains('*'));
    }

    #[test]
    fn sanitize_all_operators_returns_whitespace() {
        // These inputs must NOT reach the SQLite MATCH clause
        assert!(sanitize_fts_query("()").trim().is_empty());
        assert!(sanitize_fts_query("***").trim().is_empty());
        assert!(sanitize_fts_query("(\"*\")").trim().is_empty());
    }

    #[test]
    fn sanitize_empty_input() {
        assert!(sanitize_fts_query("").trim().is_empty());
    }

    #[test]
    fn sanitize_plain_word_unchanged() {
        assert_eq!(sanitize_fts_query("hello").trim(), "hello");
    }
}
