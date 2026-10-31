"""context-smith vs naive baselines on a public repo (PR-base snapshots, issue text as task).

usage: REPO=/path/to/clone [TASKMODE=title|full] [METRIC=strict|lenient] [CSBIN=...] bench.py [N]

Task    = the closed Issue's title (TASKMODE=title) or title+body (TASKMODE=full, default).
Truth   = the src .rs files the fixing PR changed (cands-<repo>.json, fixed; see fetch_cands.py).
Index   = rebuilt from scratch at the PR's BASE commit for every task (no leakage, no state carried over).
Methods = cs  : `contextsmith query` at the given token budget
          kw  : NAIVE BASELINE. Score a file by the number of distinct task words it contains
                (words = [a-z_][a-z0-9_]{2,}, lowercased) + 3 x the number of those words in its path;
                take files in score order and skip any that no longer fit the budget. Untuned.
          rnd : random files that fit the budget (seed 0)
Cost    = bytes/4 estimated tokens for every method, so all are charged the same way.

Two metrics for `cs` (kw/rnd only ever include whole files, so they are unaffected):
  strict  (default, the one the README reports): a file counts as found only if >= 50% of its
          content is in the bundle.
  lenient : any file the bundle mentions counts, even if it was truncated to a few tokens.
          This inflates cs against kw, which can only include files whole.
"""
import json, os, random, re, shutil, sqlite3, subprocess, sys, time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.environ["REPO"]  # a dedicated clone: this script runs `git checkout -f` and writes .contextsmith/ in it
CS = os.path.abspath(os.environ.get("CSBIN", os.path.join(HERE, "..", "target", "release", "contextsmith")))
BUDGETS = (4000, 8000, 16000)
MODE = os.environ.get("TASKMODE", "full")
METRIC = os.environ.get("METRIC", "strict")
LIMIT = int(sys.argv[1]) if len(sys.argv) > 1 else 10**9
random.seed(0)


def run(*a, **k):
    return subprocess.run(a, capture_output=True, text=True, **k)


def est(size):  # bytes -> tokens, same rule for every method
    return max(1, size // 4)


CANDS = os.environ.get("CANDS", os.path.join(HERE, f"cands-{os.path.basename(os.path.normpath(REPO))}.json"))
cands = json.load(open(CANDS))[:LIMIT]
res = {b: {m: [] for m in ("cs", "kw", "rnd")} for b in BUDGETS}
cost = {b: {m: [] for m in ("cs", "kw", "rnd")} for b in BUDGETS}
errs, skipped, t0, corpus = 0, 0, time.time(), []

for c in cands:
    r = run("git", "-C", REPO, "checkout", "-q", "-f", c["base"])
    if r.returncode:
        skipped += 1
        continue
    shutil.rmtree(f"{REPO}/.contextsmith", ignore_errors=True)  # fresh index per task: no state carried between tasks
    idx = run(CS, "index", "--repo", REPO)
    if idx.returncode:
        print("index failed", c["pr"], idx.stderr[:150])
        skipped += 1
        continue
    db = sqlite3.connect(f"{REPO}/.contextsmith/index.db")
    files = {p: s for p, s in db.execute("select path,size from files where lang!='unknown'")}
    db.close()
    gt = {g for g in c["gt"] if g in files}
    if not gt:
        skipped += 1
        continue
    corpus.append(sum(est(s) for s in files.values()))
    base = c["title"] if MODE.endswith("title") else (c["title"] + "\n" + c["body"]).strip()
    task = base
    if MODE.startswith("rare"):  # keep the K rarest task words (by # files containing them), K=8
        low = {}
        for p in files:
            try:
                low[p] = open(f"{REPO}/{p}", encoding="utf-8", errors="ignore").read().lower()
            except OSError:
                low[p] = ""
        ws = sorted({w for w in re.findall(r"[a-z_][a-z0-9_]{2,}", base.lower())})
        df = {w: sum(1 for t in low.values() if w in t) for w in ws}
        task = " ".join(sorted((w for w in ws if df[w] > 0), key=lambda w: df[w])[:8]) or base
    words = {w for w in re.findall(r"[a-z_][a-z0-9_]{2,}", task.lower())}
    score = {}
    for p in files:
        try:
            txt = open(f"{REPO}/{p}", encoding="utf-8", errors="ignore").read().lower()
        except OSError:
            txt = ""
        score[p] = sum(1 for w in words if w in txt) + 3 * sum(1 for w in words if w in p.lower())
    for b in BUDGETS:
        q = run(CS, "query", "--repo", REPO, "--task", task, "--format", "json", "--budget", str(b), *(["--top", os.environ["CSTOP"]] if os.environ.get("CSTOP") else []))
        try:
            _fs = json.loads(q.stdout)["files"]
            if METRIC == "strict":  # a file counts only if >=50% of its content is included
                _fs = [f for f in _fs if f["tokens"] >= 0.5 * est(files.get(f["path"], 0))]
            sel = [f["path"] for f in _fs]
        except Exception:
            errs += 1
            sel = []
        picks = {"cs": sel}
        for m, order in (("kw", sorted(files, key=lambda p: -score[p])), ("rnd", random.sample(sorted(files), len(files)))):
            acc, pk = 0, []
            for p in order:
                if acc + est(files[p]) <= b:
                    pk.append(p)
                    acc += est(files[p])
            picks[m] = pk
        for m, pk in picks.items():
            res[b][m].append(len(gt & set(pk)) / len(gt))
            cost[b][m].append(sum(est(files[p]) for p in pk if p in files))

n = len(res[BUDGETS[0]]["cs"])
print(f"repo={os.path.basename(os.path.normpath(REPO))} task={MODE} metric={METRIC}")
print(f"tasks measured: {n} (skipped {skipped}, cs query errors {errs}) in {time.time()-t0:.0f}s")
print(f"searchable corpus (avg est tokens): {sum(corpus)//max(1,len(corpus))}")
for b in BUDGETS:
    row = " | ".join(f"{m}: recall={sum(res[b][m])/n:.2f} cost={sum(cost[b][m])//n}" for m in ("cs", "kw", "rnd"))
    print(f"budget={b}: {row}")
