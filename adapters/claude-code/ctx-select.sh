#!/usr/bin/env bash
# ctx-select: list the files of a git repository that context-smith ranks as most relevant to a task.
# The index lives in a cache directory, so the target repository is never written to.
#
# usage: ctx-select.sh [--budget N] [--top N] <repo> <task words...>
# exit:  0 ok | 2 usage or contextsmith missing | 3 contextsmith older than $MIN_VERSION | 4 no matching files
set -euo pipefail

MIN_VERSION=0.4.1
BIN=${CONTEXTSMITH_BIN:-contextsmith}
CACHE_ROOT=${CTX_SELECT_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/contextsmith}
budget=16000
top=15

die() { echo "ctx-select: $2" >&2; exit "$1"; }

while [ $# -gt 0 ]; do
  case $1 in
    --budget) budget=${2:?--budget needs a number}; shift 2 ;;
    --top) top=${2:?--top needs a number}; shift 2 ;;
    -h|--help) sed -n '2,6p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    --) shift; break ;;
    *) break ;;
  esac
done
[ $# -ge 2 ] || die 2 "usage: ctx-select.sh [--budget N] [--top N] <repo> <task words...>"
repo_arg=$1; shift
task="$*"

command -v "$BIN" >/dev/null 2>&1 || die 2 "'$BIN' not found; install it with: cargo install context-smith (or set CONTEXTSMITH_BIN)"
version=$("$BIN" --version | awk '{print $2}')
if [ "$(printf '%s\n%s\n' "$MIN_VERSION" "$version" | sort -V | head -n1)" != "$MIN_VERSION" ]; then
  die 3 "contextsmith $version is too old (need >= $MIN_VERSION: earlier versions rank poorly and can fail on multi-word tasks); update with: cargo install context-smith"
fi

repo=$(git -C "$repo_arg" rev-parse --show-toplevel 2>/dev/null) || die 2 "$repo_arg is not inside a git repository"
key=$(printf '%s' "$repo" | sha1sum | cut -c1-16)
cache="$CACHE_ROOT/$key"
# The cache holds a copy of the repository's text: keep it private to this user (umask also
# covers index.db and the logs), and refuse a symlink or another user's directory.
umask 077
mkdir -p "$cache"
{ [ ! -L "$cache" ] && [ -O "$cache" ]; } || die 2 "$cache is a symlink or owned by another user; refusing to use it"
chmod 700 "$cache"
printf '%s\n' "$repo" > "$cache/repo.txt"

# Incremental: unchanged files are skipped, so repeated calls are cheap.
if ! "$BIN" index --repo "$repo" --out "$cache/index.db" >"$cache/index.log" 2>&1; then
  cat "$cache/index.log" >&2
  die 2 "indexing failed"
fi

if ! out=$("$BIN" query --repo "$repo" --index "$cache/index.db" --task "$task" --budget "$budget" --format json 2>"$cache/query.log"); then
  grep -q "No matching files" "$cache/query.log" && die 4 "no matching files for: $task"
  cat "$cache/query.log" >&2
  die 2 "query failed"
fi

printf '%s' "$out" | python3 -c '
import json, sys
top = int(sys.argv[1])
d = json.load(sys.stdin)
files = d["files"]
print("# %d files within %d tokens; showing %d. rank  score  tokens  path" % (len(files), d["budget"], min(top, len(files))))
for i, f in enumerate(files[:top], 1):
    print("%4d  %5.2f  %6d  %s" % (i, f["score"], f["tokens"], f["path"]))
' "$top"
