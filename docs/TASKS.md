# Tasks

Status key: `[ ]` open · `[x]` done. Work top to bottom. One task at a time.

## Phase 0 — Spikes (validate assumptions)

Goal: replace every `[Verify]` in the code with a tested fact. Record each result under "Spike results".

- [x] **S1. Hook payload shapes.** Run `rein claude`. Run a Bash command, an Edit, a Write, and a failing command. Read `<session>/hooks.jsonl`.
  Record: Bash `tool_response` shape; exit code field (if any); PostToolUseFailure fields; Edit/MultiEdit/Write `tool_input`.
  Then: replace `fixtures/claude_code/*.json` with sanitized captures. Fix `extract_output` if needed.
  Note: captured headless (`rein claude -p`). Interrupt (Esc) was not exercised. Large outputs now count as the preview the agent sees, not the full stdout.
- [x] **S2. Output replacement.** `rein claude --rein-mode optimize`. Ask the agent to run a noisy test suite, then ask what the output said.
  Record: does the agent see the summary? Does `updatedToolOutput` need a string or an object for Bash? Fix `encode_replacement`.
  Note: only PostToolUse can replace output. Failed runs are recorded as `unsupported` shape actions.
- [x] **S3. Settings injection.** Confirm `claude --settings <file>` exists and merges hooks with user and project hooks. If not, find another way (for example a plugin) and update `launcher.rs`.
- [x] **S4. Latency.** Measure shim p50/p95 on your machine with a release build. Target p95 < 20 ms. (Container measurement: p50 2.5 ms, p95 4.5 ms for a 6k-token payload.)
- [x] **S5. Transcript usage.** Compare `rein report` token numbers with the Claude Code `/cost` or usage view for the same session. Confirm field names and per-message dedupe in `report::usage_from_transcript`.
  Note: usage dedupe now keeps the last line of each message.
- [x] **S6. Ask permission.** Trigger the test guard in optimize mode. Record who sees `permissionDecisionReason` (user, agent, or both).
  Decided 2026-10-19: deny with the reason. The user lifts one block with `! rein allow`; the agent's own `rein allow` is blocked in PreToolUse.
- [x] **S7. Ctrl-C.** In a rein session, press Ctrl-C once (interrupt) and twice (exit). Confirm: the agent behaves as normal; the daemon stops; the socket is removed.
- [x] **S8. Failure events.** Does a non-zero Bash exit fire PostToolUseFailure, PostToolUse, or both? Fix `failed` handling if needed.
  Note: S1 answered it (PostToolUseFailure only). Failed runs are no longer shaped (see S2).
- [x] **S9. Name.** Check `winnow` on GitHub, crates.io, npm, PyPI, and a domain.
  Note: the name is taken on crates.io and GitHub. Renamed 2026-10-19: command `rein`, crates `rein-core` and `rein-cli`.
- [ ] **S10. Codex prep (Phase 2).** In Codex, compare PostToolUse `decision: "block"` replacement with PreToolUse `updatedInput` rewrite. Record how the model treats a "blocked" result.
  Blocked: Codex CLI is not installed on the dev machine.

### Spike results

| Spike | Date | Result | Code change |
|---|---|---|---|
| S1 | 2026-10-19 | Claude Code 2.1.303. **Bash PostToolUse** `tool_response` = `{stdout, stderr, interrupted, isImage, noOutputExpected}`; stderr is merged into stdout and `stderr` was `""` every time. Outputs up to ~25k chars arrive whole. Over 30000 chars: `stdout` is cut to 30000 and `persistedOutputPath` + `persistedOutputSize` are added; the agent sees only a `<persisted-output>` message with a 2000-byte preview (cut back to a newline). **No exit-code field.** **Non-zero exit fires only PostToolUseFailure** (answers S8): fields `error`, `is_interrupt`, `duration_ms`, no `tool_response`; `error` = `"Exit code N\n"` + merged output, identical to what the agent sees, and cut in the middle near 10k chars (`... [N characters truncated] ...`). **Edit** `tool_input` = `{file_path (absolute), old_string, new_string, replace_all}`; **Write** = `{file_path, content}`; Post `tool_response` adds `originalFile`, `structuredPatch`, `userModified`. **MultiEdit does not exist.** Edit validation errors (file not read, string not found) fire no hook. New common fields: `prompt_id`, `effort`, `duration_ms`; Stop adds `last_assistant_message`. | `extract_output` returns the persisted-output message for large outputs. 12 captured fixtures in `fixtures/claude_code/`. Connector tests rewritten on them; e2e asserts on a passing run. Open for S2/S8: in optimize mode a failed run is shaped and logged as applied, but `encode` cannot replace PostToolUseFailure output. |
| S2 | 2026-10-19 | PostToolUse `updatedToolOutput` as an object in the `tool_response` shape reaches the agent (it saw the 12-line summary and footer). A plain string is silently ignored for Bash. PostToolUseFailure ignores `updatedToolOutput` (string and object) but passes `additionalContext` to the agent. | `encode_replacement` keeps the object shape. New capability `replace_failed_output` (Claude Code: false). Shape actions on failed runs are recorded with `unsupported: true` and never applied; report shows them as "Not shapeable". Retro now plans with Claude Code capabilities in look mode. |
| S3 | 2026-10-19 | `claude --settings <file>` exists. Its hooks merge with user (`~/.claude/settings.json`), project (`.claude/settings.json`), and local (`.claude/settings.local.json`) hooks: all four fired on one Bash call. | None. |
| S4 | 2026-10-19 | Release build, WSL2, 15 cores, 500 calls each. PostToolUse 25.7 KB: p50 2.7 ms, p95 4.1 ms. PostToolUseFailure 10.6 KB: p50 2.2, p95 3.3. PreToolUse 0.7 KB: p50 1.7, p95 2.5. No daemon (fail open): p50 1.2, p95 1.9. Found: socket bind fails when the session path is longer than ~108 bytes. | `paths::socket_path` falls back to `<tmp>/rein-<hash>.sock` for long session paths. |
| S5 | 2026-10-19 | Field names `input_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`, `output_tokens` under `message.usage` are right. Report totals equal `claude -p --output-format json` usage exactly (79.5k prompt, 72.6k cache read, 407 output). One API message spans several lines (1427 of 1857 messages); the lines repeat the same usage, except one case where the first line was all zeros. | `usage_from_transcript` keeps the last line per message id. `fixtures/transcripts/loop_session.jsonl` is now a sanitized real transcript; retro finds the same loop as the live session. |
| S6 | 2026-10-19 | Headless (`-p`): `ask` acts as deny; the agent gets `permissionDecisionReason` as the error tool result. Interactive: the user sees Claude Code's normal "Do you want to make this edit?" prompt with the diff, but **not** the reason; on "No" the agent gets the generic rejection text, also without the reason. `systemMessage` did not show it either. | Claude Code `ask` capability set to false, so the guard downgrades to deny (the agent and the user both see the reason; checked in tmux). New `rein allow [a-N]` lifts one block once (`override` ledger entry); `!` commands fire no tool hooks, so the user can run it from the prompt. PreToolUse now also matches Bash and denies the agent's own `rein allow` (text match: advisory, not a security boundary). |
| S7 | 2026-10-19 | Driven through tmux. One Ctrl-C interrupts the running Bash call; Claude Code, launcher, and daemon stay up. Two Ctrl-C exit Claude Code; the launcher exits 0, the daemon stops, the socket is removed. An interrupted Bash call fires no PostToolUse or PostToolUseFailure. | None. |
| S8 | 2026-10-19 | Non-zero exit fires only PostToolUseFailure (see S1). | Covered by S2 change. |
| S9 | 2026-10-19 | crates.io `winnow`: parser-combinator crate, ~1.0B downloads. GitHub `winnow-rs/winnow`: same project, 975 stars. npm `winnow` and PyPI `winnow` taken (unrelated). Domains: winnow.io, .ai, .app, .tools, getwinnow.com resolve; winnow.dev and winnow.sh have no DNS. | Renamed to `rein` (owner wanted a verb for the command). `rein` on crates.io is an unrelated 3D library (61 downloads), so the crates are `rein-core` and `rein-cli`; no `rein` command or Debian package exists. Env vars `REIN_*`, data in `~/.local/share/rein`, config in `~/.config/rein/config.toml`. |

## Phase 1 — Look mode, Claude Code

Exit: owners of about 20 real sessions agree the reports are correct.

- [x] **T1. Captured fixtures.** After S1: all connector tests use captured payloads.
  Note: all connector tests and e2e use the S1 captures.
- [x] **T2. More parsers.** jest/vitest and go test fixtures from real runs. Acceptance: `extract_failures` finds every failure; `shape` keeps each one.
  Note: real go (plain and `-v`), jest 29, vitest 2 runs. go `-v` logs come before `--- FAIL`; a failing parent test is dropped in favor of its subtest; jest messages keep Expected/Received and diff lines. `shape` keeps go `-v` logs, jest `●` headers and diff lines; a gap of blank lines is one blank line, not a marker.
- [x] **T3. Repeated stack traces.** Collapse repeated multi-line blocks (not only repeated lines) in `shape.rs`. Acceptance: a fixture with 50 identical traces shows one trace and a count.
  Note: blocks of 2–60 identical lines repeated back to back collapse to the first copy plus `[… previous N lines repeated K more times …]`.
- [x] **T4. Report numbers.** After S5: report shows prompt tokens, cache-read ratio, and Bash noise share from real data.
  Note: report shows `noise N% (est.)` next to Bash output. Noise counts failed runs too; `Shaping` savings count only output the agent can replace.
- [x] **T5. Retro over a project.** `rein retro --all` runs over every transcript in `~/.claude/projects` and prints a total plus the top 5 sessions by noise and by loop time.
  Note: `rein retro --all [--projects <dir>]`. Loop time is wall clock, so a session resumed days later can show a very long loop.
- [x] **T6. Loop time.** Record how long each loop lasted (first occurrence to steer) and show it in the report.
  Note: `SteerTrigger::Loop.duration_ms` (optional). Retro uses transcript timestamps.
- [x] **T7. Config file.** `~/.config/rein/config.toml` overrides `Config` defaults. Acceptance: `rein report` prints active non-default values.
  Note: flat `key = value` file (no TOML crate: it depends on the `winnow` parser crate, whose name clashes with ours and breaks `cargo -p winnow`). `WINNOW_CONFIG` overrides the path. Non-default values are recorded in a `config` ledger entry at session start.
- [x] **T8. Session cleanup.** `rein sessions --prune <days>`. Remove stale sockets on start.
  Note: liveness is a ping, not the socket file. `rein sessions` uses the same check.
- [x] **T9. Ledger format version.** Add a first entry with the format version. Readers reject newer versions with a clear message.
  Note: `LEDGER_FORMAT = 1`. Ledgers without a `format` entry read as version 1. New kinds `format` and `config` and fields `unsupported`, `duration_ms` are additive.
- [x] **T10. Daemon test.** Integration test: two concurrent hook calls; the ledger has strictly increasing `seq` and no torn lines.
  Note: 8 threads × 5 hook calls against a real daemon.

## Next steps (after Phase 1)

Consolidated 2026-10-19 from the Phase 0/1 results. Owner decisions are marked **[Decided]**.

- [x] **N1. Guard reason (S6) [Decided: deny].** Deny with the reason; `rein allow` lifts one block.
- [x] **N2. Rename (S9) [Decided: rein].** Command `rein`; crates `rein-core`, `rein-cli`.
- [x] **N3. Failed-run shaping (D4) [Decided: build].** PreToolUse rewrites a test command to `rein run --id <id> --sh '<cmd>'`. `rein run` keeps the exit code, asks the daemon for a summary, and fails open to the original bytes.
  Spike (2026-10-19): `updatedInput` applies without a `permissionDecision`, and Claude Code checks permissions on the **rewritten** command (a compound rewrite was refused as "multiple operations"; `permissionDecision: "allow"` skips the check, so rein never sends it). Auto mode ran the single-command rewrite without a prompt; default mode prompted with the rewritten text. So rein wraps only in `auto` and `bypassPermissions` modes, never background calls, never commands that change shell state (`cd`, `export`, `source`, …). Live: a failing pytest run went from 324 to 54 lines; the agent named the failing test and kept exit code 1.
- [x] **N4. Git.** Repo under git; baseline commit, work on branch `next-steps`.
- [ ] **N5. CI.** Run `make check` on push.
- [ ] **N6. Private sockets and session dirs.** 0700 session dirs; the `/tmp` fallback socket in a private directory.
- [ ] **N7. Raw capture off by default.** `hooks.jsonl` only with `REIN_CAPTURE=1`.
- [ ] **N8. Old ledgers.** Report infers "not shapeable" for failed runs in ledgers written before the `unsupported` flag.
- [ ] **N9. Active loop time.** Leave out idle gaps (default: gaps over 10 min) from loop time.
- [ ] **N10. More re-open signals.** Reading a persisted-output file or re-running the same command right after a summary counts as a re-open.
- [ ] **N11. Retro and live agree.** Same tool-call counting in retro and live reports.
- [ ] **N12. Capture script.** `scripts/capture-fixtures` turns a session's `hooks.jsonl` into sanitized fixtures.
- [ ] **N13. Real pytest and cargo fixtures.** Replace the synthetic ones.
- [ ] **N14. Edge cases.** Claude Code's mid-output cut; persisted preview with no newline in 2000 bytes; the 1 MiB size boundary.
- [ ] **N15. macOS.** Latency, Ctrl-C, socket fallback. Needs a Mac.
- [ ] **N16. Real-session review (Phase 1 exit).** ~20 sessions in look mode; owners confirm the reports.

## Phase 2 — Optimize + Codex (not started)

- [ ] Tune `Config` defaults from Phase 1 data.
- [ ] A/B evaluation harness (reference section 15). Gate: task success does not decrease.
- [ ] Codex connector (`connector/codex.rs`), `rein init codex`, capability downgrades.

## Phase 3 — Protect (not started)

- [ ] Guard rules: expected value changed to last actual; tolerance increased.
- [ ] Snapshots as private git refs; `rein undo`.
- [ ] Guard overrides feed strictness.

## Phase 4 — Deciders (not started)

- [ ] Decider contract (versioned JSON); offline evaluation against ledger judgments; one self-hosted adapter.
