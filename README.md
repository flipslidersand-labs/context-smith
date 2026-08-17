# context-smith

[![crates.io](https://img.shields.io/crates/v/context-smith.svg)](https://crates.io/crates/context-smith)
[![docs.rs](https://docs.rs/context-smith/badge.svg)](https://docs.rs/context-smith)
[![CI](https://github.com/flipslidersand/context-smith/actions/workflows/ci.yml/badge.svg)](https://github.com/flipslidersand/context-smith/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/context-smith.svg)](./LICENSE)

**English**: this page | **日本語**: [README.ja.md](./README.ja.md)

**AI context compiler** — a CLI that selects only the code relevant to your task from a Git repository and emits a context bundle that fits within a token budget.

```
Feeding a whole large repository → irrelevant code degrades the AI's accuracy
              ↓
context-smith auto-selects relevant files via BM25 + a dependency graph
              ↓
Emits a bundle that fits the token budget → improves both accuracy and cost
```

## Installation

```bash
# from crates.io (recommended)
cargo install context-smith

# from source
cargo install --path .
```

Bundled dependencies (built automatically via the `bundled` feature): libgit2, SQLite, tree-sitter.

## Usage

### 1. Build the index

```bash
contextsmith index --repo ./my-project
```

Generates a SQLite index at `./my-project/.contextsmith/index.db`. The file list,
symbols, dependencies, and the BM25 full-text index all live in a single file.

For small repositories (rule of thumb: source under 10 MB) you can commit `index.db`
to share it with your team.

| Option | Default | Description |
|---|---|---|
| `--repo <PATH>` | required | Path to the target repository |
| `--out <PATH>` | `.contextsmith/index.db` | Override the output location |

### 2. Generate a context bundle

```bash
contextsmith build \
  --repo ./my-project \
  --task "Investigate the authentication error" \
  --budget 30000
```

Writes the bundle to the `context.bundle/` directory.

| Option | Default | Description |
|---|---|---|
| `--repo <PATH>` | required | Path to the target repository |
| `--task <TEXT>` | required | Task description |
| `--budget <N>` | `30000` | Token limit |
| `--out <PATH>` | `context.bundle` | Output directory |
| `--explain` | off | Show BM25 scores in `task.md` |
| `--diff-commits <N>` | `3` | Number of recent commits to include in the diff |

### Bundle layout

```
context.bundle/
├── task.md           # Task description + selected file list + recent Git diff
├── relevant-code/    # Contents of the selected files (Markdown code blocks)
└── citations.json    # Per-file scores and token counts (machine-readable)
```

## Architecture

```
contextsmith index          contextsmith build
      │                           │
      ▼                           ▼
 [GitRepo.scan()]         [search_bm25()]       ← FTS5 UNION ALL
 [SymbolExtractor]        [bfs_expand()]        ← petgraph BFS
 [build_deps()]           [allocate()]          ← greedy budget
 [populate_fts()]         [write_bundle()]      ← Markdown + JSON
      │
      ▼
 .contextsmith/index.db (SQLite)
 ├── files        — path, language, blob SHA
 ├── symbols      — function/struct/class names, line numbers, snippets
 ├── deps         — inter-file import dependency graph
 ├── meta         — repo_root, indexed_at
 ├── fts_symbols  — FTS5 (pre-tokenized, BM25 weight ×2)
 └── fts_body     — FTS5 (file bodies, BM25 weight ×1)
```

### Scoring pipeline

1. **BM25 search** — search `fts_symbols` (weight ×2) and `fts_body` (weight ×1) via UNION ALL against the task string, taking the top 20 as seeds.
2. **Dependency BFS** — propagate scores from seed files via bidirectional BFS (depth=2), decaying ×0.5 per hop.
3. **Greedy allocation** — accumulate token counts in descending score order, stopping once the budget is exceeded.
4. **Bundle output** — expand the selected files into Markdown alongside the recent Git diff.

## Supported languages

| Language | Symbol extraction | Import dependency analysis |
|---|---|---|
| Rust | ✅ functions/structs/impl/use | ✅ `crate::` path resolution |
| Python | ✅ functions/classes/import | ✅ module path resolution |
| Go | ✅ functions/types/import | ✅ package path resolution |

## Status

| Phase | Content | Status |
|---|---|---|
| 1 | Git scan / file listing | ✅ |
| 2 | Tree-sitter symbol extraction (Rust/Python/Go) | ✅ |
| 3 | Dependency graph construction (petgraph BFS) | ✅ |
| 4 | BM25 full-text search (SQLite FTS5) | ✅ |
| 5 | Budget allocation + bundle output | ✅ |
| 6 | Local embedding model + RRF fusion | 🔲 planned |

## Development

```bash
cargo test           # run the test suite
cargo clippy --all-targets
cargo fmt --all --check
```

### Release flow

1. Bump `version` in `Cargo.toml` and update [CHANGELOG.md](./CHANGELOG.md).
2. Tag and push: `git tag vX.Y.Z && git push origin vX.Y.Z`.
3. Publish: `cargo publish` (requires a crates.io token via `cargo login`; the account email must be verified).

See [CHANGELOG.md](./CHANGELOG.md) for release history.

## License

MIT — see [LICENSE](./LICENSE).
