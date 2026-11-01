---
name: ctx-select
description: Lists the files of a git repository that context-smith ranks as most relevant to a task, using BM25 and the dependency graph. Use it at the start of work in an unfamiliar repository or a large one, when you do not yet know which files to read for a bug or a feature. It reads the repository and writes nothing to it.
---

# ctx-select

Ask context-smith which files to read first.

```bash
~/.claude/skills/ctx-select/ctx-select.sh [--budget N] [--top N] <repo> <task words...>
```

- `<task words...>`: describe the task the way an issue title would, e.g. `poetry source add fails with KeyError`. Words that name a module or a class help most.
- `--budget` (default 16000 tokens) bounds the total size of the files listed; `--top` (default 15) limits the rows printed.
- Output: one row per file, `rank  score  tokens  path`. Read the top few, then use your normal search for the rest.
- The index is kept in `~/.cache/contextsmith/`; the repository is never written to. The first call on a repository takes about a second per 500 files; later calls are incremental.

Exit codes: 0 ok, 2 usage error or contextsmith missing, 3 contextsmith older than 0.4.1, 4 no file matches the words.

## What to expect (measured, not promised)

- It finds the files a real fix touched about as often as, or better than, a plain keyword match over the same words: on 12 public repositories (Rust, Python, TypeScript) its recall at a 16k budget was higher on 11 and lower on 1 (poetry), by 0.01 to 0.22. Recall is 0.3 to 0.8 in absolute terms, so it misses files, especially ones the task does not name (settings, shared helpers).
- It does not read the task semantically: a file that never contains the task's words is not found.
- Test and fixture files rank at half weight, not excluded. The benchmark counts only source files as correct, so it favours that choice; whether it helps real work is not measured.
- Whether the list makes an AI's answers better has not been measured.

Treat the list as a starting point, not as the answer.
