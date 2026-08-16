use anyhow::Result;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;
use rusqlite::{params, Connection};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

/// Populate the deps table by resolving import symbols already stored in the DB.
/// Clears the table and rebuilds atomically inside a transaction.
pub fn build_deps(conn: &Connection, file_paths: &HashMap<i64, PathBuf>) -> Result<()> {
    let path_to_id: HashMap<PathBuf, i64> =
        file_paths.iter().map(|(id, p)| (p.clone(), *id)).collect();

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
            for to_id in resolve_import(import_str, from_path, &path_to_id) {
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
) -> Vec<i64> {
    let ext = from_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "rs" => resolve_rust(import_str, path_to_id),
        "py" => resolve_python(import_str, from_path, path_to_id),
        "go" => resolve_go(import_str, path_to_id),
        _ => vec![],
    }
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

fn resolve_go(import_str: &str, path_to_id: &HashMap<PathBuf, i64>) -> Vec<i64> {
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

    // Match against the last 2 path segments to avoid false positives from same-named dirs
    let match_depth = import_segs.len().min(2);
    let match_segs = &import_segs[import_segs.len() - match_depth..];

    path_to_id
        .iter()
        .filter(|(p, _)| {
            if let Some(parent) = p.parent() {
                let parent_comps: Vec<String> = parent
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect();
                if parent_comps.len() < match_segs.len() {
                    return false;
                }
                let tail = &parent_comps[parent_comps.len() - match_segs.len()..];
                tail.iter()
                    .zip(match_segs.iter())
                    .all(|(a, b)| a.as_str() == *b)
            } else {
                false
            }
        })
        .map(|(_, &id)| id)
        .collect()
}

/// BFS-expand from seed files through the deps graph (bidirectional, score decay × 0.5/hop).
pub fn bfs_expand(
    conn: &Connection,
    seeds: &[(i64, f32)],
    max_depth: u8,
) -> Result<HashMap<i64, f32>> {
    let edges = load_all_deps(conn)?;

    let mut graph: DiGraph<i64, ()> = DiGraph::new();
    let mut file_to_node: HashMap<i64, NodeIndex> = HashMap::new();

    for &(from, to) in &edges {
        for fid in [from, to] {
            file_to_node
                .entry(fid)
                .or_insert_with(|| graph.add_node(fid));
        }
    }
    for (from, to) in edges {
        let a = file_to_node[&from];
        let b = file_to_node[&to];
        graph.add_edge(a, b, ());
    }

    let mut scores: HashMap<i64, f32> = HashMap::new();
    let mut queue: VecDeque<(NodeIndex, f32, u8)> = VecDeque::new();

    for &(file_id, score) in seeds {
        let prev = scores.entry(file_id).or_insert(0.0);
        if score > *prev {
            *prev = score;
        }
        if let Some(&node) = file_to_node.get(&file_id) {
            queue.push_back((node, score, 0));
        }
    }

    while let Some((node, score, depth)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        let decayed = score * 0.5;
        let neighbors: Vec<NodeIndex> = graph
            .neighbors_directed(node, Direction::Outgoing)
            .chain(graph.neighbors_directed(node, Direction::Incoming))
            .collect();
        for neighbor in neighbors {
            let fid = graph[neighbor];
            let prev = scores.entry(fid).or_insert(0.0);
            if decayed > *prev {
                *prev = decayed;
                queue.push_back((neighbor, decayed, depth + 1));
            }
        }
    }

    Ok(scores)
}

fn load_all_deps(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare("SELECT from_file, to_file FROM deps")?;
    let edges: Vec<(i64, i64)> = stmt
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(edges)
}
