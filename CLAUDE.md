# CLAUDE.md — tend

tend is a local supervisor for command-line coding agents.
Design: `docs/tend-reference.md`. Work items: `docs/TASKS.md`.
Current scope: look and optimize modes for Claude Code, and fleets of sessions (docs/fleet-*.md). Codex waits for S10.

## Rules

Do not break these. If a task needs you to, stop and ask.

1. **Fail open.** `tend hook` always exits 0. On any error or timeout it prints nothing.
2. **tend-core has no I/O.** No files, sockets, env vars, clocks, threads, or model calls. Time comes in as `now_ms`.
3. **Never edit the past.** Act only on content before the agent sees it.
4. **Every action is recorded** with a one-sentence reason and its causes.
5. **Look mode never returns a hook response.**
6. **Deterministic.** Use `sha256_hex` for hashes. Do not let `HashMap` order reach output or the ledger.
7. **Shim stays thin.** It only forwards. Target p95 < 20 ms (measured: ~5 ms).
8. **Capability-honest.** Plan only with `Capabilities`. Downgrade; do not fail.
9. **No new dependency** in tend-core without a reason in the commit message.
10. **Parsers need fixtures.** Every parser change adds a file in `fixtures/` and a test.

## Unverified facts

Agent behavior marked `[Verify]` in code is an assumption. Before you build on one, check `docs/TASKS.md` Phase 0. If a spike shows a fact is wrong, record it in `docs/TASKS.md` under "Spike results" and fix the code. Do not work around it silently.

## Layout

```
crates/tend-core/src/        pure logic, no I/O
  event.rs                     common event vocabulary (connector → core)
  record.rs                    ledger entries (core output)
  capability.rs                what each agent supports
  perception/classify.rs       command → output kind, test runner, `tend show` re-opens
  perception/signature.rs      failure extraction + error-signature normalizer
  shape.rs                     faithful summaries (levels 0–3)
  session.rs                   session model, adaptation knobs
  loops.rs                     loop + edit-cycle detection
  guard.rs                     test-weakening rules
  broker.rs                    question catalog, rule answers, Decider trait
  controller.rs                sense → judge → decide → act → record
  fleet/awareness.rs           F1: what other sessions did that matters here
  fleet/spec.rs                fleet files: roles, channels, limits, presets
  fleet/model.rs, model/tasks.rs  fleet state machine: slots, tasks, checks, messages
  fleet/scope.rs               a fleet role's write scope and others' claims
  learn.rs                     F2: learned fixes (fail → edits → full pass) and recall
  audit.rs                     experimental audit of changes to tests and checks
crates/tend-cli/src/         binary `tend`: all I/O
  main.rs                      CLI
  launcher.rs                  `tend claude`: daemon + agent with hooks
  daemon.rs                    per-session state behind a Unix socket
  shim.rs                      `tend hook <Event>` (fail open)
  ipc.rs                       one JSON line each way
  connector/claude_code.rs     hook JSON ↔ core events/responses, settings file
  store.rs                     JSONL ledger + blobs (SQLite later; keep the API)
  report.rs                    report, why, output lookup
  retro.rs                     look mode over old transcripts (one, or --all)
  config.rs                    ~/.config/tend/config.toml → Config
  sessions.rs                  prune old sessions, remove stale sockets
  paths.rs                     session dirs, socket path, Claude projects dir
  fleet.rs                     F1 watcher (tails peers' ledgers), `tend fleet` view
  hub.rs                       fleet hub: runs a fleet file; `tend fleet/task/msg/inbox`
  learnstore.rs                <home>/learn: fixes.jsonl, notes; `tend note/notes`
  audit.rs                     `tend audit`: worktree snapshots, diffs
crates/tend-cli/tests/e2e.rs launcher → daemon → shim with a fake agent
fixtures/                      outputs, hook payloads, transcripts
```

## Data flow

agent hook → `tend hook` → socket → daemon → `Connector::map` → `Controller::handle` → `Decision { entries, blobs, response }` → `Store::persist` → `Connector::encode` → shim stdout → agent.

## Commands

```sh
make check                          # fmt --check, clippy -D warnings, test
make check-clean                    # the same with an empty HOME and minimal PATH, as CI runs it
cargo test -p tend-core           # fast loop for core logic
make e2e                            # end-to-end with fake agent
cargo run -- retro fixtures/transcripts/loop_session.jsonl
cargo run -- retro --all            # every transcript in ~/.claude/projects
cargo build && ./target/debug/tend claude      # live, look mode
TEND_CAPTURE=1 ./target/debug/tend claude          # also keep raw hook payloads
jq . ~/.local/share/tend/sessions/<id>/hooks.jsonl   # raw payloads (with TEND_CAPTURE=1)
scripts/conformance.py --record     # live check of the Claude Code behaviors tend needs (one short session)
scripts/conformance.py --replay <dir> # re-check a run kept with --keep (no model call)
scripts/learn_live.py ./target/debug/tend <dir>   # live fleet + learning check on Haiku (~$0.04)
cargo test --release -p tend-cli --test e2e -- --ignored shim_latency   # rule 7 budget
(cd fuzz && cargo +nightly fuzz run shell -- -max_total_time=60)       # also: output, guard
scripts/capture_fixtures.py hooks <hooks.jsonl> --event PostToolUse --match pytest --out fixtures/claude_code/x.json
```

## How to work

1. One task from `docs/TASKS.md` at a time. Start in plan mode. Show the plan first.
2. Write the test from the acceptance criteria first.
3. Run `make check` before you say a task is done.
4. Tick the task in `docs/TASKS.md`. Add a short note if behavior changed.
5. Keep ledger formats backward-compatible, or bump the format and say so.

## Style

- Errors: `anyhow` in the binary. No `unwrap` on external data.
- IDs: entries are `seq` numbers; outputs `o-N`; actions `a-N`.
- Regexes: use the `re!` macro (compiled once per call site).
- User-facing text: short, plain sentences. One idea per sentence.
