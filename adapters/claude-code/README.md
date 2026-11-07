# Claude Code adapter

A skill (`SKILL.md`) and a thin wrapper (`ctx-select.sh`) that let Claude Code ask context-smith for the files most relevant to a task in any git repository.

## Install

Needs `contextsmith` >= 0.4.1 on `PATH` (`cargo install context-smith`; until 0.4.1 is published, build this repository with `cargo build --release` and set `CONTEXTSMITH_BIN`), `git`, `python3`.

```bash
mkdir -p ~/.claude/skills/ctx-select
cp adapters/claude-code/SKILL.md adapters/claude-code/ctx-select.sh ~/.claude/skills/ctx-select/
chmod +x ~/.claude/skills/ctx-select/ctx-select.sh
```

Try it on any repository:

```bash
~/.claude/skills/ctx-select/ctx-select.sh ~/some/repo "what you are working on"
```

## Test

```bash
bash adapters/claude-code/test.sh      # builds a throwaway git repository; checks ranking, exit codes, and that the target repository stays untouched
```

## Environment

| Variable | Default | Use |
|---|---|---|
| `CONTEXTSMITH_BIN` | `contextsmith` | path to the binary |
| `CTX_SELECT_CACHE` | `${XDG_CACHE_HOME:-~/.cache}/contextsmith` | where indexes are kept (one directory per repository; created mode 700 because the index holds file text, and a symlink or another user's directory is refused) |
