use anyhow::Result;
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::tokenizer::tokenize_code;

/// Populate fts_symbols and fts_body.
/// `abs_file_paths`: file_id → absolute path (for reading content).
/// Clears and rebuilds FTS tables on every call.
pub fn populate_fts_with_paths(
    conn: &Connection,
    abs_file_paths: &HashMap<i64, PathBuf>,
) -> Result<()> {
    conn.execute_batch("DELETE FROM fts_symbols; DELETE FROM fts_body;")?;

    // fts_symbols: pre-tokenized symbol names (weight ×2 in UNION ALL query)
    let mut sym_stmt = conn.prepare(
        "SELECT file_id, name FROM symbols WHERE kind != 'import'",
    )?;
    let sym_rows: Vec<(i64, String)> = sym_stmt
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?
        .filter_map(|r| r.ok())
        .collect();

    for (file_id, name) in sym_rows {
        conn.execute(
            "INSERT INTO fts_symbols (file_id, name) VALUES (?1, ?2)",
            params![file_id, tokenize_code(&name)],
        )?;
    }

    // fts_body: original source content (weight ×1, provides snippet())
    for (&file_id, abs_path) in abs_file_paths {
        let content = match std::fs::read_to_string(abs_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let truncated = if content.len() > 512 * 1024 {
            &content[..512 * 1024]
        } else {
            &content
        };
        conn.execute(
            "INSERT INTO fts_body (file_id, content) VALUES (?1, ?2)",
            params![file_id, truncated],
        )?;
    }

    Ok(())
}

/// BM25 search across fts_symbols (×2) and fts_body (×1).
/// Returns (file_id, total_score) ordered by relevance descending, limited to top_n.
///
/// FTS5 rank() returns negative values (lower = better BM25 match).
/// We negate to get positive scores, then sum with symbol weight ×2.
pub fn search_bm25(
    conn: &Connection,
    query: &str,
    top_n: usize,
) -> Result<Vec<(i64, f32)>> {
    let mut stmt = conn.prepare(
        "SELECT file_id, SUM(score) AS total
         FROM (
             SELECT file_id, rank * -2.0 AS score FROM fts_symbols WHERE name    MATCH ?1
             UNION ALL
             SELECT file_id, rank * -1.0 AS score FROM fts_body    WHERE content MATCH ?1
         )
         GROUP BY file_id
         ORDER BY total DESC
         LIMIT ?2",
    )?;
    let rows: Vec<(i64, f32)> = stmt
        .query_map(params![query, top_n as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)? as f32))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}
