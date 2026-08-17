# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- English `README.md` as the primary readme; Japanese moved to `README.ja.md` (#32).
- `CHANGELOG.md` and status badges (crates.io / docs.rs / CI / license) (#33).

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

[Unreleased]: https://github.com/flipslidersand/context-smith/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/flipslidersand/context-smith/releases/tag/v0.1.0
