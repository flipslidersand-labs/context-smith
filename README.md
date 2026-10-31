# context-smith

[![crates.io](https://img.shields.io/crates/v/context-smith.svg)](https://crates.io/crates/context-smith)
[![docs.rs](https://docs.rs/context-smith/badge.svg)](https://docs.rs/context-smith)
[![CI](https://github.com/flipslidersand/context-smith/actions/workflows/ci.yml/badge.svg)](https://github.com/flipslidersand/context-smith/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/context-smith.svg)](./LICENSE)

**English**: this page | **日本語**: [README.ja.md](./README.ja.md)

**AI context compiler** — a CLI that ranks the files of a Git repository against your task description (BM25 + a dependency graph) and emits a context bundle that fits within a token budget.

```
Feeding a whole large repository → many tokens, most of them unrelated to the task
              ↓
context-smith ranks files via BM25 + a dependency graph
              ↓
Emits a bundle that fits the token budget → far fewer tokens than the whole repository
```

Whether the bundle improves an AI's answers has **not** been measured. How well the ranking finds the right files is measured below (see [Accuracy](#accuracy)).

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

### 3. Query (stdout, for scripts and pipelines)

`query` runs the same selection pipeline as `build` but prints to stdout and
writes nothing to disk — handy for CI or shell scripting.

```bash
# Machine-readable file selection
contextsmith query --repo . --task "auth error" --format json | jq '.files[].path'

# task.md-style summary
contextsmith query --repo . --task "auth error" --format md --explain
```

| Option | Default | Description |
|---|---|---|
| `--repo <PATH>` | required | Path to the target repository |
| `--task <TEXT>` | required | Task description |
| `--budget <N>` | `30000` | Token limit |
| `--format <json\|md>` | `json` | Output format |
| `--explain` | off | Show BM25 scores (md format only) |
| `--index <PATH>` | `{repo}/.contextsmith/index.db` | Index path override |

The JSON output is `{ task, budget, used_tokens, files: [{ path, score, tokens }] }`.

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
 ├── fts_path     — FTS5 (tokenized file paths, BM25 weight ×3)
 ├── fts_symbols  — FTS5 (pre-tokenized, BM25 weight ×2)
 └── fts_body     — FTS5 (file bodies, BM25 weight ×1)
```

### Scoring pipeline

1. **BM25 search** — search `fts_path` (weight ×3), `fts_symbols` (weight ×2) and `fts_body` (weight ×1) via UNION ALL against the task string, taking the top 20 as seeds. A task with more than 8 distinct words is first reduced to its 8 rarest words (fewest files containing them), so common words do not drown out the specific ones.
2. **Dependency BFS** — propagate scores from seed files via bidirectional BFS (depth=2), decaying ×0.5 per hop.
3. **Greedy allocation** — accumulate token counts in descending score order, stopping once the budget is exceeded.
4. **Bundle output** — expand the selected files into Markdown alongside the recent Git diff.

## Supported languages

| Language | Symbol extraction | Import dependency analysis |
|---|---|---|
| Rust | ✅ functions/structs/impl/use | ✅ `crate::` path resolution |
| Python | ✅ functions/classes/import | ✅ module path resolution |
| Go | ✅ functions/types/import | ✅ package path resolution |
| TypeScript | ✅ functions/classes/interface/type/enum/arrow/import | ✅ ES import relative-path resolution (extension & `index.ts` inference) |
| JavaScript | ✅ functions/classes/arrow/import/require | ✅ ES import + CommonJS `require()` relative-path resolution |

TypeScript targets `.ts` / `.tsx` / `.mts` / `.cts`; JavaScript targets `.js` / `.jsx` / `.mjs` / `.cjs`.
Bare imports (node_modules packages such as `react`) are excluded from the dependency graph.

## Accuracy

Measured on merged fix PRs of three public Rust repositories. Task = the title of the issue the PR closed; truth = the `src/**/*.rs` files the PR changed; the index is built fresh at the PR's **base** commit (no leakage from the fix). A file counts as found only if at least 50% of its content is in the bundle. The baseline is a naive keyword match: rank files by how many task words they contain (path matches weighted ×3) and fill the same token budget. Recall, averaged over tasks:

| repo (tasks) | budget | context-smith | naive keyword match |
|---|---|---|---|
| axum (54) | 4k / 8k / 16k | 0.23 / 0.33 / 0.45 | 0.17 / 0.49 / 0.56 |
| clap (60) | 4k / 8k / 16k | 0.08 / 0.15 / 0.28 | 0.16 / 0.27 / 0.38 |
| bat (56) | 4k / 8k / 16k | 0.08 / 0.08 / 0.10 | 0.18 / 0.24 / 0.42 |

In these settings context-smith does **not** beat naive keyword matching, except at the smallest budget on axum. On bat, 88% of the right files are retrieved but ranked too low to fit the budget. Limits: three Rust repositories, BM25 only (no embeddings), an untuned baseline, other languages not measured. To reproduce these numbers (scripts, fixed candidate sets, metric definitions): [bench/](./bench/README.md). Open questions: [#135](https://github.com/flipslidersand-labs/context-smith/issues/135).

## Status

| Phase | Content | Status |
|---|---|---|
| 1 | Git scan / file listing | ✅ |
| 2 | Tree-sitter symbol extraction (Rust/Python/Go/TS/JS) | ✅ |
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
