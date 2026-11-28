//! Dependency-graph expansion strategies, selected by `CONTEXTSMITH_EXPAND` (#293).
//!
//! `bfs-capped` (default since the #293 bench) caps each file's neighbours so hub files do not
//! collect score. `bfs` is the earlier plain [`bfs_expand`].

use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};

/// Environment variable that selects the expansion mode.
pub const EXPAND_ENV: &str = "CONTEXTSMITH_EXPAND";

const CAP_K: usize = 10;
/// Max ids per `IN` clause (stays far below SQLite's bound-variable limit).
pub(crate) const CHUNK: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpandMode {
    Bfs,
    BfsCapped,
}

/// Parse a mode name; `None` (unset) and a blank value mean the default `bfs-capped`.
pub fn parse_mode(v: Option<&str>) -> Result<ExpandMode> {
    match v.map(str::trim).filter(|s| !s.is_empty()) {
        Some("bfs") => Ok(ExpandMode::Bfs),
        None | Some("bfs-capped") => Ok(ExpandMode::BfsCapped),
        Some(other) => {
            anyhow::bail!("unknown {EXPAND_ENV} value '{other}' (expected bfs or bfs-capped)")
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
    }
}

pub(crate) fn placeholders(n: usize) -> String {
    (1..=n)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Undirected, de-duplicated neighbour lists (self-loops removed) for `ids`, leaving out any
/// neighbour in `exclude`.
fn neighbors(
    conn: &Connection,
    ids: &[i64],
    exclude: &HashSet<i64>,
) -> Result<HashMap<i64, Vec<i64>>> {
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
                if !exclude.contains(&b) {
                    sets.entry(a).or_default().insert(b);
                }
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

pub(crate) fn dedup_seeds(seeds: &[(i64, f32)]) -> HashMap<i64, f32> {
    let mut m: HashMap<i64, f32> = HashMap::new();
    for &(id, s) in seeds {
        let e = m.entry(id).or_insert(0.0);
        if s > *e {
            *e = s;
        }
    }
    m
}

/// BFS-expand from seed files through the deps graph (bidirectional, score decay × 0.5/hop).
///
/// Each depth level fetches the whole frontier's neighbours in 2 SQL queries per chunk of
/// `CHUNK` ids (outgoing and incoming edges), instead of 2 per node. (#91) Edges inside the
/// frontier are not expanded, so the BFS does not re-visit it.
pub fn bfs_expand(
    conn: &Connection,
    seeds: &[(i64, f32)],
    max_depth: u8,
) -> Result<HashMap<i64, f32>> {
    let mut scores = dedup_seeds(seeds);

    // Build the frontier from the deduplicated max scores so duplicate seeds cannot
    // overwrite each other (last-wins) when collected into a map below. (#274)
    let mut frontier: Vec<(i64, f32)> = scores.iter().map(|(&id, &s)| (id, s)).collect();

    for _depth in 0..max_depth {
        if frontier.is_empty() {
            break;
        }

        let ids: Vec<i64> = frontier.iter().map(|&(id, _)| id).collect();
        let in_frontier: HashSet<i64> = ids.iter().copied().collect();
        let nbrs = neighbors(conn, &ids, &in_frontier)?;

        // Each neighbour gets the decayed score of its own best parent. (It used to get the
        // decayed score of the best node in the whole frontier, so every neighbour of any
        // seed outranked the weaker seeds. #135)
        let mut best: HashMap<i64, f32> = HashMap::new();
        for &(parent, score) in &frontier {
            let decayed = score * 0.5;
            for &neighbor in nbrs.get(&parent).into_iter().flatten() {
                let e = best.entry(neighbor).or_insert(f32::NEG_INFINITY);
                if decayed > *e {
                    *e = decayed;
                }
            }
        }

        let mut next_frontier: Vec<(i64, f32)> = Vec::new();
        for (neighbor_id, decayed) in best {
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

/// BFS where each parent expands to at most `CAP_K` neighbours (lowest degree first, then id),
/// and a neighbour's score is `parent * 0.5 / sqrt(max(1, deg(neighbour)))`. Neighbours in the
/// current frontier, or already scored at least as high as this parent would give, are skipped
/// before the K cut so they do not use up its slots.
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
        let in_frontier: HashSet<i64> = ids.iter().copied().collect();
        // Like `bfs_expand`, edges inside the current frontier are not expanded (a seed is
        // never re-scored from a neighbouring seed) and do not use up a parent's K slots.
        let nbrs = neighbors(conn, &ids, &in_frontier)?;
        let mut all: Vec<i64> = nbrs.values().flatten().copied().collect();
        all.sort_unstable();
        all.dedup();
        let deg = degrees(conn, &all)?;
        let mut best: HashMap<i64, f32> = HashMap::new();
        for &(parent, ps) in &frontier {
            let Some(list) = nbrs.get(&parent) else {
                continue;
            };
            // Also drop neighbours whose score this parent cannot raise (#529): they would
            // take a K slot but never enter `scores`/`next`, crowding out real neighbours.
            let mut picked: Vec<(usize, i64)> = list
                .iter()
                .map(|&n| (deg.get(&n).copied().unwrap_or(1), n))
                .filter(|&(d, n)| match scores.get(&n) {
                    Some(&prev) => ps * 0.5 / (d.max(1) as f32).sqrt() > prev,
                    None => true,
                })
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
    fn parse_mode_blank_is_default() {
        // #531: `CONTEXTSMITH_EXPAND=` (set but empty) must behave like unset.
        assert_eq!(parse_mode(Some("")).unwrap(), ExpandMode::BfsCapped);
        assert_eq!(parse_mode(Some("  \t")).unwrap(), ExpandMode::BfsCapped);
    }

    #[test]
    fn parse_mode_values() {
        assert_eq!(parse_mode(None).unwrap(), ExpandMode::BfsCapped);
        assert_eq!(parse_mode(Some("bfs")).unwrap(), ExpandMode::Bfs);
        assert_eq!(
            parse_mode(Some("bfs-capped")).unwrap(),
            ExpandMode::BfsCapped
        );
        let e = parse_mode(Some("nope")).unwrap_err().to_string();
        assert!(e.contains("nope") && e.contains(EXPAND_ENV), "{e}");
    }

    #[test]
    fn parse_mode_ppr_is_unknown() {
        // #533: `ppr` was removed after the bench rejected it (ADR-005).
        let e = parse_mode(Some("ppr")).unwrap_err().to_string();
        assert!(e.contains("ppr") && e.contains(EXPAND_ENV), "{e}");
        assert!(e.contains("expected bfs or bfs-capped"), "{e}");
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
    fn capped_already_scored_nodes_do_not_use_parent_k_slots() {
        // #529: seed 1 - node 2 - leaves 3..=14. At depth 2, node 2's neighbours are the seed 1
        // (already scored, whose score cannot rise) and 12 leaves. The seed must not take one
        // of node 2's K slots, so 10 leaves (3..=12) are picked, not 9 (3..=11).
        let mut e = vec![(1, 2)];
        e.extend((3..=14).map(|i| (2, i)));
        let conn = graph(&e);
        let s = bfs_capped(&conn, &[(1, 1.0)], 2).unwrap();
        assert_eq!(keys(&s), (1..=12).collect::<Vec<i64>>());
        assert_eq!(s[&1], 1.0);
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
            let nb = neighbors(&conn, &ids, &HashSet::new()).unwrap();
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
    fn neighbors_empty_ids_returns_empty() {
        let conn = graph(&[(1, 2)]);
        assert!(neighbors(&conn, &[], &HashSet::new()).unwrap().is_empty());
    }

    #[test]
    fn neighbors_returns_both_directions_and_excludes_given_ids() {
        let conn = graph(&[(1, 2), (3, 1), (1, 4), (1, 5)]);
        let ex: HashSet<i64> = [1, 4].into();
        let nb = neighbors(&conn, &[1, 4], &ex).unwrap();
        // (1,4) is internal to the excluded set and dropped; 4's only neighbour is 1.
        assert_eq!(nb.get(&1), Some(&vec![2, 3, 5]));
        assert!(!nb.contains_key(&4));
    }

    #[test]
    fn neighbors_handles_ids_over_sqlite_variable_limit() {
        let n: i64 = 40_000; // > SQLite's 32766 bound-variable limit
        let edges: Vec<(i64, i64)> = (1..=n)
            .map(|i| {
                if i % 2 == 0 {
                    (i, 100_000 + i)
                } else {
                    (100_000 + i, i)
                }
            })
            .collect();
        let conn = graph(&edges);
        let ids: Vec<i64> = (1..=n).collect();
        let ex: HashSet<i64> = ids.iter().copied().collect();
        let nb = neighbors(&conn, &ids, &ex).unwrap();
        assert_eq!(nb.len(), n as usize);
        assert_eq!(nb[&n], vec![100_000 + n]);
        assert_eq!(nb[&1], vec![100_001]);
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
}

#[cfg(test)]
mod bfs_tests {
    use super::*;

    /// In-memory graph with the given `from -> to` import edges.
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

    #[test]
    fn neighbour_gets_half_of_its_own_parent_score_not_the_best_seed() {
        // #135: file 3 is only linked to the weak seed 2. It used to get half of the *best*
        // seed's score (5.0) and outrank real seeds; it must get half of seed 2's (0.5).
        let conn = graph(&[(2, 3)]);
        let s = bfs_expand(&conn, &[(1, 10.0), (2, 1.0)], 1).unwrap();
        assert!((s[&3] - 0.5).abs() < 1e-6, "got {}", s[&3]);
        assert!((s[&1] - 10.0).abs() < 1e-6 && (s[&2] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn incoming_edge_counts_too() {
        // 3 imports seed 2: the edge is 3 -> 2, so 3 is a neighbour via an incoming edge.
        let conn = graph(&[(3, 2)]);
        let s = bfs_expand(&conn, &[(1, 10.0), (2, 1.0)], 1).unwrap();
        assert!((s[&3] - 0.5).abs() < 1e-6, "got {}", s[&3]);
    }

    #[test]
    fn neighbour_of_two_seeds_takes_the_higher_decayed_score() {
        let conn = graph(&[(1, 3), (2, 3)]);
        let s = bfs_expand(&conn, &[(1, 10.0), (2, 4.0)], 1).unwrap();
        assert!((s[&3] - 5.0).abs() < 1e-6, "got {}", s[&3]);
    }

    #[test]
    fn score_halves_again_on_the_second_hop() {
        let conn = graph(&[(1, 2), (2, 3)]);
        let s = bfs_expand(&conn, &[(1, 8.0)], 2).unwrap();
        assert!(
            (s[&2] - 4.0).abs() < 1e-6 && (s[&3] - 2.0).abs() < 1e-6,
            "{s:?}"
        );
    }

    #[test]
    fn seeds_linked_to_each_other_keep_their_own_scores() {
        // Expansion only scores files outside the current frontier; it never rewrites a seed.
        let conn = graph(&[(1, 2)]);
        let s = bfs_expand(&conn, &[(1, 10.0), (2, 1.0)], 2).unwrap();
        assert!(
            (s[&1] - 10.0).abs() < 1e-6 && (s[&2] - 1.0).abs() < 1e-6,
            "{s:?}"
        );
    }
}
