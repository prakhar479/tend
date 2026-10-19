# rein (prototype)

Local supervisor for command-line coding agents. It removes noise from tool output, finds loops and test weakening, and records every action with a reason.

Design: [`docs/rein-reference.md`](docs/rein-reference.md). Work items: [`docs/TASKS.md`](docs/TASKS.md). Rules for Claude Code: [`CLAUDE.md`](CLAUDE.md).

## Status

Phase 0/1 starter. Claude Code only. Look mode is the default.

## Build

```sh
cargo build --release          # tested with Rust 1.91
export PATH="$PWD/target/release:$PATH"
```

## Use

```sh
rein claude                          # look mode: record only
rein claude --rein-mode optimize     # apply actions
rein claude --resume                 # other arguments go to claude

rein report                          # latest session
rein show o-3                        # full original of a summarized output
rein why a-2                         # cause chain of an action
rein mode optimize                   # change mode of the live session
rein retro ~/.claude/projects/<project>/<session>.jsonl   # report on a past session
rein retro --all                     # every past session, ranked
rein allow                           # lift one test-guard block (run it yourself: `! rein allow`)
rein flag a-2 "summary hid the error" # note that rein got something wrong (a-N, o-N, or session)
rein review                          # all flags across sessions, with what they point at
rein sessions                        # list; --prune <days> removes old ones
```

Session data: `~/.local/share/rein/sessions/<id>/` (override with `REIN_HOME`).

## Environment

| Variable | Effect |
|---|---|
| `REIN_HOME` | Data directory |
| `REIN_CLAUDE_BIN` | Agent binary (default `claude`) |
| `REIN_HOOK_TIMEOUT_MS` | Shim timeout (default 250) |
| `REIN_CAPTURE=1` | Also write raw hook payloads to `hooks.jsonl` (they contain code; off by default) |
| `REIN_CONFIG` | Config file (default `~/.config/rein/config.toml`) |
