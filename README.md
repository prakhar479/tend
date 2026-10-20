# tend

Local supervisor for command-line coding agents. It removes noise from tool output, finds loops and test weakening, and records every action with a reason.

Design: [`docs/tend-reference.md`](docs/tend-reference.md). Work items: [`docs/TASKS.md`](docs/TASKS.md). Rules for Claude Code: [`CLAUDE.md`](CLAUDE.md).

## Status

Prototype. Claude Code only (tested with 2.1.303). Look mode, the default, records only. Optimize mode summarizes test, build, and install output, guards test edits, and steers loops. Linux and WSL are tested; macOS is not yet (N15). Windows is not supported.

## Install

```sh
cargo install --path crates/tend-cli   # Rust 1.87 or newer; installs `tend` into ~/.cargo/bin
```

## Use

```sh
tend claude                          # look mode: record only
tend claude --tend-mode optimize     # apply actions
tend claude --resume                 # other arguments (and --help) go to claude

tend report                          # newest session started in this directory
tend report --session -2 --full      # the one before it, every section
tend sessions                        # list: start, duration, mode, calls, project
tend show o-3                        # full original of a summarized output
tend why a-2                         # cause chain of an action
tend mode optimize                   # change the mode of the live session
tend allow                           # lift one test-guard block (run it yourself: `! tend allow`)
tend flag a-2 "summary hid the error" # note that tend got something wrong (a-N, o-N, or session)
tend review                          # all flags across sessions, with what they point at
tend retro ~/.claude/projects/<project>/<session>.jsonl   # report on a past session
tend retro --all                     # every past session and subagent, ranked
tend status                          # one line for the status line (empty outside tend)
tend doctor                          # check the install: binary, Claude Code version, data dir, config
tend audit main...HEAD               # experimental: what a diff did to tests and checks (--format markdown|json)
```

`--session` takes an id or a unique prefix of one (as `tend sessions` prints it), `-1` for the newest, `-2`, and so on, or a path.

If tend cannot start (a bad data directory, a daemon that does not come up), `tend claude` says so and runs Claude Code without it. A hook never blocks the agent because of an error in tend.

To see tend in Claude Code's status line, add this to `~/.claude/settings.json`. It replaces any status line you already have. Outside a tend session it shows nothing.

```json
"statusLine": { "type": "command", "command": "tend status" }
```

Session data: `~/.local/share/tend/sessions/<id>/` (override with `TEND_HOME`). `tend claude` deletes sessions older than `keep_days` (default 30).

## Config

`~/.config/tend/config.toml` (or `TEND_CONFIG`), flat `key = value` lines:

```toml
shape_threshold_tokens = 2000   # summarize output above this size
max_summary_ratio = 0.7         # use a summary only if it is at most this share of the original
loop_threshold = 3              # fix attempts before an unchanged failure is a loop
tighten_after = 5               # summaries without a re-open before tend summarizes harder
idle_gap_minutes = 10           # loop time leaves out gaps longer than this
keep_days = 30                  # 0 keeps every session
experimental_audit = false      # audit each session's changes to tests and checks (under test)
```

### Audit (experimental)

`tend audit` reads a git diff, or a whole `tend claude` session with `experimental_audit = true`, and lists what changed in the project's checks:

- test files: assertions and tests before and after, and anything that may weaken them (removed tests, skips, looser tolerances, always-true asserts);
- snapshot and golden files that were rewritten;
- suppressions added (`# noqa`, `# type: ignore`, `eslint-disable`, `#[allow]`, …), and whether each one gives a reason;
- check config (CI workflows, coverage thresholds, strictness) and dependency files.

It only reports; it never blocks. It reads the working tree through a temporary index, so your index, branch, and files are untouched. The feature is under test: its output may change.

A bad line is skipped with a warning when `tend claude` starts; the other lines still apply.

## Environment

| Variable | Effect |
|---|---|
| `TEND_HOME` | Data directory |
| `TEND_CLAUDE_BIN` | Agent binary (default `claude`) |
| `TEND_HOOK_TIMEOUT_MS` | Shim timeout (default 250) |
| `TEND_CAPTURE=1` | Also write raw hook payloads to `hooks.jsonl` (they contain code; off by default) |
| `TEND_CONFIG` | Config file (default `~/.config/tend/config.toml`) |
