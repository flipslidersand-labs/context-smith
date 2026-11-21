# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Breaking
- `select_candidates` now returns `Result<Selection, SelectError>` instead of `Result<Vec<Candidate>>`; use `.candidates` for the old value (#400).

### Security
- libz-sys is updated to 1.1.33 to address RUSTSEC-2026-0544 (#492).

### Changed
- Bump `ureq` to 3 for the opt-in `remote-embed` feature (RemoteEmbedder behavior unchanged: no redirects, retries, 10MB cap) (#356).
- `select_candidates` now returns a `Selection` (`candidates`, `seed_mode`, `skipped`) instead of printing diagnostics itself; the CLI prints the same stderr messages (#400).
- Vector search now orders equal-score hits by ascending file id, so results are deterministic (#481).
- The `index` output-path logic is extracted from `main` into `resolve_index_output`; behavior is unchanged (#402).
- `SelectOptions` gains `seed_count` (default 20) and `bfs_depth` (default 2), replacing hard-coded values in the select pipeline; default behavior is unchanged (#401).
- The scan reuses one lazily opened git index for all tracked-symlink checks instead of reopening the repo per symlink; fail-closed behavior is unchanged (#353).
- The 512 KiB body truncation for the full-text index is now one shared helper instead of two copies; behavior is unchanged (#290).
- `index` now reads only the object header (not the full content) of each tracked blob to get its size, so binary files are no longer loaded on every scan; sizes are unchanged (#352).
- Duplicated and dead code in `bundle_writer` and `budget` is removed; behavior is unchanged (#422).
- `populate_embeddings` no longer takes a redundant connection argument (it uses `db.connection()`), and `build_seeds` no longer returns an always-`Ok` `Result`; the redundant seed re-insert after `bfs_expand` is removed. Behavior is unchanged (#352).

### Fixed
- Indexing skips non-regular files (FIFO/device) instead of blocking on read; index Pass 1 and deps Pass 2 now commit in one transaction; the immutable read-only reopen after CANTOPEN prints a warning (#494).
- A query embedding containing NaN/inf now prints a one-time warning before falling back to BM25-only ranking (#481).
- Embedding writes now check that every vector in a batch, and every batch of a run, has the same dimension; a mid-run change is an error instead of deleting committed batches and restarting (#465).
- Selection no longer lets non-UTF-8 files (indexed by path only) occupy BM25 seed slots: more rows are fetched until `seed_count` readable seeds are found (#465).
- With `remote-embed`, a query-time embedder whose model id differs from the one that wrote the stored vectors falls back to BM25 seeds with a warning, instead of comparing different vector spaces (#465).
- `recent_diff` no longer drops the root commit's files when the repository is shallow only on a merge's second-parent side; a real root is told apart from a shallow boundary by its raw parent header, and a shallow boundary now prints a note on stderr (#494).
- `query`/`build` warn when `--index` points at an index built for a different repository (`meta.repo_root`) (#494).
- `adapters/claude-code/ctx-select.sh` now exits 2 with a message when `contextsmith --version` fails, instead of exiting silently with an undocumented code (#494).
- `open_for_read` follow-ups (#462): the immutable fallback now builds on non-Unix targets and also triggers on `SQLITE_CANTOPEN` for existing files; a write lock during the schema probe is reported as a lock error again; read-only permission tests skip when permissions are not enforced (root).
- `allocate` now sorts scores with a total order and drops NaN-scored candidates, so a NaN score can no longer panic the sort or take first place (#469).
- `search_vectors` and `rrf_fuse` now sort scores with a total order and drop NaN-scored entries, so a NaN score can no longer panic the sort or corrupt the top-N heap (#473).
- A failed rebuild after an extractor-version change no longer leaves the FTS tables empty: the FTS wipe now runs inside the rebuild transaction and is rolled back on failure (#480).
- `recent_diff` in a shallow clone (e.g. `actions/checkout` default) now diffs against the oldest reachable commit instead of the empty tree, so the whole repository no longer lands in the bundle's diff (#465).
- A file extension containing a backtick or newline can no longer break the code fence in `relevant-code/*.md`; only `[A-Za-z0-9_+#-]` is kept in the info string (#328).
- The "index is locked" message no longer prints an empty path (`Index DB  is locked`) for a temporary or in-memory DB (#354).
- `strip_rust_comments` now handles nested block comments (`/* /* */ */`), so the outer `*/` no longer leaks into expanded `use` paths; `EXTRACT_VERSION` is bumped to 9 (#496).
- `query`/`select` now open the index read-only (no `journal_mode` PRAGMA), so an index shipped on a read-only file or directory can be searched (#312).
- `recent_diff` (the bundle's diff) now includes only files still tracked in git with a known language, so a commit that deletes a secret no longer puts the removed lines into the bundle; `embed_index` now reads file bodies only for git-tracked paths, so an untracked `.env` listed in a shipped `index.db` is never sent to the embedding service (#447).
- Restoring a file that had become non-UTF-8 now re-indexes its symbols, FTS body and deps instead of leaving them empty until `--force` (#448).
- Rust grouped `use` with `//` or `/* */` comments no longer drops the item after the comment from the dependency graph (#448).
- `ctx-select.sh` now exits 2 (usage error) when `--budget` or `--top` is given without a value, matching the documented exit codes; it used to exit 1 (#451).
- Go grouped `type ( ... )` declarations now record each spec's own line (and include `X = Y` aliases) instead of the `type (` line; `EXTRACT_VERSION` is now 6, so existing indexes are rebuilt (#436).
- `index` time no longer grows with the square of the file count: `symbols(file_id)` and `deps(to_file)` are now indexed and FTS rows are deleted by rowid; `EXTRACT_VERSION` is now 7, so existing indexes are rebuilt (#283).
- Ruby `Bundler.require(:default)`, `Kernel.require 'x'` and other receiver-qualified `require` / `require_relative` calls are no longer recorded as imports; `EXTRACT_VERSION` is now 8, so existing indexes are rebuilt (#467).
- `select` now rejects `seed_count = 0` with an error instead of reporting no matches, and clamps huge `seed_count`/`top_n` values (max 1000) so BM25 limits no longer overflow (#443).
- Go module detection now reads only `go.mod` files tracked in git; untracked or ignored `go.mod` files are no longer used (#441).
- `mod x;` in `src/tests/<name>/main.rs` (and `src/examples/`, `src/benches/`) now resolves as a normal module instead of a crate root; only `src/bin/<name>/main.rs` and package-level `tests|examples|benches/<name>/main.rs` are roots (#440).
- Go imports now resolve against every `go.mod` in the repo (longest module path wins), so nested modules and `go.work`-style layouts without a root `go.mod` no longer fall back to suffix matching (#429).
- `index` no longer aborts with a stack overflow on deeply nested source files: the symbol walkers stop at depth 500 and skip deeper branches and print one warning per file; `EXTRACT_VERSION` is now 5, so existing indexes are rebuilt (#417).
- `mod x;` in `examples/*.rs` and `benches/*.rs` now resolves next to the file (they are crate roots like `tests/*.rs`); `src/examples/` etc. stay normal modules (#426).
- `mod x;` in a non-crate-root `lib.rs`/`main.rs` (e.g. `src/a/lib.rs`) now resolves under `lib/`/`main/` like a normal module, and `mod r#type;` finds `type.rs` (raw-identifier prefix stripped) (#434).
- Go imports are now resolved only inside the repo-root `go.mod` module path (exact directory match), so `net/http` or external modules no longer link to a same-named repo directory; without `go.mod` the old suffix match is kept (#419).
- `--diff-commits` larger than the history now diffs against the empty tree, so files added by the root commit are included; non-UTF-8 diff lines are kept lossily instead of dropped (#418).
- Remote embedding errors (request failure, redirect `Location`, decode failure) no longer print `user:pass@` from `EMBEDDING_SVC_URL`; it is shown as `***@` (#421).
- `build`/`query` no longer read a path from `index.db` that is not tracked in git (e.g. a crafted row pointing at `.env`) (#395).
- Rust `crate::` imports now resolve against the nearest crate root (`<dir>/src/lib.rs` or `main.rs`) instead of always the repository-root `src/`, fixing missing or wrong edges in Cargo workspaces (#393).
- Ruby `spec/` / `specs/` directories and `*_spec.*` files, and Go `testdata/` directories, are now treated as test paths and get the halved search score (#405).
- Python absolute imports (`import util` / `from util import x`) now resolve against the repository root first and fall back to the importing file's directory, instead of wrongly linking to a same-named sibling module (#394).
- Rust `mod foo;` declarations are now recorded as imports and resolved to `foo.rs` / `foo/mod.rs` next to the declaring file, so child modules without a `use crate::..` are reachable (#406). Existing indexes are rebuilt once on the next `index` (extract version 2 → 3).
- `.ts` / `.mts` / `.cts` files are now parsed with the TypeScript grammar (TSX only for `.tsx`), so `<T>x` assertions and `<T>(v) => v` no longer lose symbols. Existing indexes are rebuilt once on the next `index` (extract version 3 → 4) (#404).
- Go imports no longer create dependency edges to non-Go files (`.ts`, `.py`, ...) in the same directory (#327).
- Ruby `require` / `require_relative` are now resolved into dependencies (previously `.rb` files got no deps despite the 0.3.0 note) (#327).
- Embeddings are recomputed when the embedding model or vector dimension changes (stored in `meta` as `embed_model` / `embed_dim`), and `index --force` now re-embeds unchanged files too (#194).
- Files are re-indexed when only their detected language changes (e.g. `Unknown` to a newly supported language), without needing `--force` (#327).
- Tracked files deleted or made unreadable in the working tree without a commit no longer keep stale rows: the next `index` drops them instead of treating them as unchanged (#327).
- `ctx-select.sh` now runs its helper with `python3 -I`, so a `json.py` in the current directory can no longer run arbitrary code (#396).
- `ctx-select.sh` now prints control characters (newlines, tabs, ...) in file names as `?`, so a crafted file name can no longer forge extra rank rows (#397).
- Rust `mod x;` declarations now resolve from every Cargo target root (`build.rs`, `tests/*.rs`, `src/bin/*.rs`, `src/bin/<name>/main.rs`), not only `lib.rs`/`main.rs`/`mod.rs` (#420).

## [0.5.0] - 2026-11-16

### Added
- Files that were cut to fit the budget are now marked: `task.md` and `relevant-code/*.md`
  show `[truncated: first N of M tokens]`, and `query --format json` and `citations.json`
  gain a `truncated` field (#245).
- A warning is printed when `EMBEDDING_SVC_URL` points to a plain `http://` host that is not
  loopback, because the API key and file contents would be sent unencrypted. Set
  `EMBEDDING_ALLOW_INSECURE=1` to silence it (#242).
- Library users: `select_candidates` and `SelectOptions` are now exported from the crate
  root, moved from the binary into the library (#316).
- Library users: `embed_index(repo, db)` is exported from the crate root when the
  `remote-embed` feature is enabled. It returns `None` when `EMBEDDING_SVC_URL` is not
  configured (#322).
- The minimum supported Rust version is now declared as `rust-version = "1.89"` in
  `Cargo.toml`; it is the requirement of `rusqlite` 0.41 (#305).

### Changed
- **BREAKING (library users):** the public API is narrowed to a few entry points. The modules
  (`budget`, `bundle_writer`, `dep_builder`, `embed`, `index_builder`, `path_kind`,
  `search_index`, `select`, `tokenizer`) are now private. The crate root exports
  `Candidate`, `render_task_md`, `write_bundle`, `build_index`, `IndexDb`, `IndexStats`,
  `select_candidates`, `SelectOptions`, `GitRepo` and, with `remote-embed`, `embed_index`.
  `GitRepo`, `IndexDb`, `IndexStats`, `Candidate` and `SelectOptions` are `#[non_exhaustive]`, so they can no
  longer be built with struct literals outside the crate. `rusqlite` types no longer appear
  in public signatures, and `GitRepo::new`, `root`, `canonical_root` and `recent_diff` remain
  public (#307, #322).
- `build` now counts the Recent Diff against the token budget, together with the header, file
  list and wrappers, so `Used` matches the real total. A diff that does not fit is cut, and
  `task.md` says so; when the whole diff is dropped, a note is still written (#309, #331).
- The embedding client reports a 3xx response as an explicit redirect error (status and
  `Location`) instead of failing with a JSON parse error; redirects are still not followed
  (#369).
- `index --out ""` is now rejected with an error. BM25 sub-query ties are cut by path before
  file id, so equal-scored results come out in the same order on every run (#370).
- Updated tree-sitter to 0.26 and tree-sitter-javascript to 0.25 (#373).
- **The index is rebuilt in full once after upgrading.** The index now records the version of
  the extractor that built it. When it is missing or differs, `index` prints
  `note: index built by an older extractor; rebuilding` and rebuilds everything, as `--force`
  does (#335). The extractor version is now 2, because path tokens also keep the joined form
  (for example `parsehttprequest`); existing indexes are rebuilt on the next `index` (#357).
- `--out` is rejected when it is a symbolic link; pass the real path instead (#319). An
  `index` run is also refused when the index files (`index.db`, `-wal`, `-shm`) or the `--out`
  target are symbolic links (#358).
- `citations.json` `reason` now says `relevance_score=` instead of `bm25_score=`, because the
  value is after fusion, decay and weighting (#357).
- Files with equal scores are ordered by path, not by internal id, in search results
  (#332, #357). In embedding fusion (`rrf_fuse`) ties are now ordered by file id, so the
  result is the same on every run instead of varying between runs (#246).
- The index now follows the working tree: file hashes are computed from the working-tree
  content, so uncommitted edits are indexed and reverting them is picked up. Unchanged files
  are not reprocessed (#184).
- Library users: `rusqlite` is updated from 0.31 to 0.41, and `rusqlite::Connection` appears
  in public signatures, so this is a breaking change for library users (#305).
- Library users: `populate_fts_with_paths` is no longer public (it is test-only), which is a
  breaking change (#315).
- A lock conflict with another `index` run, or with a write in progress, now reports
  "locked by another process" instead of "database is locked" or a misleading
  "Index DB is corrupt" (#295, #301, #334).
- An empty or whitespace-only `EMBEDDING_SVC_URL` is treated as unset (#329).

### Fixed
- `index` no longer fails permanently with a foreign key error after a file that has embeddings
  is deleted from git (#182).
- `index` no longer leaves a transaction open when the cleanup step or `COMMIT` fails
  (#226, #249).
- `index` continues when a repository contains non-UTF-8 paths (skipped with a warning) or
  non-UTF-8 file contents (the file stays searchable by name) (#330).
- Stale rows of files that can no longer be read are removed, and a `query` whose matches are
  all unreadable fails with the reason instead of returning nothing (#299). A `query` whose
  matches are all empty files also fails with a reason (#332).
- Candidate selection no longer hides database errors when reading the `files` table (#225, #235).
- An error while reading the stored extractor version is reported instead of being treated as
  "no version" (#357).
- Symbolic links that resolve to the same file are listed once (#332).
- Embeddings: a vector count or dimension mismatch is detected (#224); 4xx responses and
  responses with invalid JSON are not retried (#217, #271).
- `recent_diff`: lines carry their `+` / `-` / space prefix (#250), and `0` commits always
  gives an empty diff (#253).
- Bundles: the Recent Diff fence can no longer be closed by a diff containing a code fence,
  and newlines in the task are collapsed (#227); a path containing backticks, newlines or
  `-->` can no longer break the file list or the comment (#252); slugs longer than 255 bytes
  are shortened instead of failing the write (#248); a generated slug can no longer overwrite
  another file (#215); stale `relevant-code/*.md` from an earlier run are removed (#223).
- Dependency graph: Rust `use` of an item resolves to its module (#212) and `pub use`
  re-exports are recorded (#244); TypeScript/JavaScript `export ... from` re-exports (#216),
  `.js` specifiers that point to TypeScript sources (#222), module specifiers after comments
  or strings (#247) and imports that climb above the repository root (#298) are handled
  correctly; Python `from X import a, b`, `import a, b` and relative-import edge cases are
  resolved (#254, #266); Go grouped, aliased and commented imports are resolved (#265, #270).
- Dependency expansion: duplicate seeds use the highest score (#279), and large neighbour
  sets no longer exceed the SQLite variable limit (#277).
- `ctx-select.sh` creates its cache directory with mode 700 and refuses a symbolic-link or
  other-user cache directory (#243), and rejects a non-numeric `--top` / `--budget` (#332).

### Security
- Symbolic links: tracked symbolic links that point outside the repository, or to files that
  are not tracked, are no longer read into the index or bundle, including links reached
  through a directory (#211, #338, #358). `task.md`, `citations.json` and `relevant-code/*.md`
  are not written through a symbolic link, and bundle files are created with `create_new`
  (#306, #319).
- The embedding client no longer follows redirects, so the API key cannot be sent to a
  redirect target, and response bodies are limited to 10 MiB (#338).
- `ring` is updated to 0.17.14 (RUSTSEC-2025-0009) (#338).
- `rustls`, `rustls-webpki` and `webpki-roots` are updated; this fixes RUSTSEC-2026-0285,
  RUSTSEC-2026-0346, RUSTSEC-2026-0348, RUSTSEC-2026-0349, RUSTSEC-2026-0381 and
  RUSTSEC-2026-0382 (#348).
- `libz-sys` is updated to 1.1.31, which fixes a build script that leaked environment
  variables into the build directory (#186).
- `git2` is updated to 0.21, which resolves three unsoundness advisories (#213).
- The bundled SQLite is updated to 3.51.3 or later through `rusqlite` 0.41, which includes the
  WAL-reset bug fix; RUSTSEC-2026-0459 is no longer reported (#305).

### Removed
- The unused `walkdir` dependency (#305).

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
- `adapters/claude-code/`: a Claude Code skill and a thin wrapper (`ctx-select.sh`) that list
  the files most relevant to a task in any git repository, keeping the index outside the
  repository (#145). Not included in the published crate.

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
- `index --out <path>` creates the missing parent directories of `<path>` instead of failing
  with "unable to open database file" (#172).

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

### Known limitations
- Only Rust, Python and TypeScript repositories were measured, 11 in all; the result for
  other languages and for repositories of other shapes is unknown.

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

[Unreleased]: https://github.com/flipslidersand-labs/context-smith/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/flipslidersand-labs/context-smith/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/flipslidersand-labs/context-smith/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/flipslidersand-labs/context-smith/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/flipslidersand-labs/context-smith/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/flipslidersand-labs/context-smith/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/flipslidersand-labs/context-smith/releases/tag/v0.1.0
