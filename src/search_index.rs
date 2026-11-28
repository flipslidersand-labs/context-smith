use crate::tokenizer::{cjk_grams, is_cjk, split_cjk_runs};
use anyhow::{bail, Result};
use rusqlite::{params, Connection};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

// BM25 signal weights (higher = stronger signal).
// path: file-name/directory match is the strongest localization cue.
// symbols: function/type names match the task vocabulary directly. Kept low (0.5): symbol
//   matches are plentiful and noisy; at the old 2.0 a file with many symbols outranked the best match (#151).
// body: full-text fallback, broadest but noisiest.
const W_PATH: f64 = 3.0;
const W_SYMBOLS: f64 = 0.5;
const W_BODY: f64 = 1.0;
// cjk: bigram table for CJK text (#631). Provisional: same weight as body, not tuned on a
//   CJK benchmark.
const W_CJK: f64 = 1.0;

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
    let mut terms: Vec<String> = Vec::new();
    for w in stripped.split_whitespace() {
        if !w.chars().any(is_cjk) {
            // No CJK: exactly the pre-#631 term, so the query (and ranking) is unchanged.
            terms.push(match w {
                "AND" | "OR" | "NOT" | "NEAR" => w.to_lowercase(),
                other => other.to_string(),
            });
            continue;
        }
        // unicode61 glues a CJK run (and an ASCII identifier written right before it) into
        // one token. Split at script boundaries; for a CJK run search the run as written
        // plus its bigrams, which `fts_cjk` holds (#631).
        for (cjk, seg) in split_cjk_runs(w) {
            if cjk {
                terms.push(seg.to_string());
                terms.extend(cjk_grams(seg).into_iter().filter(|g| g != seg));
            } else if seg.chars().any(char::is_alphanumeric) {
                terms.push(match seg {
                    "AND" | "OR" | "NOT" | "NEAR" => seg.to_lowercase(),
                    other => other.to_string(),
                });
            }
        }
    }
    terms.join(" OR ")
}

/// Whether the (sanitized) query or term holds CJK text, i.e. needs `fts_cjk`.
fn has_cjk(s: &str) -> bool {
    s.chars().any(is_cjk)
}

/// Whether this index has the `fts_cjk` table. An index built before #631 lacks it until
/// re-indexed; querying it must not fail.
fn has_fts_cjk(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'fts_cjk'",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

/// Upper bound on distinct terms in one MATCH. With a plain OR-join a long task (an issue
/// title + body) is dominated by common words, so the files the task is really about drop
/// out of the top results (#135: recall 0.00 on 302-file axum with title+body tasks).
pub(crate) const MAX_QUERY_TERMS: usize = 8;

/// Number of (file, table) rows matching `term` across the FTS tables — a cheap
/// document-frequency proxy. `term` must be a sanitized bareword. `fts_cjk` counts only for
/// a term with CJK text (an ASCII term never matches there) and only if the index has it.
fn term_doc_freq(conn: &Connection, term: &str, with_cjk: bool) -> Result<i64> {
    let mut total = 0;
    let tables: &[&str] = if with_cjk && has_cjk(term) {
        &["fts_path", "fts_symbols", "fts_body", "fts_cjk"]
    } else {
        &["fts_path", "fts_symbols", "fts_body"]
    };
    for table in tables {
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
    let with_cjk = has_cjk(sanitized) && has_fts_cjk(conn);
    let mut scored: Vec<(i64, &str)> = Vec::with_capacity(terms.len());
    for t in terms {
        let df = term_doc_freq(conn, t, with_cjk)?;
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

/// Fourth `UNION ALL` arm of [`search_bm25`]: the CJK bigram table, same shape as `fts_body`.
fn cjk_branch_sql() -> String {
    format!(
        "
             UNION ALL
             SELECT file_id, (rank / NULLIF(MIN(rank) OVER (), 0)) * {W_CJK:?} AS score
             FROM (SELECT file_id, rank FROM fts_cjk     WHERE content MATCH ?4 ORDER BY rank, (SELECT path FROM files WHERE files.id = file_id), file_id LIMIT ?5)"
    )
}

/// True when the (sanitized) task holds CJK text but this index cannot search it: it has no
/// `fts_cjk`, or `fts_cjk` is empty while the index was not stamped with the current
/// `EXTRACT_VERSION` (an `index` run that failed before finishing leaves exactly that).
/// An empty `fts_cjk` in an index of the current version just means no file has CJK text.
fn stale_cjk_note_needed(conn: &Connection, sanitized: &str) -> bool {
    if !has_cjk(sanitized) {
        return false;
    }
    if !has_fts_cjk(conn) {
        return true;
    }
    let empty = conn
        .query_row("SELECT NOT EXISTS (SELECT 1 FROM fts_cjk)", [], |r| {
            r.get::<_, bool>(0)
        })
        .unwrap_or(false);
    if !empty {
        return false;
    }
    let stored: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'extract_version'",
            [],
            |r| r.get(0),
        )
        .ok();
    stored.as_deref() != Some(crate::index_builder::EXTRACT_VERSION.to_string().as_str())
}

/// Print the stale-index note once per process (`select` calls `search_bm25` more than once).
fn warn_stale_cjk_index() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!(
            "note: this index has no CJK (Japanese/Chinese/Korean) search table; \
             re-run 'contextsmith index' to match CJK text"
        );
    }
}

/// Upper bound applied to `top_n` in [`search_bm25`].
const MAX_TOP_N: usize = 1000;

/// BM25 search across fts_path (×3), fts_symbols (×0.5, best match per file) and fts_body (×1).
///
/// Each sub-table's raw BM25 `rank` is normalized to [0, 1] via `rank / MIN(rank)` (BM25 `rank` is negative, so MIN is the best match) so
/// that the absolute magnitude difference between small path tables and large body tables
/// does not drown out the configured weights.  Each sub-query is also limited to
/// `top_n * 3` rows before the UNION ALL to prevent intermediate memory explosion.
///
/// Returns an empty Vec when the sanitized query is blank (nothing to match).
pub fn search_bm25(conn: &Connection, query: &str, top_n: usize) -> Result<Vec<(i64, f32)>> {
    let (rows, note) = search_bm25_noted(conn, query, top_n)?;
    if note {
        warn_stale_cjk_index();
    }
    Ok(rows)
}

/// [`search_bm25`] plus whether the task needs CJK search this index cannot do (see
/// [`stale_cjk_note_needed`]), so the decision can be tested without capturing stderr.
fn search_bm25_noted(
    conn: &Connection,
    query: &str,
    top_n: usize,
) -> Result<(Vec<(i64, f32)>, bool)> {
    let sanitized = sanitize_fts_query(query);
    // Guard: an empty MATCH expression is a SQLite FTS5 syntax error.
    if sanitized.is_empty() {
        bail!(
            "task string is empty or contains only punctuation — please provide at least one word"
        );
    }
    // Decided on the full task: `rarest_terms` drops CJK terms an old index cannot match, which
    // would otherwise hide the note for a long mixed task.
    let note = stale_cjk_note_needed(conn, &sanitized);
    let sanitized = rarest_terms(conn, &sanitized, MAX_QUERY_TERMS)?;
    // The bigram table joins in only for a CJK task on an index that has it. Any other task
    // runs the same three-table SQL as before #631.
    let cjk_branch = if has_cjk(&sanitized) && has_fts_cjk(conn) {
        cjk_branch_sql()
    } else {
        String::new()
    };

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
    // fts_symbols has one row per symbol while fts_path / fts_body have one row per file, so its
    // rows are collapsed to the best match per file (`MIN(rank)`); summing them let a file with many
    // symbols outrank the file that matches the task best (#151).
    // `ORDER BY rank` is required for the LIMIT to keep the *best* rows: without it FTS5 returns
    // matches in rowid order and the LIMIT keeps the first rows, not the highest-scoring ones (#135).
    // Clamp so `top_n * 3` and the `as i64` casts can never overflow/wrap (a negative
    // SQLite LIMIT means unlimited, defeating the sub-query cap).
    let top_n = top_n.min(MAX_TOP_N);
    let sub_limit = (top_n * 3) as i64;
    let sql = format!(
        "SELECT file_id, COALESCE(SUM(score), 0) AS total
         FROM (
             SELECT file_id, (rank / NULLIF(MIN(rank) OVER (), 0)) * ?1 AS score
             FROM (SELECT file_id, rank FROM fts_path    WHERE path    MATCH ?4 ORDER BY rank, (SELECT path FROM files WHERE files.id = file_id), file_id LIMIT ?5)
             UNION ALL
             SELECT file_id, (rank / NULLIF(MIN(rank) OVER (), 0)) * ?2 AS score
             FROM (SELECT file_id, MIN(rank) AS rank FROM fts_symbols WHERE name    MATCH ?4 GROUP BY file_id ORDER BY rank, (SELECT path FROM files WHERE files.id = file_id), file_id LIMIT ?5)
             UNION ALL
             SELECT file_id, (rank / NULLIF(MIN(rank) OVER (), 0)) * ?3 AS score
             FROM (SELECT file_id, rank FROM fts_body    WHERE content MATCH ?4 ORDER BY rank, (SELECT path FROM files WHERE files.id = file_id), file_id LIMIT ?5){cjk_branch}
         )
         GROUP BY file_id
         ORDER BY total DESC, (SELECT path FROM files WHERE files.id = file_id), file_id
         LIMIT ?6"
    );
    let mut stmt = conn.prepare(&sql)?;
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
    Ok((rows, note))
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
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT);
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
                 CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT);
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
             CREATE VIRTUAL TABLE fts_body USING fts5(file_id UNINDEXED, content);
             CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT);",
        )
        .unwrap();
        for (id, text) in docs {
            conn.execute(
                "INSERT INTO files (id, path) VALUES (?1, ?2)",
                rusqlite::params![id, format!("f{id:02}.rs")],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO fts_body (file_id, content) VALUES (?1, ?2)",
                rusqlite::params![id, text],
            )
            .unwrap();
        }
        conn
    }

    fn fts_conn_cjk(docs: &[(i64, &str)]) -> rusqlite::Connection {
        let conn = fts_conn(docs);
        conn.execute_batch("CREATE VIRTUAL TABLE fts_cjk USING fts5(file_id UNINDEXED, content);")
            .unwrap();
        for (id, text) in docs {
            let grams = crate::tokenizer::cjk_index_text(text);
            if !grams.is_empty() {
                conn.execute(
                    "INSERT INTO fts_cjk (file_id, content) VALUES (?1, ?2)",
                    rusqlite::params![id, grams],
                )
                .unwrap();
            }
        }
        conn
    }

    #[test]
    fn sanitize_ascii_query_is_unchanged_by_cjk_support() {
        assert_eq!(
            sanitize_fts_query("AND foo_bar ＡＢＣ"),
            "and OR foo_bar OR ＡＢＣ"
        );
        assert_eq!(sanitize_fts_query("ＡＢＣ"), "ＡＢＣ");
    }

    #[test]
    fn sanitize_splits_ascii_identifier_from_following_japanese() {
        assert_eq!(
            sanitize_fts_query("auth_userの修正"),
            "auth_user OR の修正 OR の修 OR 修正"
        );
        assert_eq!(sanitize_fts_query("AND修正"), "and OR 修正");
    }

    #[test]
    fn sanitize_cjk_run_lengths() {
        assert_eq!(sanitize_fts_query("の"), "の");
        assert_eq!(sanitize_fts_query("修正"), "修正"); // 2 chars: the bigram is the run
        assert_eq!(sanitize_fts_query("ー"), "ー");
        assert_eq!(sanitize_fts_query("エラー"), "エラー OR エラ OR ラー");
        assert_eq!(sanitize_fts_query("、。「」"), "");
    }

    #[test]
    fn sanitize_cjk_mixed_with_underscore_and_other_scripts() {
        assert_eq!(
            sanitize_fts_query("認証_エラー"),
            "認証 OR エラー OR エラ OR ラー"
        );
        assert_eq!(
            sanitize_fts_query("인증오류"),
            "인증오류 OR 인증 OR 증오 OR 오류"
        );
        assert_eq!(sanitize_fts_query("ｴﾗｰ"), "ｴﾗｰ OR ｴﾗ OR ﾗｰ");
    }

    #[test]
    fn cjk_task_matches_through_fts_cjk() {
        let conn = fts_conn_cjk(&[
            (1, "fn auth_user() {}"),
            (2, "// 認証エラーを修正する"),
            (3, "unrelated"),
        ]);
        let ids = |q: &str| -> Vec<i64> {
            search_bm25(&conn, q, 10)
                .unwrap()
                .into_iter()
                .map(|(i, _)| i)
                .collect()
        };
        assert_eq!(ids("auth_userの修正"), [1, 2]);
        assert_eq!(ids("エラー"), [2]);
        assert_eq!(ids("修正する"), [2]);
        assert_eq!(ids("認証_エラー"), [2]);
    }

    /// An index built before `fts_cjk` existed must still answer, ASCII part included.
    #[test]
    fn cjk_task_on_index_without_fts_cjk_still_runs() {
        let conn = fts_conn(&[(1, "fn auth_user() {}"), (2, "// 認証エラーを修正する")]);
        let hits = search_bm25(&conn, "auth_userの修正", 10).unwrap();
        assert_eq!(hits.iter().map(|h| h.0).collect::<Vec<_>>(), [1]);
        // more than MAX_QUERY_TERMS terms goes through the document-frequency lookup too
        assert!(search_bm25(&conn, "auth_user 認証エラーを修正する必要がある", 10).is_ok());
    }

    fn note_for(conn: &rusqlite::Connection, task: &str) -> bool {
        search_bm25_noted(conn, task, 10).unwrap().1
    }

    /// The note is decided on the whole task: a long mixed task is cut to its rarest terms,
    /// which on an old index leaves only the ASCII ones.
    #[test]
    fn stale_note_is_needed_for_a_long_mixed_task_on_an_index_without_fts_cjk() {
        let conn = fts_conn(&[(1, "fn auth_user() {}")]);
        let long = "auth_user 認証エラーを修正する";
        assert!(sanitize_fts_query(long).split(" OR ").count() > MAX_QUERY_TERMS);
        assert!(note_for(&conn, long));
        assert!(note_for(&conn, "修正"));
        assert!(!note_for(&conn, "auth_user"));
    }

    #[test]
    fn stale_note_only_when_fts_cjk_cannot_serve_the_task() {
        let version = crate::index_builder::EXTRACT_VERSION.to_string();
        // table with rows: fine
        let conn = fts_conn_cjk(&[(1, "// 認証エラー")]);
        assert!(!note_for(&conn, "認証"));
        // empty table, current version (no file has CJK text): fine
        let conn = fts_conn_cjk(&[(1, "fn a() {}")]);
        conn.execute_batch("CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('extract_version', ?1)",
            [&version],
        )
        .unwrap();
        assert!(!note_for(&conn, "認証"));
        // empty table, version never stamped or older (an index run that failed): note
        conn.execute("UPDATE meta SET value = '1'", []).unwrap();
        assert!(note_for(&conn, "認証"));
        conn.execute("DELETE FROM meta", []).unwrap();
        assert!(note_for(&conn, "認証"));
    }

    proptest::proptest! {
        /// Same guarantee as above with the CJK table present.
        #[test]
        fn search_bm25_with_fts_cjk_only_fails_on_blank_task(
            task in "[ -~あ-んア-ン一-龥ｱ-ﾝ가-힣ーＡ、。_]{0,40}"
        ) {
            let conn = fts_conn_cjk(&[(1, "login auth 認証エラー")]);
            let blank = super::sanitize_fts_query(&task).trim().is_empty();
            let res = super::search_bm25(&conn, &task, 10);
            proptest::prop_assert_eq!(res.is_err(), blank, "task {:?}: {:?}", task, res.err());
        }
    }

    /// #311: files that tie on score must be cut at the limit by `path`, not by `file_id`
    /// (which changes on re-index) or SQLite's internal order. The same files with their ids
    /// assigned in the opposite order (a re-index) must yield the same candidate set.
    #[test]
    fn ties_are_cut_by_path_at_the_limit() {
        let paths_for = |reverse: bool| -> Vec<String> {
            let docs: Vec<(i64, &str)> = (1..=20).rev().map(|i| (i, "tiebreak sameword")).collect();
            let conn = fts_conn(&docs);
            for id in 1..=20i64 {
                let n = if reverse { 21 - id } else { id };
                conn.execute(
                    "UPDATE files SET path = ?1 WHERE id = ?2",
                    rusqlite::params![format!("p{n:02}.rs"), id],
                )
                .unwrap();
            }
            let hits = super::search_bm25(&conn, "sameword", 2).unwrap();
            hits.iter()
                .map(|h| {
                    conn.query_row("SELECT path FROM files WHERE id = ?1", [h.0], |r| r.get(0))
                        .unwrap()
                })
                .collect()
        };
        let expected = vec!["p01.rs".to_string(), "p02.rs".to_string()];
        assert_eq!(paths_for(false), expected);
        assert_eq!(paths_for(true), expected);
    }

    /// Ties must be ordered by path, not by `file_id` (which changes on re-index).
    #[test]
    fn ties_are_ordered_by_path_not_file_id() {
        let conn = fts_conn(&[(1, "sameword"), (2, "sameword"), (3, "sameword")]);
        conn.execute_batch(
            "UPDATE files SET path = 'c.rs' WHERE id = 1;
             UPDATE files SET path = 'b.rs' WHERE id = 2;
             UPDATE files SET path = 'a.rs' WHERE id = 3;",
        )
        .unwrap();
        let hits = super::search_bm25(&conn, "sameword", 3).unwrap();
        let ids: Vec<i64> = hits.iter().map(|h| h.0).collect();
        assert_eq!(ids, vec![3, 2, 1], "{hits:?}");
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

    /// #135: `fts_symbols` holds one row per symbol, so summing per-row scores per file made a
    /// file with many symbols that merely contain one common word outrank the file whose single
    /// symbol matches the task's specific terms. Scores must be aggregated per file.
    #[test]
    fn many_symbol_rows_do_not_outrank_the_better_matching_file() {
        let conn = fts_conn(&[]);
        // 100 unrelated files so that "handler" is moderately rare (a word present in every
        // document has ~zero BM25 weight and would never inflate a sum)
        for id in 10..110 {
            conn.execute(
                "INSERT INTO fts_symbols (file_id, name) VALUES (?1, 'unrelated')",
                rusqlite::params![id],
            )
            .unwrap();
        }
        // file 1: 40 symbols, each containing the word "handler"
        for i in 0..40 {
            conn.execute(
                "INSERT INTO fts_symbols (file_id, name) VALUES (1, ?1)",
                rusqlite::params![format!("handler{i} handler")],
            )
            .unwrap();
        }
        // file 2: one symbol that matches both the specific and the common term
        conn.execute(
            "INSERT INTO fts_symbols (file_id, name) VALUES (2, 'zanzibar handler')",
            [],
        )
        .unwrap();
        let hits = super::search_bm25(&conn, "zanzibar handler", 5).unwrap();
        assert_eq!(
            hits.first().map(|h| h.0),
            Some(2),
            "file with one precise symbol match must rank first: {hits:?}"
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

    // ---- search_bm25 over an index built by the production `build_index` ----

    struct Fixture {
        db: IndexDb,
        _repo_dir: TempDir,
        login_id: i64,
        store_id: i64,
    }

    /// Indexes a real git repo through `build_index` so FTS rows get the same rowids and
    /// tokenization as production. Files: auth/login.rs (symbol authenticate_user),
    /// db/store.rs (symbol open_pool).
    fn fixture() -> Fixture {
        let dir = TempDir::new().unwrap();
        crate::test_util::commit_files(
            dir.path(),
            &[
                (
                    "auth/login.rs",
                    "fn authenticate_user() { check password }\n",
                ),
                ("db/store.rs", "fn open_pool() { connect database }\n"),
            ],
            &[],
        );
        let repo = crate::GitRepo::new(dir.path()).unwrap();
        let db = IndexDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::index_builder::build_index(&repo, &db, false).unwrap();
        let id_of = |path: &str| -> i64 {
            db.connection()
                .query_row("SELECT id FROM files WHERE path = ?1", [path], |r| r.get(0))
                .unwrap()
        };
        let (login_id, store_id) = (id_of("auth/login.rs"), id_of("db/store.rs"));
        Fixture {
            db,
            _repo_dir: dir,
            login_id,
            store_id,
        }
    }

    #[test]
    fn search_finds_file_by_path_term() {
        let f = fixture();
        let hits = search_bm25(f.db.connection(), "login", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, f.login_id);
        assert!(hits[0].1 > 0.0);
    }

    #[test]
    fn search_finds_file_by_symbol_and_body_terms() {
        let f = fixture();
        let by_symbol = search_bm25(f.db.connection(), "pool", 5).unwrap();
        assert_eq!(by_symbol[0].0, f.store_id);
        let by_body = search_bm25(f.db.connection(), "database", 5).unwrap();
        assert_eq!(by_body[0].0, f.store_id);
    }

    #[test]
    fn search_no_match_returns_empty() {
        let f = fixture();
        assert!(search_bm25(f.db.connection(), "zzzzqqqq", 5)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn search_respects_top_n() {
        let f = fixture();
        // "fn" appears in both bodies
        let all = search_bm25(f.db.connection(), "fn", 10).unwrap();
        assert_eq!(all.len(), 2);
        let one = search_bm25(f.db.connection(), "fn", 1).unwrap();
        assert_eq!(one.len(), 1);
    }

    #[test]
    fn search_huge_top_n_is_clamped_not_overflowing() {
        let f = fixture();
        for n in [usize::MAX / 2, usize::MAX] {
            let hits = search_bm25(f.db.connection(), "fn", n).unwrap();
            assert_eq!(hits.len(), 2);
        }
    }

    #[test]
    fn search_results_sorted_by_score_descending() {
        let f = fixture();
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
