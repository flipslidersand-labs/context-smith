"""usage: fetch_cands.py OWNER NAME OUT.json [LANG]

Merged PRs that close exactly one issue and change 1-4 source files of LANG (default: rust).
Truth = those changed source files; tests, docs and examples are not counted as source.

LANG is one of: rust, python, typescript. A file is source if it matches the first pattern
and not the second.
"""
import json, re, subprocess, sys

LANGS = {
    "rust": (r"(^|/)src/.*\.rs$", r"/tests/"),
    "python": (
        r"\.py$",
        r"(^|/)(tests?|testing|docs?|examples?|benchmarks?|scripts?)/|(^|/)test_[^/]*\.py$|_test\.py$|(^|/)(conftest|setup)\.py$",
    ),
    "typescript": (
        r"\.(ts|tsx)$",
        r"\.d\.ts$|\.(test|spec)\.tsx?$|(^|/)(test|tests|__tests__|docs?|examples?|benchmarks?|runtime-tests)/",
    ),
}


def is_source(path, lang):
    inc, exc = LANGS[lang]
    return bool(re.search(inc, path)) and not re.search(exc, path)


def private_paths_scrubbed(text):  # third-party home directories in issue bodies are not ours to republish
    return re.sub(r"/(?:home|Users)/[A-Za-z0-9_.-]+", lambda m: m.group(0).rsplit("/", 1)[0] + "/USER", text)


def select(prs, lang):
    cands = []
    for n in prs:
        iss = n["closingIssuesReferences"]["nodes"]
        fs = [f["path"] for f in n["files"]["nodes"]]
        src = [f for f in fs if is_source(f, lang)]
        if len(iss) == 1 and 1 <= len(src) <= 4 and len(fs) < 30:
            body = private_paths_scrubbed((iss[0]["body"] or "")[:1500])
            cands.append({"pr": n["number"], "base": n["baseRefOid"], "issue": iss[0]["number"], "title": iss[0]["title"], "body": body, "gt": src})
    return cands


def fetch(owner, name):
    q = '''query($c:String){repository(owner:"%s",name:"%s"){pullRequests(states:MERGED,first:100,after:$c,orderBy:{field:UPDATED_AT,direction:DESC}){pageInfo{hasNextPage endCursor}nodes{number baseRefOid closingIssuesReferences(first:2){nodes{number title body}}files(first:30){nodes{path}}}}}}''' % (owner, name)
    cur, allp = None, []
    for i in range(8):
        a = ["gh", "api", "graphql", "-f", "query=" + q] + (["-f", "c=" + cur] if cur else [])
        r = subprocess.run(a, capture_output=True, text=True)
        if not r.stdout.strip():
            print("EMPTY page", i, r.stderr[:200])
            break
        d = json.loads(r.stdout)["data"]["repository"]["pullRequests"]
        allp += d["nodes"]
        if not d["pageInfo"]["hasNextPage"]:
            break
        cur = d["pageInfo"]["endCursor"]
    return allp


if __name__ == "__main__":
    owner, name, out = sys.argv[1:4]
    lang = sys.argv[4] if len(sys.argv) > 4 else "rust"
    if lang not in LANGS:
        sys.exit(f"unknown LANG {lang!r}; choose from {', '.join(LANGS)}")
    allp = fetch(owner, name)
    cands = select(allp, lang)
    with open(out, "w") as f:  # one candidate per line keeps diffs and reviews small
        f.write("[\n" + ",\n".join(json.dumps(c, ensure_ascii=False) for c in cands) + "\n]\n")
    print(f"{owner}/{name} ({lang}): PRs fetched {len(allp)} | candidates {len(cands)}")
