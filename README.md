# tend

Local supervisor for command-line coding agents. It removes noise from tool output, finds loops and test weakening, and records every action with a reason.

Design: [`docs/tend-reference.md`](docs/tend-reference.md). Work items: [`docs/TASKS.md`](docs/TASKS.md). Rules for Claude Code: [`CLAUDE.md`](CLAUDE.md).

## Status

Prototype. Claude Code only (tested with 2.1.303). Look mode, the default, records only. Optimize mode summarizes test, build, and install output, guards test edits, and steers loops. Linux and WSL are tested; macOS is not yet (N15). Windows is not supported.

## Install

Prebuilt (Linux x86_64/aarch64, macOS arm64/x86_64), from the [releases](https://github.com/prakhar479/tend/releases):

```sh
curl -L https://github.com/prakhar479/tend/releases/latest/download/tend-<version>-<target>.tar.gz | tar xz
mv tend-*/tend ~/.local/bin/        # any directory on PATH
```

From source:

```sh
cargo install --path crates/tend-cli   # Rust 1.87 or newer; installs `tend` into ~/.cargo/bin
```

### As a Claude Code plugin

To keep typing `claude`, load tend's hooks as a plugin (the `tend` binary must be on PATH):

```sh
claude plugin marketplace add prakhar479/tend      # or a local clone: ./tend-rs
claude plugin install tend@tend
claude --plugin-dir ./plugin                       # or: one session, from a clone
```

The first hook of each conversation starts its session; `SessionEnd` stops it. Plugin sessions run in `plugin_mode` (config, default `look`). With `tend claude`, the plugin's hooks step aside, so nothing is recorded twice. `tend status` works for plugin sessions too (Claude Code passes the session id to the status line).

## Use

```sh
tend claude                          # look mode: record only
tend claude --tend-mode optimize     # apply actions
tend claude --resume                 # other arguments (and --help) go to claude

tend report                          # newest session started in this directory
tend report --session -2 --full      # the one before it, every section
tend sessions                        # list: start, duration, mode, calls, project
tend fleet                           # fleets, and live tend sessions on this machine, by repo
tend fleet up fleet.toml             # run a team of agents from a fleet file (or --preset lead)
tend fleet status                    # its sessions: state, pid, turns, restarts, worktree
tend fleet down                      # stop them all; `tend fleet prune` cleans up worktrees
tend task add "fix the parser" --role worker   # the task board (agents in the fleet use it too)
tend task list                       # tasks: state, session, dependencies, outcome
tend fleet report                    # tasks, sessions, tokens, cost, and the merge plan
tend fleet merge --yes               # commit finished worktrees to their branches; prints git merge steps
tend msg @lead "is the schema final?"  # a message in the fleet (to user, @role, or a session)
tend inbox                           # the fleet's messages to you (or, in a fleet session, to it)
tend claude --tend-fleet --tend-name rev   # join a fleet from this terminal, as its attached role
tend note "integration tests need DB_URL"  # a note for this project's sessions (an agent's waits for you)
tend notes                           # this project's notes; `tend notes approve n-2` reviews one
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
tend purge --older-than 7             # delete stored sessions (also --session ID, --all)
tend export --session -1 > s.json     # one session, sanitized (paths, user, secrets), for a bug report
tend retro --all --summary           # totals only, no paths: safe to share
tend audit main...HEAD               # experimental: what a diff did to tests and checks (--format markdown|json)
```

`--session` takes an id or a unique prefix of one (as `tend sessions` prints it), `-1` for the newest, `-2`, and so on, or a path.

If tend cannot start (a bad data directory, a daemon that does not come up), `tend claude` says so and runs Claude Code without it. A hook never blocks the agent because of an error in tend.

To see tend in Claude Code's status line, add this to `~/.claude/settings.json`. It replaces any status line you already have. Outside a tend session it shows nothing.

```json
"statusLine": { "type": "command", "command": "tend status" }
```

Session data: `~/.local/share/tend/sessions/<id>/` (override with `TEND_HOME`). Stored outputs and the ledger keep a copy of what tools printed; tend replaces secrets it recognizes (API keys and tokens, private keys, passwords in URLs, values of variables named like secrets) with `[redacted]` before writing. `tend claude` deletes sessions older than `keep_days` (default 30).

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
plugin_mode = look              # mode of sessions the Claude Code plugin starts
notify = false                  # desktop notification when tend steers a loop or blocks an edit
wrap_tests = true               # optimize: run tests through `tend run`; each costs ~1-2 s in auto mode
fleet = look                    # other tend sessions here: off, look (record only), on (also tell the agent)
learn = look                    # recall fixes other sessions learned, and project notes: off, look, on
```

### Learning

tend remembers fixes across sessions on this machine. When a test or build failure goes away after edits and a complete run passes (its exit code, or the runner's summary when the run is piped through `tail` or `tee`; no `-x`, `-k`, or `--lf`), the session keeps a learned fix (the failure, the command, the files changed) in `~/.local/share/tend/learn/fixes.jsonl`. All worktrees of a repo are one project. The second time another session hits the same failure, tend tells it where and how it was fixed, this project's fix first. Notes (`tend note`) are what you want every session of a project to know. A note you type in your terminal is kept; one written from an agent's shell (or without a terminal) waits until you `tend notes approve` it. With `learn = on` (optimize mode) the agent gets both; with `look`, the default, they are only recorded.

### Fleet

Sessions on one machine see what the others do (design: `docs/fleet-2026-10-21.md`). Each daemon reads the other live sessions' ledgers once a second. tend notices four things:

- **Peer edit.** In the same checkout, another session edited a file after this one did.
- **Peer git.** In the same checkout, another session ran a git command that changes files or HEAD (`git stash`, `checkout`, `reset`, `commit`, …).
- **Peer failure.** A failed run here names a file another session in this checkout edited in the last 30 minutes.
- **Worktree overlap.** In another worktree of the same repo, both sessions edited one file. Merging the branches will conflict there.

`tend fleet up` runs a team from a fleet file (TOML): roles, how many of each, who may message whom, and limits (design: `docs/fleet-topologies-2026-10-21.md`). A hub starts each session headless under `tend claude`, each in its own worktree on a `tend/<fleet>/<slot>` branch, restarts one that crashes (after 5, 10, 20 s), and records every step in `~/.local/share/tend/fleets/<id>/ledger.jsonl`. `tend fleet prune` never removes a worktree with uncommitted changes or commits past the base. Tasks go on the fleet's board (`tend task add`, `assign`, `claim`, `done`, `blocked`): a lead's agent splits its task into parts, each ready part goes to a fresh worker session, a task waits for the tasks it depends on, and a session that ends without finishing costs the task an attempt. A task counts as done only when its check passes: the fleet file's `check` (or the task's own, `--check`) runs in the worker's worktree, and a failure goes back to the worker that made the change. With `experimental_audit = true`, a change that passes but weakens tests waits for its creator (`tend task done` accepts it, `tend task reject` sends it back). Roles can be limited to paths (`may_write`, `may_not_write`), and a task's `--scope` is a claim other sessions in the same worktree may not edit. Token and wall-time budgets stop a session (tokens count everything a turn used, cache reads included, so set budgets from `tend fleet report`); the fleet pauses at its token limit or the plan's usage limit (`tend fleet resume`). tend tells the sessions apart by their process tree, so an agent cannot act as another session or as you. Sessions message each other and you (`tend msg`): an idle headless session gets a message as a turn, a busy one or one in your terminal at its next hook, labelled as another session's words. Your own terminal session can join a fleet as an attached role (`tend claude --tend-fleet`).

Each notice is an action in the ledger, caused by the other session's entry. `tend why a-N` follows the cause into that session's ledger. With `fleet = look` (the default) notices are only recorded and shown in `tend report`. With `fleet = on`, a session in optimize mode also gets them as added context at its next hook. Notices of one kind about one file make one message; at most 3 messages per hook and 40 notices per session.

## Repo policy

A `.tend.toml` at the repo root (committed with the project) adjusts the guard for that repo:

```toml
test_paths = ["e2e/**", "checks/*.py"]           # also test files
protected = ["migrations/**", "tests/golden/**"]  # edits are guarded like test weakening
audit_ignore = ["vendor/**"]                      # left out of `tend audit`
```

An edit of a protected path is denied in optimize mode (and recorded in look mode) until you run `! tend allow`, like any guard flag. Globs are relative to the repo root; `*` stays within a directory, `**` crosses them, and a pattern without `/` matches a file name anywhere.

A bad line is skipped with a warning when `tend claude` starts; the other lines still apply.

### Audit (experimental)

`tend audit` reads a git diff, or a whole `tend claude` session with `experimental_audit = true`, and lists what changed in the project's checks:

- test files: assertions and tests before and after, and anything that may weaken them (removed tests, skips, looser tolerances, always-true asserts);
- snapshot and golden files that were rewritten;
- suppressions added (`# noqa`, `# type: ignore`, `eslint-disable`, `#[allow]`, …), and whether each one gives a reason;
- check config (CI workflows, coverage thresholds, strictness) and dependency files.

It only reports; it never blocks. It reads the working tree through a temporary index, so your index, branch, and files are untouched. The feature is under test: its output may change.


## Environment

| Variable | Effect |
|---|---|
| `TEND_HOME` | Data directory |
| `TEND_CLAUDE_BIN` | Agent binary (default `claude`) |
| `TEND_HOOK_TIMEOUT_MS` | Shim timeout (default 250) |
| `TEND_CAPTURE=1` | Also write raw hook payloads to `hooks.jsonl` (they contain code; off by default) |
| `TEND_CONFIG` | Config file (default `~/.config/tend/config.toml`) |
