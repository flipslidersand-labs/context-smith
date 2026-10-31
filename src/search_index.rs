use anyhow::{bail, Result};
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::tokenizer::{tokenize_code, tokenize_path};

// BM25 signal weights (higher = stronger signal).
// path: file-name/directory match is the strongest localization cue.
// symbols: function/type names match the task vocabulary directly.
// body: full-text fallback, broadest but noisiest.
const W_PATH: f64 = 3.0;
const W_SYMBOLS: f64 = 2.0;
const W_BODY: f64 = 1.0;

/// Keep only bareword characters (alphanumerics and `_`); everything else could be
/// FTS5 query syntax and would cause a syntax error in MATCH queries.
/// Additionally, FTS5 boolean keywords (AND, OR, NOT, NEAR) are recognised only in
/// uppercase; downcasing them neutralises their operator role so they are treated as
/// plain search terms instead of query syntax.
/// Terms are joined with an explicit `OR` (FTS5's implicit AND returned no files for
/// multi-word tasks); BM25 ranks files matching more terms higher.
/// Returns an empty string (which callers should reject before issuing MATCH) if the
/// result contains no non-whitespace characters.
pub(crate) fn sanitize_fts_query(query: &str) -> String {
    // Allowlist, not denylist: keep only characters that are plain FTS5 barewords
    // (alphanumerics, `_`). Every other character is potential query syntax (`=`, `.`,
    // `#`, `+`, `<`, ...); enumerating them one by one kept leaking new ones (#12, #48,
    // #79, #125). The unicode61 tokenizer splits on these anyway, so matching is unchanged.
    let stripped: String = query
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                ' '
            }
        })
        .collect();
    stripped
        .split_whitespace()
        .map(|w| match w {
            "AND" | "OR" | "NOT" | "NEAR" => w.to_lowercase(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Upper bound on distinct terms in one MATCH. With a plain OR-join a long task (an issue
/// title + body) is dominated by common words, so the files the task is really about drop
/// out of the top results (#135: recall 0.00 on 302-file axum with title+body tasks).
pub(crate) const MAX_QUERY_TERMS: usize = 8;

/// Number of (file, table) rows matching `term` across the three FTS tables — a cheap
/// document-frequency proxy. `term` must be a sanitized bareword.
fn term_doc_freq(conn: &Connection, term: &str) -> Result<i64> {
    let mut total = 0;
    for table in ["fts_path", "fts_symbols", "fts_body"] {
        total += conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {table} MATCH ?1"),
            params![term],
            |row| row.get::<_, i64>(0),
        )?;
    }
    Ok(total)
}

/// Reduce a sanitized `a OR b OR ...` query to its `k` rarest terms (lowest document
/// frequency; ties broken by longer term, then alphabetically, so the result is stable).
/// Duplicates are folded case-insensitively and terms that match nothing are dropped.
/// Queries with at most `k` distinct terms are returned unchanged.
pub(crate) fn rarest_terms(conn: &Connection, sanitized: &str, k: usize) -> Result<String> {
    let mut seen = HashSet::new();
    let terms: Vec<&str> = sanitized
        .split(" OR ")
        .filter(|t| seen.insert(t.to_lowercase()))
        .collect();
    if terms.len() <= k {
        return Ok(sanitized.to_string());
    }
    let mut scored: Vec<(i64, &str)> = Vec::with_capacity(terms.len());
    for t in terms {
        let df = term_doc_freq(conn, t)?;
        if df > 0 {
            scored.push((df, t));
        }
    }
    if scored.is_empty() {
        return Ok(sanitized.to_string());
    }
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(b.1.len().cmp(&a.1.len()))
            .then(a.1.cmp(b.1))
    });
    Ok(scored
        .into_iter()
        .take(k)
        .map(|(_, t)| t)
        .collect::<Vec<_>>()
        .join(" OR "))
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
    let sanitized = rarest_terms(conn, &sanitized, MAX_QUERY_TERMS)?;

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
    // `ORDER BY rank` is required for the LIMIT to keep the *best* rows: without it FTS5 returns
    // matches in rowid order and the LIMIT keeps the first rows, not the highest-scoring ones (#135).
    let sub_limit = (top_n * 3) as i64;
    let mut stmt = conn.prepare(
        "SELECT file_id, COALESCE(SUM(score), 0) AS total
         FROM (
             SELECT file_id, (rank / NULLIF(MIN(rank) OVER (), 0)) * ?1 AS score
             FROM (SELECT file_id, rank FROM fts_path    WHERE path    MATCH ?4 ORDER BY rank LIMIT ?5)
             UNION ALL
             SELECT file_id, (rank / NULLIF(MIN(rank) OVER (), 0)) * ?2 AS score
             FROM (SELECT file_id, rank FROM fts_symbols WHERE name    MATCH ?4 ORDER BY rank LIMIT ?5)
             UNION ALL
             SELECT file_id, (rank / NULLIF(MIN(rank) OVER (), 0)) * ?3 AS score
             FROM (SELECT file_id, rank FROM fts_body    WHERE content MATCH ?4 ORDER BY rank LIMIT ?5)
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
    use super::*;
    use crate::index_builder::IndexDb;
    use tempfile::TempDir;

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
    fn sanitize_joins_terms_with_or() {
        assert_eq!(sanitize_fts_query("decoder error"), "decoder OR error");
    }

    #[test]
    fn sanitize_plain_word_unchanged() {
        assert_eq!(sanitize_fts_query("hello").trim(), "hello");
    }

    #[test]
    fn sanitize_downcases_fts5_boolean_keywords() {
        // User-supplied AND/OR/NOT/NEAR must become plain lowercase terms; the only
        // uppercase `OR` left is the joiner between terms.
        let terms = |q: &str| -> Vec<String> {
            sanitize_fts_query(q)
                .split(" OR ")
                .map(str::to_string)
                .collect()
        };
        assert_eq!(
            terms("authentication OR 1=1"),
            ["authentication", "or", "1", "1"]
        );
        assert_eq!(terms("foo AND bar"), ["foo", "and", "bar"]);
        assert_eq!(terms("foo NOT bar"), ["foo", "not", "bar"]);
        assert_eq!(terms("NEAR(foo bar)"), ["near", "foo", "bar"]);
    }

    /// Regression for #125 (and the #12/#48/#79 family): a denylist of operator
    /// characters kept missing new ones (`=`, `.`, ...). Run every printable ASCII
    /// punctuation char — alone and inside a realistic task — through a real FTS5
    /// MATCH so any character FTS5 treats as syntax is caught, not just known ones.
    #[test]
    fn search_bm25_never_errors_on_ascii_punctuation() {
        use super::search_bm25;
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE fts_path USING fts5(file_id UNINDEXED, path);
             CREATE VIRTUAL TABLE fts_symbols USING fts5(file_id UNINDEXED, name);
             CREATE VIRTUAL TABLE fts_body USING fts5(file_id UNINDEXED, content);
             INSERT INTO fts_body (file_id, content) VALUES (1, 'login 1 v0 4 0 auth');",
        )
        .unwrap();

        for c in (0x21u8..=0x7e)
            .map(char::from)
            .filter(|c| !c.is_alphanumeric())
        {
            for task in [
                format!("login {c} auth"),
                format!("v0{c}4{c}0"),
                format!("{c}{c}x{c}"),
            ] {
                if super::sanitize_fts_query(&task).trim().is_empty() {
                    continue; // blank is rejected earlier with a clear message
                }
                search_bm25(&conn, &task, 10)
                    .unwrap_or_else(|e| panic!("task {task:?} (char {c:?}) errored: {e}"));
            }
        }
        // the exact reports
        assert!(search_bm25(&conn, "login 1=1", 10).is_ok());
        assert!(search_bm25(&conn, "feat: add dependency graph (v0.4.0)", 10).is_ok());
    }

    proptest::proptest! {
        /// Whatever the task string is (any Unicode, control chars, operators), the only
        /// permitted failure is the explicit "blank after sanitizing" rejection — never
        /// an FTS5 syntax error. Guards the whole input class, not known characters.
        #[test]
        fn search_bm25_only_fails_on_blank_task(task in proptest::prelude::any::<String>()) {
            let conn = rusqlite::Connection::open_in_memory().unwrap();
            conn.execute_batch(
                "CREATE VIRTUAL TABLE fts_path USING fts5(file_id UNINDEXED, path);
                 CREATE VIRTUAL TABLE fts_symbols USING fts5(file_id UNINDEXED, name);
                 CREATE VIRTUAL TABLE fts_body USING fts5(file_id UNINDEXED, content);
                 INSERT INTO fts_body (file_id, content) VALUES (1, 'login auth v0 4 0');",
            )
            .unwrap();
            let blank = super::sanitize_fts_query(&task).trim().is_empty();
            let res = super::search_bm25(&conn, &task, 10);
            proptest::prop_assert_eq!(res.is_err(), blank, "task {:?}: {:?}", task, res.err());
        }
    }

    fn fts_conn(docs: &[(i64, &str)]) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE fts_path USING fts5(file_id UNINDEXED, path);
             CREATE VIRTUAL TABLE fts_symbols USING fts5(file_id UNINDEXED, name);
             CREATE VIRTUAL TABLE fts_body USING fts5(file_id UNINDEXED, content);",
        )
        .unwrap();
        for (id, text) in docs {
            conn.execute(
                "INSERT INTO fts_body (file_id, content) VALUES (?1, ?2)",
                rusqlite::params![id, text],
            )
            .unwrap();
        }
        conn
    }

    /// #135: a long task is dominated by common words under a plain OR-join. Only the
    /// rarest MAX_QUERY_TERMS terms may be queried, so the file with the rare term ranks.
    #[test]
    fn rarest_terms_keeps_rare_terms_of_a_long_task() {
        use super::{rarest_terms, MAX_QUERY_TERMS};
        let mut docs: Vec<(i64, String)> = (1..=30)
            .map(|i| {
                (
                    i,
                    "the common words appear in every single file here".to_string(),
                )
            })
            .collect();
        docs.push((99, "zanzibar handler".to_string()));
        let refs: Vec<(i64, &str)> = docs.iter().map(|(i, t)| (*i, t.as_str())).collect();
        let conn = fts_conn(&refs);
        let task = "the common words appear in every single file here and zanzibar handler too";
        let kept = rarest_terms(&conn, &super::sanitize_fts_query(task), MAX_QUERY_TERMS).unwrap();
        let n = kept.split(" OR ").count();
        assert!(n <= MAX_QUERY_TERMS, "kept {n} terms: {kept}");
        assert!(kept.contains("zanzibar"), "rare term dropped: {kept}");
        assert!(kept.contains("handler"), "rare term dropped: {kept}");
        let hits = super::search_bm25(&conn, task, 5).unwrap();
        assert_eq!(
            hits.first().map(|h| h.0),
            Some(99),
            "rare-term file must rank first: {hits:?}"
        );
    }

    #[test]
    fn rarest_terms_leaves_short_tasks_unchanged() {
        use super::{rarest_terms, MAX_QUERY_TERMS};
        let conn = fts_conn(&[(1, "alpha beta gamma")]);
        let q = super::sanitize_fts_query("alpha beta gamma");
        assert_eq!(rarest_terms(&conn, &q, MAX_QUERY_TERMS).unwrap(), q);
    }

    #[test]
    fn rarest_terms_is_deterministic_and_dedups() {
        use super::rarest_terms;
        let conn = fts_conn(&[(1, "aa bb cc dd ee ff gg hh ii jj")]);
        let q = super::sanitize_fts_query("aa AA bb cc dd ee ff gg hh ii jj");
        let a = rarest_terms(&conn, &q, 4).unwrap();
        let b = rarest_terms(&conn, &q, 4).unwrap();
        assert_eq!(a, b);
        let lower: Vec<String> = a.split(" OR ").map(str::to_lowercase).collect();
        let uniq: std::collections::HashSet<_> = lower.iter().collect();
        assert_eq!(lower.len(), uniq.len(), "duplicate terms kept: {a}");
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

    // ---- populate_fts_with_paths / search_bm25 ----

    struct Fixture {
        db: IndexDb,
        dir: TempDir,
        rel: HashMap<i64, PathBuf>,
        abs: HashMap<i64, PathBuf>,
    }

    fn count(conn: &Connection, table: &str) -> i64 {
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    /// Files: auth/login.rs (symbol authenticate_user), db/store.rs (symbol open_pool).
    fn fixture() -> Fixture {
        let db = IndexDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let dir = TempDir::new().unwrap();
        let mut rel = HashMap::new();
        let mut abs = HashMap::new();
        let specs = [
            (
                "auth/login.rs",
                "authenticate_user",
                "fn authenticate_user() { check password }",
            ),
            (
                "db/store.rs",
                "open_pool",
                "fn open_pool() { connect database }",
            ),
        ];
        for (path, sym, body) in specs {
            let id: i64 = db
                .upsert_file(std::path::Path::new(path), crate::Language::Rust, "sha", 1)
                .unwrap()
                .0;
            db.connection()
                .execute(
                    "INSERT INTO symbols (file_id, name, kind, line, snippet) VALUES (?1, ?2, 'function', 1, '')",
                    params![id, sym],
                )
                .unwrap();
            let abs_path = dir.path().join(path.replace('/', "_"));
            std::fs::write(&abs_path, body).unwrap();
            rel.insert(id, PathBuf::from(path));
            abs.insert(id, abs_path);
        }
        db.connection()
            .execute(
                "INSERT INTO symbols (file_id, name, kind, line, snippet) VALUES (1, 'use std::io', 'import', 1, '')",
                [],
            )
            .unwrap();
        Fixture { db, dir, rel, abs }
    }

    #[test]
    fn populate_fills_all_three_fts_tables() {
        let f = fixture();
        populate_fts_with_paths(f.db.connection(), &f.rel, &f.abs).unwrap();
        let c = f.db.connection();
        assert_eq!(count(c, "fts_path"), 2);
        // import symbols are excluded
        assert_eq!(count(c, "fts_symbols"), 2);
        assert_eq!(count(c, "fts_body"), 2);
    }

    #[test]
    fn populate_twice_does_not_duplicate_rows() {
        let f = fixture();
        populate_fts_with_paths(f.db.connection(), &f.rel, &f.abs).unwrap();
        populate_fts_with_paths(f.db.connection(), &f.rel, &f.abs).unwrap();
        assert_eq!(count(f.db.connection(), "fts_path"), 2);
        assert_eq!(count(f.db.connection(), "fts_body"), 2);
    }

    #[test]
    fn populate_with_empty_maps_clears_fts() {
        let f = fixture();
        populate_fts_with_paths(f.db.connection(), &f.rel, &f.abs).unwrap();
        populate_fts_with_paths(f.db.connection(), &HashMap::new(), &HashMap::new()).unwrap();
        assert_eq!(count(f.db.connection(), "fts_path"), 0);
        assert_eq!(count(f.db.connection(), "fts_body"), 0);
    }

    #[test]
    fn populate_skips_unreadable_body_but_keeps_rest() {
        let mut f = fixture();
        let id = *f.abs.keys().next().unwrap();
        f.abs.insert(id, f.dir.path().join("does_not_exist.rs"));
        populate_fts_with_paths(f.db.connection(), &f.rel, &f.abs).unwrap();
        assert_eq!(count(f.db.connection(), "fts_body"), 1);
        assert_eq!(count(f.db.connection(), "fts_path"), 2);
    }

    #[test]
    fn populate_truncates_large_body_on_char_boundary() {
        let f = fixture();
        let id = *f.abs.keys().next().unwrap();
        std::fs::write(&f.abs[&id], "あ".repeat(200_000)).unwrap();
        populate_fts_with_paths(f.db.connection(), &f.rel, &f.abs).unwrap();
        let bytes: i64 =
            f.db.connection()
                .query_row(
                    "SELECT length(CAST(content AS BLOB)) FROM fts_body WHERE file_id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .unwrap();
        assert!(bytes <= 512 * 1024 && bytes > 512 * 1024 - 3, "{bytes}");
    }

    #[test]
    fn populate_error_rolls_back_transaction() {
        let f = fixture();
        populate_fts_with_paths(f.db.connection(), &f.rel, &f.abs).unwrap();
        f.db.connection()
            .execute_batch("DROP TABLE fts_symbols")
            .unwrap();
        let res = populate_fts_with_paths(f.db.connection(), &f.rel, &f.abs);
        assert!(res.is_err());
        // Transaction must have been closed and earlier data restored.
        assert!(f.db.connection().is_autocommit());
        assert_eq!(count(f.db.connection(), "fts_path"), 2);
    }

    #[test]
    fn search_finds_file_by_path_term() {
        let f = fixture();
        populate_fts_with_paths(f.db.connection(), &f.rel, &f.abs).unwrap();
        let hits = search_bm25(f.db.connection(), "login", 5).unwrap();
        assert_eq!(hits.len(), 1);
        let login_id = *f
            .rel
            .iter()
            .find(|(_, p)| p.ends_with("login.rs"))
            .unwrap()
            .0;
        assert_eq!(hits[0].0, login_id);
        assert!(hits[0].1 > 0.0);
    }

    #[test]
    fn search_finds_file_by_symbol_and_body_terms() {
        let f = fixture();
        populate_fts_with_paths(f.db.connection(), &f.rel, &f.abs).unwrap();
        let store_id = *f
            .rel
            .iter()
            .find(|(_, p)| p.ends_with("store.rs"))
            .unwrap()
            .0;
        let by_symbol = search_bm25(f.db.connection(), "pool", 5).unwrap();
        assert_eq!(by_symbol[0].0, store_id);
        let by_body = search_bm25(f.db.connection(), "database", 5).unwrap();
        assert_eq!(by_body[0].0, store_id);
    }

    #[test]
    fn search_no_match_returns_empty() {
        let f = fixture();
        populate_fts_with_paths(f.db.connection(), &f.rel, &f.abs).unwrap();
        assert!(search_bm25(f.db.connection(), "zzzzqqqq", 5)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn search_respects_top_n() {
        let f = fixture();
        populate_fts_with_paths(f.db.connection(), &f.rel, &f.abs).unwrap();
        // "fn" appears in both bodies
        let all = search_bm25(f.db.connection(), "fn", 10).unwrap();
        assert_eq!(all.len(), 2);
        let one = search_bm25(f.db.connection(), "fn", 1).unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn search_results_sorted_by_score_descending() {
        let f = fixture();
        populate_fts_with_paths(f.db.connection(), &f.rel, &f.abs).unwrap();
        let hits = search_bm25(f.db.connection(), "fn authenticate", 10).unwrap();
        assert!(hits.windows(2).all(|w| w[0].1 >= w[1].1), "{hits:?}");
    }

    #[test]
    fn search_empty_query_is_error() {
        let f = fixture();
        let err = search_bm25(f.db.connection(), "", 5).unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn search_punctuation_only_query_is_error() {
        let f = fixture();
        assert!(search_bm25(f.db.connection(), "(*) - \"", 5).is_err());
    }

    #[test]
    fn search_with_fts_operators_in_query_does_not_error() {
        let f = fixture();
        populate_fts_with_paths(f.db.connection(), &f.rel, &f.abs).unwrap();
        assert!(search_bm25(f.db.connection(), "login OR (x) AND \"y\" *", 5).is_ok());
    }

    #[test]
    fn search_on_empty_index_returns_empty() {
        let db = IndexDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        assert!(search_bm25(db.connection(), "anything", 3)
            .unwrap()
            .is_empty());
    }
}
