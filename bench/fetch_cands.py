"""usage: fetch_cands.py OWNER NAME OUT.json [LANG] [--with-body]

Merged PRs that close exactly one issue and change 1-4 source files of LANG (default: rust).
Truth = those changed source files; tests, docs and examples are not counted as source.

Issue bodies are third-party text (and the only place stray tokens or home paths show up), so
they are left out unless --with-body is given; only TASKMODE=full of bench.py needs them.

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


# A home-directory prefix followed by one path component. The prefix is /home/ or /Users/ (not
# anchored, so "/usr/home/x" is also scrubbed: over-scrubbing is the safe side for a privacy
# filter), or a Windows drive path C:\Users\ / C:/Users/ (any slash run, "Users" in any case).
# The name is any run of characters up to a path separator or whitespace, so non-ASCII names are
# covered. Both classes are flat single quantifiers, so matching stays linear on any input.
# A name with a space ("john smith") is cut at the space: where it ends cannot be told from the text.
_HOME_RE = re.compile(r"(?P<pre>/(?:home|Users)/|[A-Za-z]:[\\/]+(?i:users)[\\/]+)(?P<name>[^/\\\s]+)")
_TRAILING_PUNCT = ".,;:!?)]}'\"`>。、，！？）」』】"  # sentence punctuation after a name is not part of it


def private_paths_scrubbed(text):  # third-party home directories in issue bodies are not ours to republish
    def repl(m):
        name = m.group("name")
        kept = name.rstrip(_TRAILING_PUNCT)
        if not kept:  # nothing but punctuation ("/home/."), so there is no name to hide
            return m.group(0)
        return m.group("pre") + "USER" + name[len(kept):]

    return _HOME_RE.sub(repl, text)


def select(prs, lang, with_body=False):
    cands = []
    for n in prs:
        iss = n["closingIssuesReferences"]["nodes"]
        fs = [f["path"] for f in n["files"]["nodes"]]
        src = [f for f in fs if is_source(f, lang)]
        if len(iss) == 1 and 1 <= len(src) <= 4 and len(fs) < 30:
            c = {"pr": n["number"], "base": n["baseRefOid"], "issue": iss[0]["number"], "title": iss[0]["title"], "gt": src}
            if with_body:
                c["body"] = private_paths_scrubbed((iss[0]["body"] or "")[:1500])
            cands.append(c)
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
    pos = [a for a in sys.argv[1:] if not a.startswith("--")]
    owner, name, out = pos[:3]
    lang = pos[3] if len(pos) > 3 else "rust"
    if lang not in LANGS:
        sys.exit(f"unknown LANG {lang!r}; choose from {', '.join(LANGS)}")
    allp = fetch(owner, name)
    cands = select(allp, lang, "--with-body" in sys.argv)
    with open(out, "w") as f:  # one candidate per line keeps diffs and reviews small
        f.write("[\n" + ",\n".join(json.dumps(c, ensure_ascii=False) for c in cands) + "\n]\n")
    print(f"{owner}/{name} ({lang}): PRs fetched {len(allp)} | candidates {len(cands)}")
