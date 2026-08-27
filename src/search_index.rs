use anyhow::{bail, Result};
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
/// Additionally, FTS5 boolean keywords (AND, OR, NOT, NEAR) are recognised only in
/// uppercase; downcasing them neutralises their operator role so they are treated as
/// plain search terms instead of query syntax.
/// Returns an empty string (which callers should reject before issuing MATCH) if the
/// result contains no non-whitespace characters.
pub(crate) fn sanitize_fts_query(query: &str) -> String {
    let stripped = query.replace(['(', ')', '"', '*', '-', ':', '^', '{', '}', '~', '!'], " ");
    stripped
        .split_whitespace()
        .map(|w| match w {
            "AND" | "OR" | "NOT" | "NEAR" => w.to_lowercase(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
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
///
/// Each sub-table's raw BM25 `rank` is normalized to [0, 1] via `rank / MAX(rank)` so
/// that the absolute magnitude difference between small path tables and large body tables
/// does not drown out the configured weights.  Each sub-query is also limited to
/// `top_n * 3` rows before the UNION ALL to prevent intermediate memory explosion.
///
/// Returns an empty Vec when the sanitized query is blank (nothing to match).
pub fn search_bm25(conn: &Connection, query: &str, top_n: usize) -> Result<Vec<(i64, f32)>> {
    let sanitized = sanitize_fts_query(query);
    // Guard: an empty MATCH expression is a SQLite FTS5 syntax error.
    if sanitized.trim().is_empty() {
        bail!(
            "task string is empty or contains only punctuation — please provide at least one word"
        );
    }

    // Each sub-query: normalize raw BM25 rank to [0,1] then apply the weight.
    // `rank` in FTS5 is ≤ 0 (more negative = better); dividing by MIN(rank) (the most
    // negative value, i.e. best match) yields a [0,1] ratio that is table-agnostic, so
    // the configured weights are not dominated by the size of fts_body.
    //
    // NULLIF(MIN(rank) OVER (), 0) prevents NULL propagation when MIN(rank) = 0
    // (SQLite treats x / 0 as NULL, which then silently drops rows from SUM and ORDER BY).
    // COALESCE(SUM(score), 0) provides an additional defence so that a sub-query
    // returning no rows still contributes 0 rather than NULL to the aggregate.
    //
    // Sub-queries are wrapped in derived tables so LIMIT applies before UNION ALL,
    // preventing intermediate memory explosion.
    let sub_limit = (top_n * 3) as i64;
    let mut stmt = conn.prepare(
        "SELECT file_id, COALESCE(SUM(score), 0) AS total
         FROM (
             SELECT file_id, (rank / NULLIF(MIN(rank) OVER (), 0)) * ?1 AS score
             FROM (SELECT file_id, rank FROM fts_path    WHERE path    MATCH ?4 LIMIT ?5)
             UNION ALL
             SELECT file_id, (rank / NULLIF(MIN(rank) OVER (), 0)) * ?2 AS score
             FROM (SELECT file_id, rank FROM fts_symbols WHERE name    MATCH ?4 LIMIT ?5)
             UNION ALL
             SELECT file_id, (rank / NULLIF(MIN(rank) OVER (), 0)) * ?3 AS score
             FROM (SELECT file_id, rank FROM fts_body    WHERE content MATCH ?4 LIMIT ?5)
         )
         GROUP BY file_id
         ORDER BY total DESC
         LIMIT ?6",
    )?;
    let rows: Vec<(i64, f32)> = stmt
        .query_map(
            params![
                W_PATH,
                W_SYMBOLS,
                W_BODY,
                sanitized,
                sub_limit,
                top_n as i64
            ],
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

    #[test]
    fn sanitize_downcases_fts5_boolean_keywords() {
        // AND/OR/NOT/NEAR must be lowercased so FTS5 treats them as plain terms
        let result = sanitize_fts_query("authentication OR 1=1");
        assert!(
            !result.contains("OR"),
            "OR should be downcased, got: {result}"
        );
        assert!(
            result.contains("or"),
            "OR should become 'or', got: {result}"
        );

        let result = sanitize_fts_query("foo AND bar");
        assert!(
            !result.contains("AND"),
            "AND should be downcased, got: {result}"
        );

        let result = sanitize_fts_query("foo NOT bar");
        assert!(
            !result.contains("NOT"),
            "NOT should be downcased, got: {result}"
        );

        let result = sanitize_fts_query("NEAR(foo bar)");
        // parens are stripped, NEAR is lowercased
        assert!(
            !result.contains("NEAR"),
            "NEAR should be downcased, got: {result}"
        );
    }

    #[test]
    fn sanitize_preserves_mixed_case_non_keywords() {
        // Words that are not exactly AND/OR/NOT/NEAR should survive unchanged
        let result = sanitize_fts_query("And Oregon Notify");
        assert!(
            result.contains("And"),
            "mixed-case 'And' should be preserved, got: {result}"
        );
        assert!(result.contains("Oregon"), "got: {result}");
        assert!(result.contains("Notify"), "got: {result}");
    }
}
