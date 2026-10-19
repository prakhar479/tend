# CLAUDE.md — rein

rein is a local supervisor for command-line coding agents.
Design: `docs/rein-reference.md`. Work items: `docs/TASKS.md`.
Current scope: Phase 0 spikes and Phase 1 (look mode, Claude Code only).

## Rules

Do not break these. If a task needs you to, stop and ask.

1. **Fail open.** `rein hook` always exits 0. On any error or timeout it prints nothing.
2. **rein-core has no I/O.** No files, sockets, env vars, clocks, threads, or model calls. Time comes in as `now_ms`.
3. **Never edit the past.** Act only on content before the agent sees it.
4. **Every action is recorded** with a one-sentence reason and its causes.
5. **Look mode never returns a hook response.**
6. **Deterministic.** Use `sha256_hex` for hashes. Do not let `HashMap` order reach output or the ledger.
7. **Shim stays thin.** It only forwards. Target p95 < 20 ms (measured: ~5 ms).
8. **Capability-honest.** Plan only with `Capabilities`. Downgrade; do not fail.
9. **No new dependency** in rein-core without a reason in the commit message.
10. **Parsers need fixtures.** Every parser change adds a file in `fixtures/` and a test.

## Unverified facts

Agent behavior marked `[Verify]` in code is an assumption. Before you build on one, check `docs/TASKS.md` Phase 0. If a spike shows a fact is wrong, record it in `docs/TASKS.md` under "Spike results" and fix the code. Do not work around it silently.

## Layout

```
crates/rein-core/src/        pure logic, no I/O
  event.rs                     common event vocabulary (connector → core)
  record.rs                    ledger entries (core output)
  capability.rs                what each agent supports
  perception/classify.rs       command → output kind, test runner, `rein show` re-opens
  perception/signature.rs      failure extraction + error-signature normalizer
  shape.rs                     faithful summaries (levels 0–3)
  session.rs                   session model, adaptation knobs
  loops.rs                     loop + edit-cycle detection
  guard.rs                     test-weakening rules
  broker.rs                    question catalog, rule answers, Decider trait
  controller.rs                sense → judge → decide → act → record
crates/rein-cli/src/         binary `rein`: all I/O
  main.rs                      CLI
  launcher.rs                  `rein claude`: daemon + agent with hooks
  daemon.rs                    per-session state behind a Unix socket
  shim.rs                      `rein hook <Event>` (fail open)
  ipc.rs                       one JSON line each way
  connector/claude_code.rs     hook JSON ↔ core events/responses, settings file
  store.rs                     JSONL ledger + blobs (SQLite later; keep the API)
  report.rs                    report, why, output lookup
  retro.rs                     look mode over old transcripts (one, or --all)
  config.rs                    ~/.config/rein/config.toml → Config
  sessions.rs                  prune old sessions, remove stale sockets
  paths.rs                     session dirs, socket path, Claude projects dir
crates/rein-cli/tests/e2e.rs launcher → daemon → shim with a fake agent
fixtures/                      outputs, hook payloads, transcripts
```

## Data flow

agent hook → `rein hook` → socket → daemon → `Connector::map` → `Controller::handle` → `Decision { entries, blobs, response }` → `Store::persist` → `Connector::encode` → shim stdout → agent.

## Commands

```sh
make check                          # fmt --check, clippy -D warnings, test
cargo test -p rein-core           # fast loop for core logic
make e2e                            # end-to-end with fake agent
cargo run -- retro fixtures/transcripts/loop_session.jsonl
cargo run -- retro --all            # every transcript in ~/.claude/projects
cargo build && ./target/debug/rein claude      # live, look mode
REIN_CAPTURE=1 ./target/debug/rein claude          # also keep raw hook payloads
jq . ~/.local/share/rein/sessions/<id>/hooks.jsonl   # raw payloads (with REIN_CAPTURE=1)
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
