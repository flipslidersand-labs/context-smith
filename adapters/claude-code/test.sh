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

printf '#!/bin/sh\necho "contextsmith 0.4.0"\n' > "$tmp/old"; chmod +x "$tmp/old"
CONTEXTSMITH_BIN=$tmp/old "$sel" "$repo" invoice >/dev/null 2>&1; check "old version -> exit 3" "$?" 3
CONTEXTSMITH_BIN=$tmp/absent "$sel" "$repo" invoice >/dev/null 2>&1; check "missing binary -> exit 2" "$?" 2

exit $fail
