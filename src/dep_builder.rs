use anyhow::Result;
use petgraph::graph::{DiGraph, NodeIndex};
use petgraph::Direction;
use rusqlite::{params, Connection};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

/// Populate the deps table by resolving import symbols already stored in the DB.
pub fn build_deps(conn: &Connection, file_paths: &HashMap<i64, PathBuf>) -> Result<()> {
    let path_to_id: HashMap<PathBuf, i64> =
        file_paths.iter().map(|(id, p)| (p.clone(), *id)).collect();

    let mut stmt = conn.prepare("SELECT file_id, name FROM symbols WHERE kind = 'import'")?;
    let imports: Vec<(i64, String)> = stmt
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?
        .filter_map(|r| r.ok())
        .collect();

    for (from_id, import_str) in imports {
        let from_path = match file_paths.get(&from_id) {
            Some(p) => p,
            None => continue,
        };
        for to_id in resolve_import(&import_str, from_path, &path_to_id) {
            if from_id != to_id {
                conn.execute(
                    "INSERT OR IGNORE INTO deps (from_file, to_file) VALUES (?1, ?2)",
                    params![from_id, to_id],
                )?;
            }
        }
    }
    Ok(())
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
        inner
            .split(',')
            .flat_map(|part| expand_rust_use(&format!("{}{}", prefix, part.trim())))
            .collect()
    } else {
        let clean = s.split(" as ").next().unwrap_or(s).trim();
        let clean = clean.trim_end_matches("::*");
        vec![clean.to_string()]
    }
}

fn resolve_rust(import_str: &str, path_to_id: &HashMap<PathBuf, i64>) -> Vec<i64> {
    let mut ids = Vec::new();
    for expanded in expand_rust_use(import_str) {
        let rel = match expanded.strip_prefix("crate::") {
            Some(tail) => tail.replace("::", "/"),
            None => continue, // external crate or super:: — skip
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
    let module = if let Some(tail) = import_str.strip_prefix("from ") {
        tail.split_whitespace().next().unwrap_or("").trim_start_matches('.')
    } else if let Some(tail) = import_str.strip_prefix("import ") {
        tail.split_whitespace().next().unwrap_or("")
    } else {
        return vec![];
    };

    let base_dir = from_path.parent().unwrap_or(Path::new(""));
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
    // "import "github.com/org/repo/pkg/foo"" → match files whose parent dir = "foo"
    let pkg = import_str
        .trim()
        .trim_start_matches("import")
        .trim()
        .trim_matches('"');
    let last = pkg.rsplit('/').next().unwrap_or(pkg);
    path_to_id
        .iter()
        .filter(|(p, _)| {
            p.parent()
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                == Some(last)
        })
        .map(|(_, &id)| id)
        .collect()
}

/// BFS-expand from seed files through the deps graph (bidirectional, score decay × 0.5/hop).
/// Returns file_id → score for all reachable files including seeds.
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
            file_to_node.entry(fid).or_insert_with(|| graph.add_node(fid));
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
        .filter_map(|r| r.ok())
        .collect();
    Ok(edges)
}
