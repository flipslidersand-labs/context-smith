# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.1] - 2026-11-01

Retrieval fixes found while measuring accuracy on real fix PRs (#135, #160). On the `bench/`
set (strict metric, title-only tasks) recall at a 16k-token budget, against naive keyword
matching: axum 0.69 / 0.56, clap 0.53 / 0.38, bat 0.59 / 0.42, ripgrep 0.49 / 0.27 (Rust);
hono 0.83 / 0.66, zod 0.39 / 0.18 (TypeScript); scrapy 0.48 / 0.47, poetry 0.30 / 0.40
(Python). Python is the weak spot (poetry is still behind), and the symbol weight was chosen
on axum, clap and bat, so their numbers are optimistic.

### Added
- `contextsmith --version` / `-V` (#111).
- `bench/`: scripts, fixed candidate sets and a README that reproduce the Accuracy table
  in the README (#140). Not included in the published crate.

### Fixed
- Multi-word tasks no longer fail with "No matching files found": terms are OR-joined (#133).
- `sanitize_fts_query` uses an allow-list of characters, so punctuation such as `.` or `=`
  in a task can no longer break the FTS5 query (#134).
- The BM25 sub-query `LIMIT` kept the first rows in rowid order instead of the best-scoring
  ones, and long tasks let common words drown out the rare ones. Tasks are now reduced to
  their 8 rarest terms (#137).
- Budget allocation: one huge top-ranked file (benchmark fixtures, large tests) can no
  longer use up the whole budget. A file that does not fit is truncated into the remainder
  only if at least half of it fits (the fragment is then worth having); a smaller fragment is
  skipped and the search goes on. Only when nothing fits is the best file truncated. Files
  with equal scores are ordered by path, so output is deterministic (#142, #162).
- Dependency expansion: each neighbour is scored from its own parent (half of the parent's
  score) instead of half of the best seed's, so unrelated neighbours no longer outrank
  weaker seeds (#143).
- Symbol matches are aggregated per file (best match) and weighted 0.5 instead of 2.0, so
  files with many symbols no longer outrank the best match (#151).
- `--task` values that start with `-` are accepted instead of being rejected as unknown
  options (#149).

### Changed
- Test and fixture files (`tests/`, `test/`, `__tests__/`, `fixtures/` directories, and
  `test_*`, `*_test.*`, `*.test.*`, `*.spec.*` names) have their search score halved. They
  name the same things as the code under test and otherwise filled the top of the results
  (60% of the top 10 on poetry). They are not excluded. The benchmark counts only source
  files as correct, so it favours this change; its value for real use is not measured (#164).
- A bundle no longer ends with a tiny fragment of a file that mostly did not fit (see Budget
  allocation above).
- `Cargo.toml` `repository` / `homepage` point to `flipslidersand-labs/context-smith`.
- README: the Accuracy section reports measured numbers and the comparison with naive
  keyword matching (#139, #155).

## [0.4.0] - 2026-08-27

### Added
- Incremental indexing (#56): `blob_sha` diff detection skips unchanged files on
  re-index. A new `indexed_sha` column tracks what was last ingested; `--force`
  resets it. Deleted files are pruned from the DB automatically. Typical re-index
  on a large repo is now seconds instead of minutes.
- CI coverage gate: `cargo llvm-cov --lib` with a 36% line threshold (lib scope,
  regression guard). fmt/clippy and test jobs now run in parallel (#92).

### Changed
- BFS dependency expansion now batches outgoing+incoming edges per frontier hop
  with a single `IN (…)` query, eliminating the O(2N) per-node call pattern (#91).
- `populate_embeddings` is now incremental: only files whose `blob_sha` changed
  are re-embedded, and vectors are streamed in configurable batches (default 64)
  instead of accumulating all content in RAM first (#93).
- `--budget 0` now returns an error immediately instead of silently producing an
  empty bundle (#89).
- `recent_diff` output is capped at 512 KiB to prevent unbounded growth from
  binary diffs (#94).

### Fixed
- **Security**: `normalize_path` now rejects `..` traversals that escape the repo
  root, preventing potential arbitrary-file access via crafted import paths (#87).
- **Security**: `RemoteEmbedder.api_key` is stored in `Zeroizing<String>` and
  is cleared from memory on drop (#85).
- **Security**: `bundle_writer` escapes triple-backtick code fences using a
  dynamic fence length, preventing context injection (#83).
- `dep_builder`: `resolve_go` excludes `*_test.go`, `*.pb.go`, and `mock_*.go`
  from BFS traversal, reducing irrelevant token usage (#91).
- Stale-file cleanup in `build_index` is now wrapped in the same
  `BEGIN IMMEDIATE` transaction as Pass-1, preventing DB inconsistency on
  crash (#88).
- `IndexDb::open` now sets `PRAGMA busy_timeout = 5000` and runs `PRAGMA
  quick_check` on open (#82).
- `sanitize_fts_query` now down-cases FTS5 boolean keywords (`AND`, `OR`, `NOT`,
  `NEAR`) to prevent them from being treated as operators (#79).
- `select_candidates` now filters by score before calling `read_to_string`,
  avoiding unnecessary file reads and peak RAM usage (#80).
- Slug generation uses an alphanumeric allowlist, stripping null bytes and
  Unicode special characters (#81).
- BM25 normalization query guards against `MIN(rank) = 0` NULL propagation (#90).
- `canonicalize` error messages now include path context; `repo.workdir()` is
  used as the root when available (#86).
- `read_to_string` and `recent_diff` errors are now surfaced via `eprintln!`
  warnings instead of being silently dropped (#84).

## [0.3.0] - 2026-08-22

### Added
- Ruby language support (`.rb`): class/module/method/constant symbol extraction
  and `require`/`require_relative` dependency resolution.
- Rust extractor now captures `enum` variants, `trait` definitions, and `type`
  aliases in addition to functions and structs.
- BM25 path weighting (`fts_path × 3`): filenames matching the task keyword now
  rank higher than body-only matches. A `tokenize_path` helper splits snake_case,
  kebab-case, and mixed identifiers for better prefix matching.

### Changed
- `bundled-sqlite` is now a Cargo feature (default ON). Pass
  `--no-default-features` to link against the system SQLite.
- tree-sitter upgraded to 0.24; all grammars aligned to 0.23 series to resolve
  `cc` version conflicts.

### Fixed
- `blob_to_vec` replaced `as_chunks` (nightly) with `chunks_exact` (stable),
  fixing compilation on Rust ≤ 1.82.

## [0.2.0] - 2026-08-19

### Added
- `query` subcommand (#28): runs the same selection pipeline as `build` but
  prints to stdout and writes nothing to disk. `--format json` emits
  `{ task, budget, used_tokens, files: [{ path, score, tokens }] }`; `--format md`
  emits the task.md-style summary (`--explain` adds BM25 scores). Designed for
  CI and shell pipelines.
- TypeScript and JavaScript language support (#27): symbol extraction
  (functions/classes/interfaces/types/enums/arrow functions/methods) and
  dependency resolution for ES `import` and CommonJS `require()` with extension
  and `index.*` inference. Covers `.ts/.tsx/.mts/.cts` and `.js/.jsx/.mjs/.cjs`.
- English `README.md` as the primary readme; Japanese moved to `README.ja.md` (#32).
- `CHANGELOG.md` and status badges (crates.io / docs.rs / CI / license) (#33).
- Semantic search (Phase 6, #26): optional dense embeddings fused with BM25 via
  Reciprocal Rank Fusion. An `Embedder` trait with a feature-gated
  `RemoteEmbedder` (`remote-embed`, off by default), an `embeddings` BLOB table,
  cosine vector search, and a `--no-embed` build flag. Backward compatible:
  without the feature or stored vectors, seeding stays BM25-only. Semantic seeds
  can now surface files with zero lexical overlap (fusion runs even when BM25
  returns nothing). Verified live against the embedding service (e5, 768-dim):
  Recall@1 improved from 0/3 to 2/3 on a zero-overlap task set.

### Changed
- Token estimation is now CJK-aware and conservative (#30): ASCII keeps the
  `chars / 4` rate while multi-byte characters count as one token each, so
  Japanese-heavy files no longer under-count and overflow the budget. Bundle
  truncation uses the same weighting to cut on a token (and UTF-8) boundary.
  Kept dependency-free (no tiktoken) to preserve the offline design.

### Fixed
- `RemoteEmbedder` now matches the real embedding-svc contract: `X-API-Key`
  auth, `{collection, texts, mode: index|search}` request, `{vectors}` response,
  and `EMBEDDING_SVC_URL` / `EMBEDDING_API_KEY` / `EMBEDDING_COLLECTION` env vars
  (aligned with the memory-ingest ecosystem). Embedding failures during `index`
  are non-fatal (the BM25 index stays usable).

## [0.1.0] - 2026-08-17

Initial release. Published to [crates.io](https://crates.io/crates/context-smith).

### Added
- `index` command — scan a Git repo's HEAD, extract Rust/Python/Go symbols and
  import dependencies via tree-sitter, and store everything in a single SQLite
  (bundled) `index.db`.
- BM25 full-text search over FTS5 (`fts_symbols` ×2 / `fts_body` ×1).
- Dependency-graph expansion — import resolution feeding a bidirectional
  petgraph BFS (0.5-per-hop decay).
- `build` command — greedy token-budget allocation emitting a bundle of
  `task.md` / `relevant-code/` / `citations.json`.
- Regression test suite (17 tests) covering the batch fixes #11–#20 (#21).

### Fixed
- UTF-8 char-boundary panic when truncating multibyte bodies over 512 KiB (#11).
- FTS5 MATCH special-character sanitization to avoid syntax errors (#12).
- Python multi-dot relative import resolution to the parent package (#14).
- Go imports no longer match same-named packages in a different directory (#16).
- Short (<4 char) files are still selected when the budget allows (#17).
- Slug collisions now produce distinct bundle output filenames (#20).

[Unreleased]: https://github.com/flipslidersand/context-smith/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/flipslidersand/context-smith/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/flipslidersand/context-smith/releases/tag/v0.1.0
