# CLAUDE.md

Rust CLI (`contextsmith`) that ranks a Git repo's files against a task (BM25 + dependency graph) and emits a token-budgeted bundle. Usage and flags: [README.md](./README.md). Design notes: [docs/](./docs/).

## Commands

```bash
cargo test                                   # default build (bundled SQLite)
cargo test --all --no-default-features       # what CI runs; needs libsqlite3-dev + pkg-config
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings    # same as .pre-commit-config.yaml
```

`remote-embed` is an opt-in feature (`--features remote-embed`); the default build is BM25 only.

## Pitfalls

- Bump `EXTRACT_VERSION` in `src/index_builder.rs` whenever stored extraction output changes (symbols, imports, FTS rows). Dependency *resolution* is rebuilt on every `index` run and needs no bump.
- Every user-visible fix/change gets a line under `## [Unreleased]` in [CHANGELOG.md](./CHANGELOG.md). Parallel PRs conflict there; keep both lines.
- The public library API is only what `src/lib.rs` re-exports; other modules are private. Core types are `#[non_exhaustive]`.
- Paths read from `index.db` must be tracked in git (see #395); keep that check when touching `select` / `bundle_writer`.
- Do not tag, publish a GitHub Release or run `cargo publish` from a feature PR, and do not bump `version` by hand. `prepare-release.yml` (release-please) opens a draft release PR; merging it is the human step and does the rest. PR titles are Conventional Commits and decide the bump (`feat!` / `BREAKING CHANGE` is a minor bump while on 0.x). The workflow renames `## [Unreleased]` in that PR. Until the prerequisites are done (#611–#613) releases are still tagged and published by hand.
