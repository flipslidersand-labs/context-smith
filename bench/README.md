# bench/ — reproducing the README "Accuracy" table

Measures how often `contextsmith query` puts the files a real fix touched into the bundle, against a naive keyword baseline. Manual, not part of CI (needs three external clones and takes a while). Background and open questions: [#135](https://github.com/flipslidersand-labs/context-smith/issues/135).

## What is measured

- **Task**: the title of the Issue that a merged PR closed (`TASKMODE=title`; `TASKMODE=full` = title + body).
- **Truth**: the `src/**/*.rs` files that PR changed. The candidate PRs are fixed in `cands-<repo>.json` (re-fetching changes the set; `fetch_cands.py` shows how they were collected).
- **Index**: rebuilt from scratch at the PR's **base** commit for every task, so there is no leakage from the fix and no state carried between tasks.
- **Baseline `kw`**: score each file by the number of distinct task words it contains plus 3 x the number of those words in its path; take files in that order, skipping any that no longer fit. Untuned. Definition: the header of `bench.py`.
- **Cost**: every method is charged `bytes / 4` estimated tokens. Budgets 4,000 / 8,000 / 16,000.

## Two metrics

| `METRIC` | a file counts as found when | use |
|---|---|---|
| `strict` (default) | at least 50% of its content is in the bundle | **the README numbers** |
| `lenient` | the bundle lists it at all, even if truncated to a few tokens | tends to flatter `cs`: `kw` can only include whole files |

Numbers quoted without a metric in older #135 comments before 2026-10-31 12:45 are `lenient`.

## Run

Needs Python 3, `git`, `cargo`. Use a **dedicated** clone per repo: the script runs `git checkout -f` and writes `.contextsmith/` in it.

```bash
cargo build --release                         # from the repo root -> target/release/contextsmith
git clone --filter=blob:none https://github.com/tokio-rs/axum.git /tmp/bench/axum
git clone --filter=blob:none https://github.com/clap-rs/clap.git  /tmp/bench/clap
git clone --filter=blob:none https://github.com/sharkdp/bat.git   /tmp/bench/bat
git clone --filter=blob:none https://github.com/BurntSushi/ripgrep.git /tmp/bench/ripgrep
git clone --filter=blob:none https://github.com/sharkdp/fd.git /tmp/bench/fd

REPO=/tmp/bench/axum TASKMODE=title python3 bench/bench.py
REPO=/tmp/bench/clap TASKMODE=title python3 bench/bench.py
REPO=/tmp/bench/bat  TASKMODE=title python3 bench/bench.py
REPO=/tmp/bench/ripgrep CANDS=bench/cands-ripgrep.json TASKMODE=title python3 bench/bench.py
REPO=/tmp/bench/fd CANDS=bench/cands-fd.json TASKMODE=title python3 bench/bench.py
```

Options: `CSBIN` (binary path), `CANDS` (candidate file), `METRIC=lenient`, optional argument `N` = use only the first N candidates. To use your own candidates: `python3 bench/fetch_cands.py OWNER NAME OUT.json [rust|python|typescript]` (the language decides which changed files count as source), then `CANDS=OUT.json`. The candidate file is looked up as `cands-<repo directory name>.json`, so for the other languages below use the repo name as the clone directory.

## Expected (README table, `strict`, title only; recall averaged over tasks)

| repo (tasks) | budget | context-smith | naive keyword match |
|---|---|---|---|
| axum (54) | 4k / 8k / 16k | 0.45 / 0.54 / 0.67 | 0.17 / 0.49 / 0.56 |
| clap (60) | 4k / 8k / 16k | 0.23 / 0.33 / 0.48 | 0.16 / 0.27 / 0.38 |
| bat (56) | 4k / 8k / 16k | 0.25 / 0.35 / 0.48 | 0.18 / 0.24 / 0.42 |
| ripgrep (26) | 4k / 8k / 16k | 0.15 / 0.22 / 0.49 | 0.08 / 0.19 / 0.27 |
| fd (74) | 4k / 8k / 16k | 0.37 / 0.48 / 0.60 | 0.25 / 0.37 / 0.69 |

These are the numbers of `master` after #142 (budget allocation), #143 (BFS neighbour scores), #151 (symbols scored per file, weight 0.5) and #162 (a file of which at least half fits is truncated, not skipped). Before #162 (#142 + #143 + #151) they were axum 0.22 / 0.52 / 0.67, clap 0.21 / 0.29 / 0.44, bat 0.16 / 0.30 / 0.53, ripgrep 0.04 / 0.17 / 0.22, fd 0.21 / 0.48 / 0.61. Before those fixes the same benchmark gave axum 0.23 / 0.33 / 0.45, clap 0.08 / 0.15 / 0.28, bat 0.08 / 0.08 / 0.10 (this is also what the 0.4.0 release produces).

If you only want the current ceiling to compare against: with the true files known, whole files that fit into 4k / 8k / 16k are axum 0.50 / 0.86 / 0.97, clap 0.55 / 0.73 / 0.87, bat 0.49 / 0.85 / 0.99.

## Other languages (Python, TypeScript; added 2026-11-01, #148)

None of these four repositories was used to choose any parameter (the symbol weight was chosen on axum, clap and bat), so this is the closer test of how the numbers carry over. Same method: title-only tasks, `strict` metric, index rebuilt at each PR's base commit, the first 60 candidates (most recently updated merged PRs that close exactly one issue and change 1-4 source files). Truth = the changed source files: `*.py` (not tests, docs, examples, benchmarks, scripts, `conftest.py`, `setup.py`) and `*.ts` / `*.tsx` (not `*.d.ts`, `*.test.ts`, `*.spec.ts`, test or docs directories).

| repo (language, tasks) | budget | context-smith | naive keyword match | ceiling |
|---|---|---|---|---|
| scrapy (Python, 59) | 4k / 8k / 16k | 0.22 / 0.32 / 0.37 | 0.19 / 0.29 / 0.47 | 0.47 / 0.80 / 0.97 |
| poetry (Python, 60) | 4k / 8k / 16k | 0.18 / 0.22 / 0.19 | 0.19 / 0.31 / 0.40 | 0.51 / 0.86 / 0.99 |
| hono (TypeScript, 60) | 4k / 8k / 16k | 0.58 / 0.70 / 0.68 | 0.35 / 0.56 / 0.66 | 0.80 / 1.00 / 1.00 |
| zod (TypeScript, 60) | 4k / 8k / 16k | 0.20 / 0.28 / 0.34 | 0.04 / 0.07 / 0.18 | 0.28 / 0.53 / 0.68 |

Measured once each on master `aa4ef89` (#142 + #143 + #151 + #162). Before #162 (master `1e21261`) the four rows were scrapy 0.23 / 0.29 / 0.38, poetry 0.10 / 0.20 / 0.24, hono 0.45 / 0.65 / 0.74, zod 0.07 / 0.27 / 0.34. TypeScript is ahead of the baseline in both repositories at every budget (hono's 0.02 at 16k is noise). **Python is behind it at 16k in both** (by 0.10 on scrapy, 0.21 on poetry; poetry also at 8k by 0.09, and level at 4k). #162 raised 4k (+0.08 to +0.13 on poetry, hono and zod; -0.01 on scrapy) but lowered 16k on poetry (-0.05) and hono (-0.06), and on those two recall at 16k is now **below** recall at 8k, which should not happen when the budget grows. Hypothesis, not verified: at 16k a large, irrelevant file now has at least half of it fit and uses up the remaining budget, which at 8k it was skipped for. One task is worth about 0.02.

Where the Python gap comes from, from a one-off measurement made before #162 (the true files' rank in the unlimited-budget result, dependency expansion on; the script is not part of `bench/`, so this paragraph cannot be reproduced from this repository yet): the unlimited-budget result has a median of 108 files on poetry and 457 on scrapy (the search itself seeds at most 20, so dependency expansion adds the rest), against 245 on hono. On poetry the true files' recall at rank 10 is 0.25 for context-smith against 0.46 for the baseline, and 55% of the true files are not returned at all; on scrapy the figures are 0.43 against 0.55 at rank 10. On hono the ranking is better than the baseline (rank 1: 0.42 against 0.28). So for Python the ranking itself is weaker, unlike for Rust where the budget allocation was the main loss. The cause inside the ranking (path/identifier tokenisation, symbol extraction, the dependency graph) is not yet identified.

Speed on these repositories (about 390-510 indexed files): index 0.9-1.6 s median (2.1 s max), query 0.04-0.07 s.

## Verified

The first verification, on 2026-10-31 (master `239a17b`, i.e. before #142 and #143; binary built with `cargo build --release`, three repos run in parallel): **all 18 cells of the then-current table matched exactly** (two decimals). The current table was measured on 2026-11-01 with a binary built from master `6aea5f2` (#142 + #143 + #151), run twice per repo with identical results. Counts: axum 54 measured (55 candidates, 1 skipped by the script: checkout failed, or no truth file is in the index at the base commit), clap 60 (`cands-clap.json` holds the 60 candidates the README used; the fetch returned more), bat 56. Runtime about 1.5-2 minutes per repo (84 s / 124 s / 120 s) plus 1.2 minutes to build; the clones are the larger cost. Re-run twice (axum 2x, clap 2x, bat 2x): recall was identical every time; the reported `cost` of context-smith on bat differed slightly between the two runs (e.g. 14585 vs 14539 at 16k). The cause was the order of equal-score files, which came from a hash map; #142 breaks ties by path, and after it two runs were identical in recall and cost. The index is rebuilt per task and the random baseline is seeded.

ripgrep (26 candidates, added 2026-11-01) was measured on master `ffc78b0` (#142 + #143 + #151 + #156), run twice with identical results: 26 measured, 0 skipped, 0 `cs` query errors. It is the repository the fixes were *not* developed on; see #135 for stage-by-stage numbers (A: before the series, B: symbols only, C: #142 + #143 only, D: current).

On 2026-11-01, on master `aa4ef89` (#142 + #143 + #151 + #162): axum, clap, bat, ripgrep and fd reproduce the numbers measured for #162 with a binary built from the PR branch (two measurements, identical to two decimals). fd (74 candidates, added here; 0 skipped, 0 `cs` query errors) was not used to choose any parameter. scrapy, poetry, hono and zod were measured once (59 / 60 / 60 / 60 tasks; 0 query errors).

The Issue bodies in `cands-*.json` have third-party home-directory paths replaced with `/home/USER` (or `/Users/USER`) before committing; titles and truth files are untouched, so the `title` numbers above are unaffected. The `full` numbers quoted in #135 were measured before this and were not re-run.

The Python and TypeScript candidate files carry **no Issue bodies**: they are third-party text, `gitleaks` flagged a pasted WebSocket handshake key and (through one keyword) every commit SHA in `cands-hono.json`, and only `TASKMODE=full` needs them. `fetch_cands.py` now leaves bodies out unless `--with-body` is given (do not commit those files), and `bench.py` stops with a message if `TASKMODE=full` is asked of a file without bodies. The Rust files still contain bodies, so `TASKMODE=full` works for them only.

The `full` (title + body) task mode is available but is not what the README table reports.
