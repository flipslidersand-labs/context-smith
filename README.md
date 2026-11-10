# context-smith

[![crates.io](https://img.shields.io/crates/v/context-smith.svg)](https://crates.io/crates/context-smith)
[![docs.rs](https://docs.rs/context-smith/badge.svg)](https://docs.rs/context-smith)
[![CI](https://github.com/flipslidersand-labs/context-smith/actions/workflows/ci.yml/badge.svg)](https://github.com/flipslidersand-labs/context-smith/actions/workflows/ci.yml)
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
| `--force` | off | Force a full re-index even for files whose content has not changed |

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
| `--index <PATH>` | `{repo}/.contextsmith/index.db` | Index path override (use when the index was built with `--out`) |
| `--explain` | off | Show BM25 scores in `task.md` |
| `--diff-commits <N>` | `3` | Number of recent commits to include in the diff |
| `--no-embed` | off | Disable semantic (embedding) seeds even when the index has them; BM25 only |

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
| `--no-embed` | off | Disable semantic (embedding) seeds even when the index has them; BM25 only |

The JSON output is `{ task, budget, used_tokens, files: [{ path, score, tokens, truncated }] }`. `truncated` is `true` when the file was cut to fit the remaining budget.

## Architecture

```
contextsmith index          contextsmith build
      │                           │
      ▼                           ▼
 [GitRepo.scan()]         [search_bm25()]       ← FTS5 UNION ALL
 [SymbolExtractor]        [bfs_expand()]        ← SQL-based BFS
 [build_deps()]           [allocate()]          ← greedy budget
 [populate_fts()]         [write_bundle()]      ← Markdown + JSON
      │
      ▼
 .contextsmith/index.db (SQLite)
 ├── files        — path, language, blob SHA of the working-tree content (uncommitted edits are indexed; only files tracked in HEAD)
 ├── symbols      — function/struct/class names, line numbers, snippets
 ├── deps         — inter-file import dependency graph
 ├── meta         — repo_root, indexed_at
 ├── fts_path     — FTS5 (tokenized file paths, BM25 weight ×3)
 ├── fts_symbols  — FTS5 (pre-tokenized, one row per symbol, BM25 weight ×0.5)
 └── fts_body     — FTS5 (file bodies, BM25 weight ×1)
```

### Scoring pipeline

1. **BM25 search** — search `fts_path` (weight ×3), `fts_symbols` (weight ×0.5, best match per file) and `fts_body` (weight ×1) via UNION ALL against the task string, taking the top 20 as seeds. A task with more than 8 distinct words is first reduced to its 8 rarest words (fewest files containing them), so common words do not drown out the specific ones.
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
| Ruby | ✅ class/module/method/constant (`.rb`) | ❌ not supported |

TypeScript targets `.ts` / `.tsx` / `.mts` / `.cts`; JavaScript targets `.js` / `.jsx` / `.mjs` / `.cjs`.
Bare imports (node_modules packages such as `react`) are excluded from the dependency graph.

## Accuracy

Measured on merged fix PRs of five public Rust repositories. Task = the title of the issue the PR closed; truth = the `src/**/*.rs` files the PR changed; the index is built fresh at the PR's **base** commit (no leakage from the fix). A file counts as found only if at least 50% of its content is in the bundle. The baseline is a naive keyword match: rank files by how many task words they contain (path matches weighted ×3) and fill the same token budget. Recall, averaged over tasks:

| repo (tasks) | budget | context-smith | naive keyword match |
|---|---|---|---|
| axum (54) | 4k / 8k / 16k | 0.45 / 0.54 / 0.67 | 0.17 / 0.49 / 0.56 |
| clap (60) | 4k / 8k / 16k | 0.23 / 0.33 / 0.48 | 0.16 / 0.27 / 0.38 |
| bat (56) | 4k / 8k / 16k | 0.25 / 0.35 / 0.48 | 0.18 / 0.24 / 0.42 |
| ripgrep (26) | 4k / 8k / 16k | 0.15 / 0.22 / 0.49 | 0.08 / 0.19 / 0.27 |
| fd (74) | 4k / 8k / 16k | 0.37 / 0.48 / 0.60 | 0.25 / 0.37 / 0.69 |

On the five Rust repositories context-smith is ahead of naive keyword matching at 4k (by 0.07-0.28) and at 8k (by 0.03-0.11) on every one, and at 16k on four of them (by 0.06-0.22); on fd it is behind at 16k (by 0.09, about 7 of its 74 tasks). One task is worth 0.014-0.04 depending on the repository (74 to 26 tasks), so gaps of 0.03 or less are within noise. For scale: if the right files were known in advance, 87-99% of them would fit whole into 16k. These numbers are for the development version; the 0.4.0 release on crates.io predates the budget, ranking and symbol-scoring fixes ([#142](https://github.com/flipslidersand-labs/context-smith/pull/142), [#143](https://github.com/flipslidersand-labs/context-smith/pull/143), [#151](https://github.com/flipslidersand-labs/context-smith/pull/151), [#162](https://github.com/flipslidersand-labs/context-smith/pull/162)). Limits: five Rust repositories, BM25 only (no embeddings), an untuned baseline. Other languages (Python, TypeScript): see [bench/](./bench/README.md#other-languages-python-typescript-added-2026-11-01-148): TypeScript is ahead of the baseline in both repositories at every budget (hono's lead at 16k, 0.02, is noise); **Python is behind it at 16k in both** (scrapy by 0.10, poetry by 0.21, and poetry at 8k by 0.09), so do not expect the Rust numbers to carry over to Python. On poetry and hono, recall at 16k is lower than at 8k, which should not happen when the budget grows. Cause, confirmed on the two poetry tasks that drop from 1.00 to 0.00 (a 16,000-token test file takes the whole budget as a fragment, where at 8k it is skipped): the half-file rule ([#162](https://github.com/flipslidersand-labs/context-smith/pull/162)) lets a large, high-ranked file that is not a truth file use up the whole budget when it is at most twice the budget, see [#135](https://github.com/flipslidersand-labs/context-smith/issues/135). The symbol weight (0.5) and the budget rules were developed on axum, clap, bat and ripgrep, so the table is optimistic for them; fd and the four Python and TypeScript repositories were not used to choose any parameter. ripgrep's truth files are large (median about 9,400 tokens, versus about 3,500 elsewhere), which is where the budget rule matters most. To reproduce these numbers (scripts, fixed candidate sets, metric definitions): [bench/](./bench/README.md). Open questions: [#135](https://github.com/flipslidersand-labs/context-smith/issues/135).

## Status

| Phase | Content | Status |
|---|---|---|
| 1 | Git scan / file listing | ✅ |
| 2 | Tree-sitter symbol extraction (Rust/Python/Go/TS/JS) | ✅ |
| 3 | Dependency graph construction (SQL-based BFS) | ✅ |
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
