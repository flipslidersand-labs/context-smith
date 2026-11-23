# context-smith

[![crates.io](https://img.shields.io/crates/v/context-smith.svg)](https://crates.io/crates/context-smith)
[![docs.rs](https://docs.rs/context-smith/badge.svg)](https://docs.rs/context-smith)
[![CI](https://github.com/flipslidersand-labs/context-smith/actions/workflows/ci.yml/badge.svg)](https://github.com/flipslidersand-labs/context-smith/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/context-smith.svg)](./LICENSE)

Release type: `library`

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
| `--budget <N>` | `30000` | Token limit for the whole bundle (`task.md` + `relevant-code/`, incl. headers and the diff; the diff is cut to fit, files are shrunk first). File bodies take priority; the diff gets only what is left, and when none of it fits `task.md` still shows `## Recent Diff` with an omitted note. `Used` in `task.md` is the real total. If the fixed headers alone exceed a tiny budget, file bodies and diff are dropped and `Used` shows the overshoot |
| `--out <PATH>` | `context.bundle` | Output directory |
| `--index <PATH>` | `{repo}/.contextsmith/index.db` | Index path override (use when the index was built with `--out`) |
| `--explain` | off | Show relevance scores in `task.md` |
| `--diff-commits <N>` | `3` | Number of recent commits to include in the diff |
| `--no-embed` | off | Disable semantic (embedding) seeds even when the index has them; BM25 only (no effect unless built with the `remote-embed` feature) |

### Bundle layout

```
context.bundle/
├── task.md           # Task description + selected file list + recent Git diff
├── relevant-code/    # Contents of the selected files (Markdown code blocks)
└── citations.json    # Per-file scores and token counts (machine-readable); the recent diff is a final entry `(recent diff)`
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
| `--budget <N>` | `30000` | Token limit for the whole bundle (`task.md` + `relevant-code/`, incl. headers and the diff; the diff is cut to fit, files are shrunk first). File bodies take priority; the diff gets only what is left, and when none of it fits `task.md` still shows `## Recent Diff` with an omitted note. `Used` in `task.md` is the real total. If the fixed headers alone exceed a tiny budget, file bodies and diff are dropped and `Used` shows the overshoot |
| `--format <json\|md>` | `json` | Output format |
| `--explain` | off | Show relevance scores (md format only) |
| `--index <PATH>` | `{repo}/.contextsmith/index.db` | Index path override |
| `--no-embed` | off | Disable semantic (embedding) seeds even when the index has them; BM25 only (no effect unless built with the `remote-embed` feature) |

Environment: `CONTEXTSMITH_EXPAND` = `bfs-capped` (default: cap 10 neighbours per file, low-degree first) / `bfs` (previous plain BFS) / `ppr` (personalized PageRank ranking, experimental) selects how dependency scores are spread.

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
| Ruby | ✅ class/module/method/require (`.rb`) | ✅ `require_relative` from the caller's directory; `require 'x'` to `lib/x.rb` or `x.rb` (gems are not resolved) |

TypeScript targets `.ts` / `.tsx` / `.mts` / `.cts`; JavaScript targets `.js` / `.jsx` / `.mjs` / `.cjs`.
Bare imports (node_modules packages such as `react`) are excluded from the dependency graph.

## Accuracy

Measured on merged fix PRs of nine public repositories: five Rust (axum, clap, bat, ripgrep, fd), two Python (scrapy, poetry), two TypeScript (hono, zod). Task = the title of the issue the PR closed; truth = the source files the PR changed (`src/**/*.rs`, `*.py`, `*.ts` / `*.tsx`; tests and docs excluded); the index is built fresh at the PR's **base** commit (no leakage from the fix). A file counts as found only if at least 50% of its content is in the bundle. The baseline is a naive keyword match: rank files by how many task words they contain (path matches weighted ×3) and fill the same token budget. Recall, averaged over tasks, context-smith / naive keyword match (difference in brackets):

| repo (tasks) | 4k | 8k | 16k |
|---|---|---|---|
| axum (54) | 0.47 / 0.17 (+0.30) | 0.58 / 0.49 (+0.09) | 0.70 / 0.56 (+0.14) |
| clap (60) | 0.26 / 0.16 (+0.10) | 0.35 / 0.27 (+0.08) | 0.53 / 0.38 (+0.15) |
| bat (56) | 0.28 / 0.18 (+0.10) | 0.40 / 0.24 (+0.16) | 0.63 / 0.42 (+0.21) |
| ripgrep (26) | 0.15 / 0.08 (+0.07) | 0.22 / 0.19 (+0.03) | 0.49 / 0.27 (+0.22) |
| fd (74) | 0.38 / 0.25 (+0.13) | 0.57 / 0.37 (+0.20) | 0.76 / 0.69 (+0.07) |
| scrapy (59) | 0.29 / 0.19 (+0.10) | 0.45 / 0.29 (+0.16) | 0.54 / 0.47 (+0.07) |
| poetry (60) | 0.23 / 0.19 (+0.04) | 0.25 / 0.31 (**-0.06**) | 0.29 / 0.40 (**-0.11**) |
| hono (60) | 0.59 / 0.35 (+0.24) | 0.81 / 0.56 (+0.25) | 0.86 / 0.66 (+0.20) |
| zod (60) | 0.21 / 0.04 (+0.17) | 0.31 / 0.07 (+0.24) | 0.38 / 0.18 (+0.20) |

Source: [bench/README.md](./bench/README.md#targets-and-current-status-147), measured 2026-11-21 on master `53e4db1` with the `bfs-capped` expansion (the default since [#293](https://github.com/flipslidersand-labs/context-smith/issues/293)), release binary, one run per repository, strict metric, title-only tasks. Reproduce with `REPO=<clone> TASKMODE=title python3 bench/bench.py` (setup and per-repository options: [bench/](./bench/README.md)). One task is worth 0.02-0.04, so gaps of 0.03 or less are within noise.

**The accuracy target is not met ([#135](https://github.com/flipslidersand-labs/context-smith/issues/135) stays open).** The floor (context-smith at least the baseline minus 0.02 in every cell) holds in 25 of 27 cells; the two that fail are **poetry at 8k (-0.06) and 16k (-0.11)**. The value target (at 16k, ahead of the baseline by 0.05 or more) holds in 8 of 9 repositories; the miss is poetry (-0.11), Python. So do not expect the Rust numbers to carry over to Python. In this measurement recall no longer drops from 8k to 16k on poetry (0.25, 0.29) or hono (0.81, 0.86); the earlier figures (master `aa4ef89`, before [#164](https://github.com/flipslidersand-labs/context-smith/pull/164)) did, and are kept in [bench/](./bench/README.md). With the true files known, the whole files that fit into 16k give a ceiling of axum 0.97, clap 0.87, bat 0.99, scrapy 0.97, poetry 0.99, hono 1.00, zod 0.68 (bench; ripgrep and fd not listed there). These numbers are for the development version. The budget, ranking and symbol-scoring fixes ([#142](https://github.com/flipslidersand-labs/context-smith/pull/142), [#143](https://github.com/flipslidersand-labs/context-smith/pull/143), [#151](https://github.com/flipslidersand-labs/context-smith/pull/151), [#162](https://github.com/flipslidersand-labs/context-smith/pull/162)) and [#164](https://github.com/flipslidersand-labs/context-smith/pull/164) are in 0.4.1, the version on crates.io; later fixes listed in [CHANGELOG.md](./CHANGELOG.md) are not published yet. Limits: BM25 only (no embeddings), an untuned baseline, one run per repository. The symbol weight (0.5) and the budget rules were developed on axum, clap, bat, scrapy and poetry, so the table is optimistic for them; ripgrep, fd, hono and zod were not used to choose any parameter. ripgrep's truth files are large (median about 9,400 tokens, versus about 3,500 elsewhere), which is where the budget rule matters most. To reproduce these numbers (scripts, fixed candidate sets, metric definitions): [bench/](./bench/README.md). Open questions: [#135](https://github.com/flipslidersand-labs/context-smith/issues/135).

## Status

| Phase | Content | Status |
|---|---|---|
| 1 | Git scan / file listing | ✅ |
| 2 | Tree-sitter symbol extraction (Rust/Python/Go/TS/JS/Ruby) | ✅ |
| 3 | Dependency graph construction (SQL-based BFS) | ✅ |
| 4 | BM25 full-text search (SQLite FTS5) | ✅ |
| 5 | Budget allocation + bundle output | ✅ |
| 6 | Embedding seeds + RRF fusion (opt-in `remote-embed` feature, remote embedding service via `EMBEDDING_SVC_URL`; plain `http://` to a non-loopback host is refused unless `EMBEDDING_ALLOW_INSECURE=1`, because the API key and file contents would be sent unencrypted) | ✅ (a local embedding model is not implemented) |

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
