"""usage: fetch_cands.py OWNER NAME OUT.json -- merged PRs that close exactly one issue and change 1-4 src .rs files."""
import json, re, subprocess, sys
owner, name, out = sys.argv[1:4]
Q = '''query($c:String){repository(owner:"%s",name:"%s"){pullRequests(states:MERGED,first:100,after:$c,orderBy:{field:UPDATED_AT,direction:DESC}){pageInfo{hasNextPage endCursor}nodes{number baseRefOid closingIssuesReferences(first:2){nodes{number title body}}files(first:30){nodes{path}}}}}}''' % (owner, name)
cur, allp = None, []
for i in range(8):
    a = ["gh", "api", "graphql", "-f", "query=" + Q] + (["-f", "c=" + cur] if cur else [])
    r = subprocess.run(a, capture_output=True, text=True)
    if not r.stdout.strip():
        print("EMPTY page", i, r.stderr[:200]); break
    d = json.loads(r.stdout)["data"]["repository"]["pullRequests"]; allp += d["nodes"]
    if not d["pageInfo"]["hasNextPage"]: break
    cur = d["pageInfo"]["endCursor"]
cands = []
for n in allp:
    iss = n["closingIssuesReferences"]["nodes"]; fs = [f["path"] for f in n["files"]["nodes"]]
    src = [f for f in fs if re.search(r"(^|/)src/.*\.rs$", f) and "/tests/" not in f]
    if len(iss) == 1 and 1 <= len(src) <= 4 and len(fs) < 30:
        cands.append({"pr": n["number"], "base": n["baseRefOid"], "issue": iss[0]["number"], "title": iss[0]["title"], "body": (iss[0]["body"] or "")[:1500], "gt": src})
json.dump(cands, open(out, "w"), ensure_ascii=False, indent=1)
print(f"{owner}/{name}: PRs fetched {len(allp)} | candidates {len(cands)}")
