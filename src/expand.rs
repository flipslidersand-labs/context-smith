//! Dependency-graph expansion strategies, selected by `CONTEXTSMITH_EXPAND` (#293).
//!
//! `bfs-capped` (default since the #293 bench) caps each file's neighbours so hub files do not
//! collect score. `bfs` is the earlier plain [`bfs_expand`]; `ppr` is experimental.

use crate::dep_builder::bfs_expand;
use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};

/// Environment variable that selects the expansion mode.
pub const EXPAND_ENV: &str = "CONTEXTSMITH_EXPAND";

const CAP_K: usize = 10;
const PPR_ALPHA: f32 = 0.85;
const PPR_MAX_ITER: usize = 50;
const PPR_TOL: f32 = 1e-6;
const CHUNK: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandMode {
    Bfs,
    BfsCapped,
    Ppr,
}

/// Parse a mode name; `None` (unset) means the default `bfs-capped`.
pub fn parse_mode(v: Option<&str>) -> Result<ExpandMode> {
    match v {
        Some("bfs") => Ok(ExpandMode::Bfs),
        None | Some("bfs-capped") => Ok(ExpandMode::BfsCapped),
        Some("ppr") => Ok(ExpandMode::Ppr),
        Some(other) => {
            anyhow::bail!("unknown {EXPAND_ENV} value '{other}' (expected bfs, bfs-capped or ppr)")
        }
    }
}

/// Thin env reader; all logic lives in [`parse_mode`] so tests need not touch the environment.
pub fn mode_from_env() -> Result<ExpandMode> {
    match std::env::var(EXPAND_ENV) {
        Ok(v) => parse_mode(Some(&v)),
        Err(std::env::VarError::NotPresent) => parse_mode(None),
        Err(e) => anyhow::bail!("invalid {EXPAND_ENV}: {e}"),
    }
}

pub fn expand(
    conn: &Connection,
    mode: ExpandMode,
    seeds: &[(i64, f32)],
    depth: u8,
) -> Result<HashMap<i64, f32>> {
    match mode {
        ExpandMode::Bfs => bfs_expand(conn, seeds, depth),
        ExpandMode::BfsCapped => bfs_capped(conn, seeds, depth),
        ExpandMode::Ppr => ppr(conn, seeds, depth),
    }
}

fn placeholders(n: usize) -> String {
    (1..=n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Undirected, de-duplicated neighbour lists (self-loops removed) for `ids`.
fn neighbors(conn: &Connection, ids: &[i64]) -> Result<HashMap<i64, Vec<i64>>> {
    let mut sets: HashMap<i64, HashSet<i64>> = HashMap::new();
    for chunk in ids.chunks(CHUNK) {
        let ph = placeholders(chunk.len());
        for sql in [
            format!("SELECT from_file, to_file FROM deps WHERE from_file IN ({ph}) AND from_file != to_file"),
            format!("SELECT to_file, from_file FROM deps WHERE to_file IN ({ph}) AND from_file != to_file"),
        ] {
            let mut stmt = conn.prepare_cached(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
            let rows = stmt.query_map(params.as_slice(), |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?;
            for row in rows {
                let (a, b) = row?;
                sets.entry(a).or_default().insert(b);
            }
        }
    }
    Ok(sets
        .into_iter()
        .map(|(k, v)| {
            let mut v: Vec<i64> = v.into_iter().collect();
            v.sort_unstable();
            (k, v)
        })
        .collect())
}

/// Undirected degree (distinct neighbours, no self-loop) of each id, one SQL per chunk.
fn degrees(conn: &Connection, ids: &[i64]) -> Result<HashMap<i64, usize>> {
    let mut out = HashMap::new();
    for chunk in ids.chunks(CHUNK) {
        let ph = placeholders(chunk.len());
        let sql = format!(
            "SELECT id, COUNT(*) FROM (
                SELECT from_file AS id, to_file AS o FROM deps
                    WHERE from_file IN ({ph}) AND from_file != to_file
                UNION
                SELECT to_file AS id, from_file AS o FROM deps
                    WHERE to_file IN ({ph}) AND from_file != to_file
             ) GROUP BY id"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> =
            chunk.iter().map(|i| i as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(params.as_slice(), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (id, d) = row?;
            out.insert(id, d as usize);
        }
    }
    Ok(out)
}

fn dedup_seeds(seeds: &[(i64, f32)]) -> HashMap<i64, f32> {
    let mut m: HashMap<i64, f32> = HashMap::new();
    for &(id, s) in seeds {
        let e = m.entry(id).or_insert(0.0);
        if s > *e {
            *e = s;
        }
    }
    m
}

/// BFS where each parent expands to at most `CAP_K` neighbours (lowest degree first, then id),
/// and a neighbour's score is `parent * 0.5 / sqrt(max(1, deg(neighbour)))`.
pub fn bfs_capped(
    conn: &Connection,
    seeds: &[(i64, f32)],
    max_depth: u8,
) -> Result<HashMap<i64, f32>> {
    let mut scores = dedup_seeds(seeds);
    let mut frontier: Vec<(i64, f32)> = scores.iter().map(|(&i, &s)| (i, s)).collect();
    for _ in 0..max_depth {
        if frontier.is_empty() {
            break;
        }
        let ids: Vec<i64> = frontier.iter().map(|&(i, _)| i).collect();
        let nbrs = neighbors(conn, &ids)?;
        let mut all: Vec<i64> = nbrs.values().flatten().copied().collect();
        all.sort_unstable();
        all.dedup();
        let deg = degrees(conn, &all)?;
        // Like `bfs_expand`, edges inside the current frontier are not expanded (a seed is
        // never re-scored from a neighbouring seed) and do not use up a parent's K slots.
        let in_frontier: std::collections::HashSet<i64> = ids.iter().copied().collect();
        let mut best: HashMap<i64, f32> = HashMap::new();
        for &(parent, ps) in &frontier {
            let Some(list) = nbrs.get(&parent) else {
                continue;
            };
            let mut picked: Vec<(usize, i64)> = list
                .iter()
                .filter(|n| !in_frontier.contains(n))
                .map(|&n| (deg.get(&n).copied().unwrap_or(1), n))
                .collect();
            picked.sort_unstable();
            picked.truncate(CAP_K);
            for (d, n) in picked {
                let decayed = ps * 0.5 / (d.max(1) as f32).sqrt();
                let e = best.entry(n).or_insert(f32::NEG_INFINITY);
                if decayed > *e {
                    *e = decayed;
                }
            }
        }
        let mut next = Vec::new();
        for (n, decayed) in best {
            let prev = scores.entry(n).or_insert(0.0);
            if decayed > *prev {
                *prev = decayed;
                next.push((n, decayed));
            }
        }
        frontier = next;
    }
    Ok(scores)
}

/// Personalized PageRank over the undirected deps graph, restricted to the plain-BFS candidate
/// set (so only the ranking changes). Non-seeds score `0.5 * max_seed * ppr / max_nonseed_ppr`.
pub fn ppr(conn: &Connection, seeds: &[(i64, f32)], depth: u8) -> Result<HashMap<i64, f32>> {
    let seed_scores = dedup_seeds(seeds);
    let cand = bfs_expand(conn, seeds, depth)?;
    let mut nodes: Vec<i64> = cand.keys().copied().collect();
    nodes.sort_unstable();
    let idx: HashMap<i64, usize> = nodes.iter().enumerate().map(|(i, &n)| (n, i)).collect();
    let n = nodes.len();

    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (node, list) in neighbors(conn, &nodes)? {
        adj[idx[&node]] = list.iter().filter_map(|m| idx.get(m).copied()).collect();
    }

    // Personalization: normalised seed scores (uniform over seeds if none is positive).
    let mut p = vec![0.0f32; n];
    let pos_sum: f32 = seed_scores
        .values()
        .filter(|s| s.is_finite() && **s > 0.0)
        .sum();
    for (&id, &s) in &seed_scores {
        p[idx[&id]] = if pos_sum > 0.0 {
            if s.is_finite() && s > 0.0 {
                s / pos_sum
            } else {
                0.0
            }
        } else {
            1.0 / seed_scores.len() as f32
        };
    }

    let mut r = p.clone();
    for _ in 0..PPR_MAX_ITER {
        let mut next = vec![0.0f32; n];
        let mut dangling = 0.0f32;
        for i in 0..n {
            if adj[i].is_empty() {
                dangling += r[i];
            } else {
                let share = r[i] / adj[i].len() as f32;
                for &j in &adj[i] {
                    next[j] += share;
                }
            }
        }
        let mut diff = 0.0f32;
        for i in 0..n {
            next[i] = PPR_ALPHA * (next[i] + dangling * p[i]) + (1.0 - PPR_ALPHA) * p[i];
            diff += (next[i] - r[i]).abs();
        }
        r = next;
        if diff < PPR_TOL {
            break;
        }
    }

    let max_seed = seed_scores.values().copied().fold(0.0f32, f32::max);
    let max_non = nodes
        .iter()
        .filter(|id| !seed_scores.contains_key(id))
        .map(|id| r[idx[id]])
        .fold(0.0f32, f32::max);
    let mut out = HashMap::new();
    for &id in &nodes {
        if let Some(&s) = seed_scores.get(&id) {
            out.insert(id, s);
        } else if max_non > 0.0 {
            out.insert(id, 0.5 * max_seed * r[idx[&id]] / max_non);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(edges: &[(i64, i64)]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE deps (from_file INTEGER NOT NULL, to_file INTEGER NOT NULL);",
        )
        .unwrap();
        for (a, b) in edges {
            conn.execute("INSERT INTO deps VALUES (?1, ?2)", [a, b])
                .unwrap();
        }
        conn
    }

    fn keys(m: &HashMap<i64, f32>) -> Vec<i64> {
        let mut k: Vec<i64> = m.keys().copied().collect();
        k.sort_unstable();
        k
    }

    #[test]
    fn parse_mode_values() {
        assert_eq!(parse_mode(None).unwrap(), ExpandMode::BfsCapped);
        assert_eq!(parse_mode(Some("bfs")).unwrap(), ExpandMode::Bfs);
        assert_eq!(
            parse_mode(Some("bfs-capped")).unwrap(),
            ExpandMode::BfsCapped
        );
        assert_eq!(parse_mode(Some("ppr")).unwrap(), ExpandMode::Ppr);
        let e = parse_mode(Some("nope")).unwrap_err().to_string();
        assert!(e.contains("nope") && e.contains(EXPAND_ENV), "{e}");
    }

    #[test]
    fn unset_default_equals_bfs_capped() {
        let conn = graph(&[(1, 2), (2, 3), (4, 1), (5, 5)]);
        let seeds = [(1, 10.0), (3, 2.0)];
        let mode = parse_mode(None).unwrap();
        assert_eq!(
            expand(&conn, mode, &seeds, 2).unwrap(),
            bfs_capped(&conn, &seeds, 2).unwrap()
        );
        // `bfs` stays selectable and is the plain BFS.
        assert_eq!(
            expand(&conn, parse_mode(Some("bfs")).unwrap(), &seeds, 2).unwrap(),
            bfs_expand(&conn, &seeds, 2).unwrap()
        );
    }

    /// Seed 1 links to hub 100 (many other links) and to leaf 2.
    fn hub_graph() -> Connection {
        let mut e = vec![(1, 100), (1, 2)];
        for i in 200..230 {
            e.push((i, 100));
        }
        graph(&e)
    }

    #[test]
    fn capped_prefers_low_degree_and_suppresses_hub() {
        let conn = hub_graph();
        let s = bfs_capped(&conn, &[(1, 8.0)], 1).unwrap();
        assert!((s[&2] - 4.0).abs() < 1e-6); // deg 1
        assert!(s[&100] < s[&2]); // deg 31 -> 4/sqrt(31)
        assert!((s[&100] - 4.0 / 31f32.sqrt()).abs() < 1e-5);
    }

    #[test]
    fn capped_limits_to_k_lowest_degree_ties_by_id() {
        let e: Vec<(i64, i64)> = (2..=16).map(|i| (1, i)).collect(); // 15 leaves
        let conn = graph(&e);
        let s = bfs_capped(&conn, &[(1, 1.0)], 1).unwrap();
        assert_eq!(keys(&s), vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
    }

    #[test]
    fn capped_candidates_subset_of_bfs() {
        let conn = hub_graph();
        let b = bfs_expand(&conn, &[(1, 8.0)], 2).unwrap();
        let c = bfs_capped(&conn, &[(1, 8.0)], 2).unwrap();
        assert!(c.keys().all(|k| b.contains_key(k)));
    }

    #[test]
    fn capped_does_not_rescore_adjacent_seeds() {
        // Seeds 1 (8.0) and 2 (1.0) are adjacent; bfs keeps seed 2 at 1.0, so must capped.
        let conn = graph(&[(1, 2), (2, 3)]);
        let seeds = [(1, 8.0), (2, 1.0)];
        let b = bfs_expand(&conn, &seeds, 2).unwrap();
        let c = bfs_capped(&conn, &seeds, 2).unwrap();
        assert_eq!(b[&2], 1.0);
        assert_eq!(c[&2], 1.0);
    }

    /// Hub 0 plus leaves 1..=n; edge directions alternate, and consecutive leaves are chained so
    /// that neighbours straddle SQL chunk boundaries. A self-loop on 1 must be ignored.
    fn star_chain(n: i64) -> Connection {
        let mut e = vec![(1, 1)];
        for i in 1..=n {
            e.push(if i % 2 == 0 { (i, 0) } else { (0, i) });
            if i < n {
                e.push((i, i + 1));
            }
        }
        graph(&e)
    }

    #[test]
    fn neighbors_and_degrees_correct_across_chunk_boundaries() {
        for n in [499i64, 500, 501, 1000, 1001] {
            let conn = star_chain(n);
            // Hub last, so it lands in the final chunk; leaves straddle every boundary.
            let mut ids: Vec<i64> = (1..=n).collect();
            ids.push(0);
            let nb = neighbors(&conn, &ids).unwrap();
            let deg = degrees(&conn, &ids).unwrap();
            assert_eq!(nb.len(), ids.len(), "n={n}");
            assert_eq!(nb[&0], (1..=n).collect::<Vec<_>>(), "n={n}");
            assert_eq!(deg[&0], n as usize, "n={n}");
            for i in 1..=n {
                let mut want = vec![0];
                if i > 1 {
                    want.push(i - 1);
                }
                if i < n {
                    want.push(i + 1);
                }
                want.sort_unstable();
                assert_eq!(nb[&i], want, "n={n} leaf {i}");
                assert_eq!(deg[&i], want.len(), "n={n} leaf {i}");
            }
        }
    }

    #[test]
    fn capped_depth_zero_returns_only_seeds() {
        let conn = graph(&[(1, 2), (2, 3)]);
        let s = bfs_capped(&conn, &[(1, 8.0), (3, 2.0), (1, 5.0)], 0).unwrap();
        assert_eq!(keys(&s), vec![1, 3]);
        assert_eq!(s[&1], 8.0);
        assert_eq!(s[&3], 2.0);
    }

    #[test]
    fn capped_keeps_max_over_paths_and_lower_rescore_does_not_enter_frontier() {
        // Node 2 is reached from seed 1 (high) and seed 3 (low): the max must remain. On the
        // next hop 2 offers lower scores back to both seeds, which must not overwrite them.
        let conn = graph(&[(1, 2), (2, 3)]);
        let seeds = [(1, 100.0), (3, 60.0)];
        let d1 = bfs_capped(&conn, &seeds, 1).unwrap();
        let want = 100.0 * 0.5 / 2f32.sqrt();
        assert!((d1[&2] - want).abs() < 1e-4);
        let d2 = bfs_capped(&conn, &seeds, 2).unwrap();
        assert_eq!(d2[&1], 100.0);
        assert_eq!(d2[&3], 60.0);
        assert!((d2[&2] - want).abs() < 1e-4);
        // Nothing improved on hop 2, so the frontier is empty and further depth changes nothing.
        assert_eq!(bfs_capped(&conn, &seeds, 5).unwrap(), d2);
    }

    #[test]
    fn ppr_candidate_set_matches_bfs_and_bounds_scores() {
        let conn = hub_graph();
        let seeds = [(1, 8.0)];
        let b = bfs_expand(&conn, &seeds, 2).unwrap();
        let p = ppr(&conn, &seeds, 2).unwrap();
        assert_eq!(keys(&p), keys(&b));
        assert_eq!(p[&1], 8.0);
        assert!(p
            .iter()
            .all(|(&k, &v)| k == 1 || (v > 0.0 && v <= 4.0 + 1e-5)));
    }

    #[test]
    fn ppr_suppresses_hub_relative_to_bfs() {
        // Two seeds both point at hub H(100) which has many links; seed 1 also has leaf 2.
        let mut e = vec![(1, 100), (1, 2), (3, 100)];
        for i in 200..260 {
            e.push((i, 100));
        }
        let conn = graph(&e);
        let seeds = [(1, 8.0), (3, 1.0)];
        let b = bfs_expand(&conn, &seeds, 2).unwrap();
        let p = ppr(&conn, &seeds, 2).unwrap();
        // BFS: hub gets 4.0 (same as leaf). PPR: far-away 2-hop nodes (200..) rank below the leaf.
        assert!((b[&100] - b[&2]).abs() < 1e-6);
        assert!(p[&200] < p[&2]);
    }

    #[test]
    fn ppr_ignores_self_loops_and_handles_isolated_seed() {
        let conn = graph(&[(1, 1)]);
        let p = ppr(&conn, &[(1, 3.0)], 2).unwrap();
        assert_eq!(p.len(), 1);
        assert_eq!(p[&1], 3.0);
        assert!(ppr(&conn, &[], 2).unwrap().is_empty());
    }
}
