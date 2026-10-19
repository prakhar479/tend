# winnow (prototype)

Local supervisor for command-line coding agents. It removes noise from tool output, finds loops and test weakening, and records every action with a reason.

Design: [`docs/winnow-reference.md`](docs/winnow-reference.md). Work items: [`docs/TASKS.md`](docs/TASKS.md). Rules for Claude Code: [`CLAUDE.md`](CLAUDE.md).

## Status

Phase 0/1 starter. Claude Code only. Look mode is the default.

## Build

```sh
cargo build --release          # tested with Rust 1.91
export PATH="$PWD/target/release:$PATH"
```

## Use

```sh
winnow claude                          # look mode: record only
winnow claude --winnow-mode optimize   # apply actions
winnow claude --resume                 # other arguments go to claude

winnow report                          # latest session
winnow show o-3                        # full original of a summarized output
winnow why a-2                         # cause chain of an action
winnow mode optimize                   # change mode of the live session
winnow retro ~/.claude/projects/<project>/<session>.jsonl   # report on a past session
winnow sessions
```

Session data: `~/.local/share/winnow/sessions/<id>/` (override with `WINNOW_HOME`).

## Environment

| Variable | Effect |
|---|---|
| `WINNOW_HOME` | Data directory |
| `WINNOW_CLAUDE_BIN` | Agent binary (default `claude`) |
| `WINNOW_HOOK_TIMEOUT_MS` | Shim timeout (default 250) |
| `WINNOW_CAPTURE=0` | Do not write raw hook payloads to `hooks.jsonl` |
