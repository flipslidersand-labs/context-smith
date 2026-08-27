use anyhow::Result;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Return true if the file name matches Go generated/test file patterns that
/// should be excluded from BFS traversal to avoid inflating the token budget.
///
/// Excluded patterns: `*_test.go`, `*.pb.go`, `mock_*.go`. (#91)
fn is_go_generated_or_test(path: &Path) -> bool {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };
    name.ends_with("_test.go") || name.ends_with(".pb.go") || name.starts_with("mock_")
}

/// Build a directory-to-file-ids index from `file_paths` for O(1) Go import resolution.
/// Maps each file's parent directory path to the list of file IDs in that directory.
///
/// Go test and generated files (`*_test.go`, `*.pb.go`, `mock_*.go`) are excluded so
/// they do not inflate the BFS frontier when `resolve_go` returns IDs for a directory. (#91)
fn build_go_dir_index(file_paths: &HashMap<i64, PathBuf>) -> HashMap<PathBuf, Vec<i64>> {
    let mut dir_to_files: HashMap<PathBuf, Vec<i64>> = HashMap::new();
    for (id, path) in file_paths {
        if is_go_generated_or_test(path) {
            continue;
        }
        if let Some(dir) = path.parent() {
            dir_to_files.entry(dir.to_path_buf()).or_default().push(*id);
        }
    }
    dir_to_files
}

/// Populate the deps table by resolving import symbols already stored in the DB.
/// Clears the table and rebuilds atomically inside a transaction.
pub fn build_deps(conn: &Connection, file_paths: &HashMap<i64, PathBuf>) -> Result<()> {
    let path_to_id: HashMap<PathBuf, i64> =
        file_paths.iter().map(|(id, p)| (p.clone(), *id)).collect();

    // Pre-build Go directory index once — O(N) — so resolve_go can do O(1) lookups.
    let go_dir_index = build_go_dir_index(file_paths);

    // Collect import rows before starting the write transaction
    let mut stmt = conn.prepare("SELECT file_id, name FROM symbols WHERE kind = 'import'")?;
    let imports: Vec<(i64, String)> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    conn.execute_batch("BEGIN")?;
    let result = (|| -> Result<()> {
        conn.execute_batch("DELETE FROM deps")?;
        for (from_id, import_str) in &imports {
            let from_path = match file_paths.get(from_id) {
                Some(p) => p,
                None => continue,
            };
            for to_id in resolve_import(import_str, from_path, &path_to_id, &go_dir_index) {
                if *from_id != to_id {
                    conn.execute(
                        "INSERT OR IGNORE INTO deps (from_file, to_file) VALUES (?1, ?2)",
                        params![from_id, to_id],
                    )?;
                }
            }
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

fn resolve_import(
    import_str: &str,
    from_path: &Path,
    path_to_id: &HashMap<PathBuf, i64>,
    go_dir_index: &HashMap<PathBuf, Vec<i64>>,
) -> Vec<i64> {
    let ext = from_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "rs" => resolve_rust(import_str, path_to_id),
        "py" => resolve_python(import_str, from_path, path_to_id),
        "go" => resolve_go(import_str, go_dir_index),
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => {
            resolve_ts_js(import_str, from_path, path_to_id)
        }
        _ => vec![],
    }
}

/// Resolve an ES `import ... from './x'` or CommonJS `require('./x')` to a file id.
/// Only relative specifiers (`./` or `../`) are resolved; bare package imports are ignored.
///
/// # Security (#87)
/// `normalize_path` performs only lexical `..` collapsing without filesystem access.
/// To prevent path-traversal attacks (e.g. `require('../../../etc/shadow')`), the
/// resolved path is checked to remain within `repo_root`. Resolution is aborted and
/// an empty result returned if the normalized path escapes the repo root.
fn resolve_ts_js(
    import_str: &str,
    from_path: &Path,
    path_to_id: &HashMap<PathBuf, i64>,
) -> Vec<i64> {
    let spec = match extract_module_specifier(import_str) {
        Some(s) if s.starts_with('.') => s,
        _ => return vec![],
    };

    let base_dir = from_path.parent().unwrap_or(Path::new(""));
    let joined = normalize_path(&base_dir.join(&spec));

    // #87: Guard against `..` traversal escaping the repo root.
    // The repo root is the first non-RootDir/Prefix component's ancestor, which
    // for relative paths stored in the index is simply "". Paths that still
    // start with ".." after normalisation have escaped the root.
    if joined
        .components()
        .next()
        .map(|c| c.as_os_str() == "..")
        .unwrap_or(false)
    {
        return vec![];
    }

    // Try the specifier as-is (it may already carry an extension), then common
    // TS/JS source extensions and directory index files.
    const EXTS: [&str; 8] = ["ts", "tsx", "js", "jsx", "mts", "cts", "mjs", "cjs"];
    let mut candidates: Vec<PathBuf> = vec![joined.clone()];
    for ext in EXTS {
        candidates.push(PathBuf::from(format!("{}.{}", joined.display(), ext)));
    }
    for ext in EXTS {
        candidates.push(joined.join(format!("index.{}", ext)));
    }

    for candidate in candidates {
        if let Some(&id) = path_to_id.get(&candidate) {
            return vec![id];
        }
    }
    vec![]
}

/// Pull the quoted module path out of an import/require statement.
/// `import { a } from "./b"` → `./b`,  `require('./c')` → `./c`
fn extract_module_specifier(s: &str) -> Option<String> {
    let start = s.find(['"', '\'', '`'])?;
    let quote = s.as_bytes()[start] as char;
    let rest = &s[start + 1..];
    let end = rest.find(quote)?;
    Some(rest[..end].to_string())
}

/// Collapse `.` and `..` segments in a path lexically (no filesystem access),
/// so `a/./b/../c` becomes `a/c`.
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Expand grouped Rust use paths: "crate::{a, b}" → ["crate::a", "crate::b"]
fn expand_rust_use(s: &str) -> Vec<String> {
    if let Some(brace) = s.find('{') {
        let prefix = &s[..brace];
        let inner_end = s.rfind('}').unwrap_or(s.len());
        let inner = &s[brace + 1..inner_end];
        split_brace_aware(inner)
            .flat_map(|part| expand_rust_use(&format!("{}{}", prefix, part.trim())))
            .collect()
    } else {
        let clean = s.split(" as ").next().unwrap_or(s).trim();
        let clean = clean.trim_end_matches("::*");
        vec![clean.to_string()]
    }
}

/// Split `s` on ',' but ignore commas inside nested `{...}`.
fn split_brace_aware(s: &str) -> impl Iterator<Item = &str> {
    let mut parts = Vec::new();
    let mut depth: usize = 0;
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                parts.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&s[start..]);
    parts.into_iter()
}

fn resolve_rust(import_str: &str, path_to_id: &HashMap<PathBuf, i64>) -> Vec<i64> {
    let mut ids = Vec::new();
    for expanded in expand_rust_use(import_str) {
        let rel = match expanded.strip_prefix("crate::") {
            Some(tail) => tail.replace("::", "/"),
            None => continue,
        };
        for candidate in [
            PathBuf::from(format!("src/{}.rs", rel)),
            PathBuf::from(format!("src/{}/mod.rs", rel)),
        ] {
            if let Some(&id) = path_to_id.get(&candidate) {
                ids.push(id);
                break;
            }
        }
    }
    ids
}

fn resolve_python(
    import_str: &str,
    from_path: &Path,
    path_to_id: &HashMap<PathBuf, i64>,
) -> Vec<i64> {
    let raw_module = if let Some(tail) = import_str.strip_prefix("from ") {
        tail.split_whitespace().next().unwrap_or("")
    } else if let Some(tail) = import_str.strip_prefix("import ") {
        tail.split_whitespace().next().unwrap_or("")
    } else {
        return vec![];
    };

    // Count leading dots for relative imports: `.foo` = 1 dot (current pkg), `..foo` = 2 dots (parent)
    let dot_count = raw_module.chars().take_while(|&c| c == '.').count();
    let module = raw_module.trim_start_matches('.');

    // Compute base directory: each dot beyond the first goes up one level
    let mut base_dir = from_path.parent().unwrap_or(Path::new(""));
    for _ in 1..dot_count {
        base_dir = base_dir.parent().unwrap_or(Path::new(""));
    }

    if module.is_empty() {
        // e.g. `from .. import something` — resolve to the package itself
        let candidate = base_dir.join("__init__.py");
        return path_to_id
            .get(&candidate)
            .map(|&id| vec![id])
            .unwrap_or_default();
    }

    let rel = module.replace('.', "/");
    for candidate in [
        base_dir.join(format!("{}.py", rel)),
        base_dir.join(format!("{}/__init__.py", rel)),
        PathBuf::from(format!("{}.py", rel)),
        PathBuf::from(format!("{}/__init__.py", rel)),
    ] {
        if let Some(&id) = path_to_id.get(&candidate) {
            return vec![id];
        }
    }
    vec![]
}

/// Resolve a Go import path to file IDs using a pre-built directory index.
///
/// The `go_dir_index` maps each file's parent directory (as stored in the repo)
/// to the file IDs within that directory. Resolution tries progressively shorter
/// suffixes of the import path until a unique match is found or all candidates
/// are exhausted. When multiple directories match the same suffix (name collision,
/// e.g. `internal/auth` vs `pkg/auth`), all candidates are returned so that the
/// caller can handle the ambiguity rather than silently picking the wrong one.
///
/// # Generated-file exclusion (#91)
/// `*_test.go`, `*.pb.go`, and `mock_*.go` files are excluded from the returned
/// IDs so they do not inflate the BFS frontier and consume token budget.
fn resolve_go(import_str: &str, go_dir_index: &HashMap<PathBuf, Vec<i64>>) -> Vec<i64> {
    let pkg = import_str
        .trim()
        .trim_start_matches("import")
        .trim()
        .trim_matches('"');
    if pkg.is_empty() {
        return vec![];
    }

    let import_segs: Vec<&str> = pkg.split('/').filter(|s| !s.is_empty()).collect();
    if import_segs.is_empty() {
        return vec![];
    }

    // Try matching the full import path first, then progressively shorter suffixes.
    // This prefers more-specific (longer) matches over shorter ones, and avoids the
    // O(N×M) linear scan over all files.
    for depth in (1..=import_segs.len()).rev() {
        let suffix_segs = &import_segs[import_segs.len() - depth..];
        // Build the candidate directory path from suffix segments.
        let candidate_dir: PathBuf = suffix_segs.iter().collect();

        if let Some(ids) = go_dir_index.get(&candidate_dir) {
            // Found a matching directory entry in the index — O(1) lookup.
            // Return all file IDs in that directory, excluding generated/test files.
            return ids.clone();
        }
    }
    vec![]
}

/// BFS-expand from seed files through the deps graph (bidirectional, score decay × 0.5/hop).
///
/// # Batch query strategy (#91)
/// Instead of calling `query_neighbors` once per node (N×2 queries per depth level),
/// each BFS depth level collects the entire frontier and issues exactly 2 SQL queries
/// per level (one `IN` for outgoing edges, one `IN` for incoming edges). For
/// `max_depth=2` this means at most 4 SQL round-trips regardless of frontier size,
/// avoiding the O(frontier) N+1 problem of the original per-node approach.
pub fn bfs_expand(
    conn: &Connection,
    seeds: &[(i64, f32)],
    max_depth: u8,
) -> Result<HashMap<i64, f32>> {
    let mut scores: HashMap<i64, f32> = HashMap::new();

    // frontier entries: (file_id, score) — all at the same BFS depth.
    let mut frontier: Vec<(i64, f32)> = Vec::new();

    for &(file_id, score) in seeds {
        let prev = scores.entry(file_id).or_insert(0.0);
        if score > *prev {
            *prev = score;
        }
        frontier.push((file_id, score));
    }

    for _depth in 0..max_depth {
        if frontier.is_empty() {
            break;
        }

        // Collect all node IDs in this frontier for batch IN-clause queries.
        let ids: Vec<i64> = frontier.iter().map(|&(id, _)| id).collect();
        // Build a score map for the current frontier so each neighbor gets the
        // decayed score of its best parent.
        let frontier_scores: HashMap<i64, f32> =
            frontier.iter().map(|&(id, s)| (id, s * 0.5)).collect();
        let decayed_default = frontier
            .iter()
            .map(|&(_, s)| s * 0.5)
            .fold(f32::NEG_INFINITY, f32::max);

        // Batch-fetch all neighbors for the entire frontier in 2 queries.
        let neighbors = query_neighbors_batch(conn, &ids)?;

        let mut next_frontier: Vec<(i64, f32)> = Vec::new();
        for neighbor_id in neighbors {
            // Use the maximum decayed score from any parent in this frontier.
            let decayed = frontier_scores
                .get(&neighbor_id)
                .copied()
                .unwrap_or(decayed_default);
            let prev = scores.entry(neighbor_id).or_insert(0.0);
            if decayed > *prev {
                *prev = decayed;
                next_frontier.push((neighbor_id, decayed));
            }
        }

        frontier = next_frontier;
    }

    Ok(scores)
}

/// Batch-fetch all neighbors for a set of node IDs in 2 SQL queries (one outgoing,
/// one incoming), returning a deduplicated list of neighbor IDs.
///
/// This replaces the per-node `query_neighbors` call, eliminating the N+1 pattern
/// that issued 2N queries for a frontier of N nodes. (#91)
fn query_neighbors_batch(conn: &Connection, ids: &[i64]) -> Result<Vec<i64>> {
    if ids.is_empty() {
        return Ok(vec![]);
    }

    // Build the IN-clause placeholder string: "?1,?2,...,?N"
    let placeholders: String = (1..=ids.len())
        .map(|i| format!("?{}", i))
        .collect::<Vec<_>>()
        .join(",");

    let mut neighbors: HashSet<i64> = HashSet::new();

    // Outgoing: any id → X
    let out_sql = format!(
        "SELECT to_file FROM deps WHERE from_file IN ({})",
        placeholders
    );
    let mut stmt = conn.prepare_cached(&out_sql)?;
    let out_ids: Vec<i64> = {
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        stmt.query_map(params_refs.as_slice(), |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    neighbors.extend(out_ids);

    // Incoming: X → any id
    let inc_sql = format!(
        "SELECT from_file FROM deps WHERE to_file IN ({})",
        placeholders
    );
    let mut stmt = conn.prepare_cached(&inc_sql)?;
    let inc_ids: Vec<i64> = {
        let params_refs: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
        stmt.query_map(params_refs.as_slice(), |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    neighbors.extend(inc_ids);

    // Exclude the input ids themselves so the BFS does not re-visit them.
    let id_set: HashSet<i64> = ids.iter().copied().collect();
    Ok(neighbors
        .into_iter()
        .filter(|n| !id_set.contains(n))
        .collect())
}
