# tend (prototype)

Local supervisor for command-line coding agents. It removes noise from tool output, finds loops and test weakening, and records every action with a reason.

Design: [`docs/tend-reference.md`](docs/tend-reference.md). Work items: [`docs/TASKS.md`](docs/TASKS.md). Rules for Claude Code: [`CLAUDE.md`](CLAUDE.md).

## Status

Phase 0/1 starter. Claude Code only. Look mode is the default.

## Build

```sh
cargo build --release          # tested with Rust 1.91
export PATH="$PWD/target/release:$PATH"
```

## Use

```sh
tend claude                          # look mode: record only
tend claude --tend-mode optimize     # apply actions
tend claude --resume                 # other arguments go to claude

tend report                          # latest session
tend show o-3                        # full original of a summarized output
tend why a-2                         # cause chain of an action
tend mode optimize                   # change mode of the live session
tend retro ~/.claude/projects/<project>/<session>.jsonl   # report on a past session
tend retro --all                     # every past session, ranked
tend allow                           # lift one test-guard block (run it yourself: `! tend allow`)
tend flag a-2 "summary hid the error" # note that tend got something wrong (a-N, o-N, or session)
tend review                          # all flags across sessions, with what they point at
tend sessions                        # list; --prune <days> removes old ones
tend status                          # one line for the status line (empty outside tend)
```

To see tend in Claude Code's status line, add this to `~/.claude/settings.json`. It replaces any status line you already have. Outside a tend session it shows nothing.

```json
"statusLine": { "type": "command", "command": "tend status" }
```

Session data: `~/.local/share/tend/sessions/<id>/` (override with `TEND_HOME`).

## Environment

| Variable | Effect |
|---|---|
| `TEND_HOME` | Data directory |
| `TEND_CLAUDE_BIN` | Agent binary (default `claude`) |
| `TEND_HOOK_TIMEOUT_MS` | Shim timeout (default 250) |
| `TEND_CAPTURE=1` | Also write raw hook payloads to `hooks.jsonl` (they contain code; off by default) |
| `TEND_CONFIG` | Config file (default `~/.config/tend/config.toml`) |
