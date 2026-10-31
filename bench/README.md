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

REPO=/tmp/bench/axum TASKMODE=title python3 bench/bench.py
REPO=/tmp/bench/clap TASKMODE=title python3 bench/bench.py
REPO=/tmp/bench/bat  TASKMODE=title python3 bench/bench.py
```

Options: `CSBIN` (binary path), `CANDS` (candidate file), `METRIC=lenient`, optional argument `N` = use only the first N candidates. To use your own candidates: `python3 bench/fetch_cands.py OWNER NAME OUT.json`, then `CANDS=OUT.json`.

## Expected (README table, `strict`, title only; recall averaged over tasks)

| repo (tasks) | budget | context-smith | naive keyword match |
|---|---|---|---|
| axum (54) | 4k / 8k / 16k | 0.22 / 0.40 / 0.46 | 0.17 / 0.49 / 0.56 |
| clap (60) | 4k / 8k / 16k | 0.13 / 0.28 / 0.38 | 0.16 / 0.27 / 0.38 |
| bat (56) | 4k / 8k / 16k | 0.13 / 0.30 / 0.41 | 0.18 / 0.24 / 0.42 |

These are the numbers of `master` after #142 (budget allocation) and #143 (BFS neighbour scores). Before those two fixes the same benchmark gave axum 0.23 / 0.33 / 0.45, clap 0.08 / 0.15 / 0.28, bat 0.08 / 0.08 / 0.10 (this is also what the 0.4.0 release produces).

If you only want the current ceiling to compare against: with the true files known, whole files that fit into 4k / 8k / 16k are axum 0.50 / 0.86 / 0.97, clap 0.55 / 0.73 / 0.87, bat 0.49 / 0.85 / 0.99.

## Verified

The first verification, on 2026-10-31 (master `239a17b`, i.e. before #142 and #143; binary built with `cargo build --release`, three repos run in parallel): **all 18 cells of the then-current table matched exactly** (two decimals). The current table was measured on 2026-11-01 with a binary built from #142 + #143, run twice per repo with identical results. Counts: axum 54 measured (55 candidates, 1 skipped by the script: checkout failed, or no truth file is in the index at the base commit), clap 60 (`cands-clap.json` holds the 60 candidates the README used; the fetch returned more), bat 56. Runtime about 1.5-2 minutes per repo (84 s / 124 s / 120 s) plus 1.2 minutes to build; the clones are the larger cost. Re-run twice (axum 2x, clap 2x, bat 2x): recall was identical every time; the reported `cost` of context-smith on bat differed slightly between the two runs (e.g. 14585 vs 14539 at 16k). The cause was the order of equal-score files, which came from a hash map; #142 breaks ties by path, and after it two runs were identical in recall and cost. The index is rebuilt per task and the random baseline is seeded.

The Issue bodies in `cands-*.json` have third-party home-directory paths replaced with `/home/USER` (or `/Users/USER`) before committing; titles and truth files are untouched, so the `title` numbers above are unaffected. The `full` numbers quoted in #135 were measured before this and were not re-run.

The `full` (title + body) task mode is available but is not what the README table reports.
