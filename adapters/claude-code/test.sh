#!/usr/bin/env bash
# Tests for ctx-select.sh. Needs a contextsmith >= 0.4.1 on PATH (or CONTEXTSMITH_BIN).
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
sel=$here/ctx-select.sh
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
export CTX_SELECT_CACHE=$tmp/cache
fail=0
check() { if [ "$2" = "$3" ]; then echo "ok   - $1"; else echo "FAIL - $1 (expected [$3], got [$2])"; fail=1; fi; }

# A tiny git repository with distinct vocabulary per file.
repo=$tmp/repo; mkdir -p "$repo/src"
git -C "$repo" init -q
cat > "$repo/src/billing.py" <<'PY'
def compute_invoice_total(items):
    """Sum invoice line items and apply the tax rate."""
    return sum(i.price for i in items)
PY
cat > "$repo/src/network.py" <<'PY'
def open_socket(host, port):
    """Open a TCP socket with a retry on timeout."""
    return (host, port)
PY
cat > "$repo/src/render.py" <<'PY'
def draw_chart(values):
    """Render a bar chart."""
    return values
PY
git -C "$repo" add src/billing.py src/network.py src/render.py && git -C "$repo" -c user.name=t -c user.email=t@example.invalid commit -q -m init

out=$("$sel" "$repo" invoice tax total 2>&1); code=$?
check "exit code 0" "$code" 0
check "billing.py ranks first" "$(printf '%s\n' "$out" | sed -n '2p' | awk '{print $4}')" "src/billing.py"
check "target repo is untouched" "$(git -C "$repo" status --porcelain)" ""
check "no index inside the repo" "$(ls -A "$repo" | grep -c -E 'contextsmith|bundle' || true)" "0"

out2=$("$sel" --top 1 "$repo" socket retry timeout 2>&1); code=$?
check "second query exit code 0 (reuses the cache)" "$code" 0
check "--top 1 prints header plus one row" "$(printf '%s\n' "$out2" | wc -l | tr -d ' ')" "2"
check "network.py ranks first" "$(printf '%s\n' "$out2" | sed -n '2p' | awk '{print $4}')" "src/network.py"

"$sel" "$repo" zzzqqqxxx >/dev/null 2>&1; check "no match -> exit 4" "$?" 4
"$sel" "$tmp" something else >/dev/null 2>&1; check "not a git repo -> exit 2" "$?" 2
"$sel" "$repo" >/dev/null 2>&1; check "missing task -> exit 2" "$?" 2
"$sel" --top abc "$repo" invoice >/dev/null 2>&1; check "non-numeric --top -> exit 2" "$?" 2
"$sel" --top -1 "$repo" invoice >/dev/null 2>&1; check "negative --top -> exit 2" "$?" 2
"$sel" --budget 1x "$repo" invoice >/dev/null 2>&1; check "non-numeric --budget -> exit 2" "$?" 2

printf '#!/bin/sh\necho "contextsmith 0.4.0"\n' > "$tmp/old"; chmod +x "$tmp/old"
CONTEXTSMITH_BIN=$tmp/old "$sel" "$repo" invoice >/dev/null 2>&1; check "old version -> exit 3" "$?" 3
CONTEXTSMITH_BIN=$tmp/absent "$sel" "$repo" invoice >/dev/null 2>&1; check "missing binary -> exit 2" "$?" 2

# Option parsing.
out=$("$sel" --help 2>&1); code=$?
check "--help -> exit 0" "$code" 0
check "--help prints usage" "$(printf '%s\n' "$out" | grep -c '^usage: ctx-select.sh')" "1"
"$sel" -h >/dev/null 2>&1; check "-h -> exit 0" "$?" 0
"$sel" "$repo" invoice --budget >/dev/null 2>&1; check "task words with trailing --budget are taken as task, not option" "$?" 0
# A missing option value is a usage error and exits 2 (the documented code).
"$sel" --budget >/dev/null 2>&1; check "--budget without a value -> exit 2" "$?" 2
"$sel" --top >/dev/null 2>&1; check "--top without a value -> exit 2" "$?" 2
out=$("$sel" --top 0 "$repo" invoice tax total 2>&1); code=$?
check "--top 0 -> exit 0" "$code" 0
check "--top 0 prints the header only" "$(printf '%s\n' "$out" | wc -l | tr -d ' ')" "1"
check "--top 0 header says showing 0" "$(printf '%s\n' "$out" | grep -c 'showing 0\.')" "1"
"$sel" -- "$repo" invoice tax >/dev/null 2>&1; check "-- ends option parsing -> exit 0" "$?" 0
"$sel" --top 1 -- "$repo" invoice tax >/dev/null 2>&1; check "option then -- -> exit 0" "$?" 0

# Stub contextsmith: version from $1, behaviour of index/query from $2/$3 (ok|fail).
mkstub() { # path version index_mode query_mode
  cat > "$1" <<STUB
#!/bin/sh
case \$1 in
  --version) echo "contextsmith $2" ;;
  index) [ "$3" = ok ] || { echo "stub index boom" >&2; exit 1; } ;;
  query) [ "$4" = ok ] || { echo "stub query boom" >&2; exit 1; }
         echo '{"budget":100,"files":[]}' ;;
esac
STUB
  chmod +x "$1"
}
mkstub "$tmp/idxfail" 0.4.1 fail ok
out=$(CONTEXTSMITH_BIN=$tmp/idxfail "$sel" "$repo" invoice 2>&1); code=$?
check "index failure -> exit 2" "$code" 2
check "index failure reports 'indexing failed'" "$(printf '%s\n' "$out" | grep -c 'indexing failed')" "1"
mkstub "$tmp/qfail" 0.4.1 ok fail
out=$(CONTEXTSMITH_BIN=$tmp/qfail "$sel" "$repo" invoice 2>&1); code=$?
check "other query failure -> exit 2" "$code" 2
check "query failure reports 'query failed'" "$(printf '%s\n' "$out" | grep -c 'query failed')" "1"
check "query failure shows the query's stderr" "$(printf '%s\n' "$out" | grep -c 'stub query boom')" "1"

# Version boundaries (compared with sort -V).
# Pre-release suffixes sort after the bare version under sort -V, so 0.4.1-rc1 is accepted (pinned, not endorsed).
for v in 0.4.1 0.10.0 1.0.0 0.5.0-rc1 0.4.1-rc1; do
  mkstub "$tmp/ver" "$v" ok ok
  CONTEXTSMITH_BIN=$tmp/ver "$sel" "$repo" invoice >/dev/null 2>&1; check "version $v is accepted" "$?" 0
done
for v in 0.4.0 0.3.9; do
  mkstub "$tmp/ver" "$v" ok ok
  CONTEXTSMITH_BIN=$tmp/ver "$sel" "$repo" invoice >/dev/null 2>&1; check "version $v -> exit 3" "$?" 3
done

# The cache holds a copy of the repository's text, so it must not be readable by other users,
# whatever the caller's umask is.
perm() { stat -c %a "$1"; }
umask 022
rm -rf "$tmp/cache"
"$sel" "$repo" invoice tax >/dev/null 2>&1
cdir=$(dirname "$(ls "$tmp"/cache/*/index.db)")
check "cache dir is 700 under umask 022" "$(perm "$cdir")" 700
check "index.db is 600 under umask 022" "$(perm "$cdir/index.db")" 600
chmod 755 "$cdir"
"$sel" "$repo" invoice tax >/dev/null 2>&1
check "an existing 755 cache dir is tightened to 700" "$(perm "$cdir")" 700

# A cache dir that is a symlink (planted by someone else) is refused.
key_link=$tmp/cache2; mkdir -p "$tmp/target-dir" "$key_link"
ln -s "$tmp/target-dir" "$key_link/$(basename "$cdir")"
CTX_SELECT_CACHE=$key_link "$sel" "$repo" invoice tax >/dev/null 2>&1; check "symlinked cache dir -> exit 2" "$?" 2
check "nothing written through the symlink" "$(ls -A "$tmp/target-dir" | wc -l | tr -d ' ')" "0"

# cwd must not influence which python modules ctx-select imports (a json.py in an untrusted
# working directory must not run).
evil=$tmp/evil; mkdir -p "$evil"
printf 'import pathlib\npathlib.Path(%s).write_text("pwned")\n' "'$tmp/marker'" > "$evil/json.py"
(cd "$evil" && "$sel" "$repo" invoice tax >/dev/null 2>&1)
check "a json.py in the cwd is not imported" "$(ls "$tmp/marker" 2>/dev/null | wc -l | tr -d ' ')" "0"

# A file name with a newline must not be able to forge extra rows in the output.
nl=$tmp/nlrepo; mkdir -p "$nl/src"
git -C "$nl" init -q
nlname=$(printf 'src/zebra\n   1  9.99  999  forged.rs')
printf 'def quokka_zebra():\n    """zebra quokka marmot."""\n' > "$nl/$nlname"
git -C "$nl" add -- "$nlname" && git -C "$nl" -c user.name=t -c user.email=t@example.invalid commit -q -m init
out3=$("$sel" "$nl" zebra quokka marmot 2>&1); code=$?
check "newline file name: exit code 0" "$code" 0
check "newline file name is printed on one row" "$(printf '%s\n' "$out3" | wc -l | tr -d ' ')" "2"
check "newline is replaced by ?" "$(printf '%s\n' "$out3" | sed -n '2p' | cut -c22-)" "src/zebra?   1  9.99  999  forged.rs"

exit $fail
