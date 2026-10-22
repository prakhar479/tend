# Tasks

Status key: `[ ]` open · `[x]` done. Work top to bottom. One task at a time.

## Phase 0 — Spikes (validate assumptions)

Goal: replace every `[Verify]` in the code with a tested fact. Record each result under "Spike results".

- [x] **S1. Hook payload shapes.** Run `tend claude`. Run a Bash command, an Edit, a Write, and a failing command. Read `<session>/hooks.jsonl`.
  Record: Bash `tool_response` shape; exit code field (if any); PostToolUseFailure fields; Edit/MultiEdit/Write `tool_input`.
  Then: replace `fixtures/claude_code/*.json` with sanitized captures. Fix `extract_output` if needed.
  Note: captured headless (`tend claude -p`). Interrupt (Esc) was not exercised. Large outputs now count as the preview the agent sees, not the full stdout.
- [x] **S2. Output replacement.** `tend claude --tend-mode optimize`. Ask the agent to run a noisy test suite, then ask what the output said.
  Record: does the agent see the summary? Does `updatedToolOutput` need a string or an object for Bash? Fix `encode_replacement`.
  Note: only PostToolUse can replace output. Failed runs are recorded as `unsupported` shape actions.
- [x] **S3. Settings injection.** Confirm `claude --settings <file>` exists and merges hooks with user and project hooks. If not, find another way (for example a plugin) and update `launcher.rs`.
- [x] **S4. Latency.** Measure shim p50/p95 on your machine with a release build. Target p95 < 20 ms. (Container measurement: p50 2.5 ms, p95 4.5 ms for a 6k-token payload.)
- [x] **S5. Transcript usage.** Compare `tend report` token numbers with the Claude Code `/cost` or usage view for the same session. Confirm field names and per-message dedupe in `report::usage_from_transcript`.
  Note: usage dedupe now keeps the last line of each message.
- [x] **S6. Ask permission.** Trigger the test guard in optimize mode. Record who sees `permissionDecisionReason` (user, agent, or both).
  Decided 2026-10-19: deny with the reason. The user lifts one block with `! tend allow`; the agent's own `tend allow` is blocked in PreToolUse.
- [x] **S7. Ctrl-C.** In a tend session, press Ctrl-C once (interrupt) and twice (exit). Confirm: the agent behaves as normal; the daemon stops; the socket is removed.
- [x] **S8. Failure events.** Does a non-zero Bash exit fire PostToolUseFailure, PostToolUse, or both? Fix `failed` handling if needed.
  Note: S1 answered it (PostToolUseFailure only). Failed runs are no longer shaped (see S2).
- [x] **S9. Name.** Check `winnow` on GitHub, crates.io, npm, PyPI, and a domain.
  Note: the name is taken on crates.io and GitHub. Renamed 2026-10-19 to `rein`, then to `tend` the same day: command `tend`, crates `tend-core` and `tend-cli`.
- [ ] **S10. Codex prep (Phase 2).** In Codex, compare PostToolUse `decision: "block"` replacement with PreToolUse `updatedInput` rewrite. Record how the model treats a "blocked" result.
  Blocked: Codex CLI is not installed on the dev machine.

### Spike results

| Spike | Date | Result | Code change |
|---|---|---|---|
| S1 | 2026-10-19 | Claude Code 2.1.303. **Bash PostToolUse** `tool_response` = `{stdout, stderr, interrupted, isImage, noOutputExpected}`; stderr is merged into stdout and `stderr` was `""` every time. Outputs up to ~25k chars arrive whole. Over 30000 chars: `stdout` is cut to 30000 and `persistedOutputPath` + `persistedOutputSize` are added; the agent sees only a `<persisted-output>` message with a 2000-byte preview (cut back to a newline). **No exit-code field.** **Non-zero exit fires only PostToolUseFailure** (answers S8): fields `error`, `is_interrupt`, `duration_ms`, no `tool_response`; `error` = `"Exit code N\n"` + merged output, identical to what the agent sees, and cut in the middle near 10k chars (`... [N characters truncated] ...`). **Edit** `tool_input` = `{file_path (absolute), old_string, new_string, replace_all}`; **Write** = `{file_path, content}`; Post `tool_response` adds `originalFile`, `structuredPatch`, `userModified`. **MultiEdit does not exist.** Edit validation errors (file not read, string not found) fire no hook. New common fields: `prompt_id`, `effort`, `duration_ms`; Stop adds `last_assistant_message`. | `extract_output` returns the persisted-output message for large outputs. 12 captured fixtures in `fixtures/claude_code/`. Connector tests rewritten on them; e2e asserts on a passing run. Open for S2/S8: in optimize mode a failed run is shaped and logged as applied, but `encode` cannot replace PostToolUseFailure output. |
| S2 | 2026-10-19 | PostToolUse `updatedToolOutput` as an object in the `tool_response` shape reaches the agent (it saw the 12-line summary and footer). A plain string is silently ignored for Bash. PostToolUseFailure ignores `updatedToolOutput` (string and object) but passes `additionalContext` to the agent. | `encode_replacement` keeps the object shape. New capability `replace_failed_output` (Claude Code: false). Shape actions on failed runs are recorded with `unsupported: true` and never applied; report shows them as "Not shapeable". Retro now plans with Claude Code capabilities in look mode. |
| S3 | 2026-10-19 | `claude --settings <file>` exists. Its hooks merge with user (`~/.claude/settings.json`), project (`.claude/settings.json`), and local (`.claude/settings.local.json`) hooks: all four fired on one Bash call. | None. |
| S4 | 2026-10-19 | Release build, WSL2, 15 cores, 500 calls each. PostToolUse 25.7 KB: p50 2.7 ms, p95 4.1 ms. PostToolUseFailure 10.6 KB: p50 2.2, p95 3.3. PreToolUse 0.7 KB: p50 1.7, p95 2.5. No daemon (fail open): p50 1.2, p95 1.9. Found: socket bind fails when the session path is longer than ~108 bytes. | `paths::socket_path` falls back to `<tmp>/tend-<hash>.sock` for long session paths. |
| S5 | 2026-10-19 | Field names `input_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`, `output_tokens` under `message.usage` are right. Report totals equal `claude -p --output-format json` usage exactly (79.5k prompt, 72.6k cache read, 407 output). One API message spans several lines (1427 of 1857 messages); the lines repeat the same usage, except one case where the first line was all zeros. | `usage_from_transcript` keeps the last line per message id. `fixtures/transcripts/loop_session.jsonl` is now a sanitized real transcript; retro finds the same loop as the live session. |
| S6 | 2026-10-19 | Headless (`-p`): `ask` acts as deny; the agent gets `permissionDecisionReason` as the error tool result. Interactive: the user sees Claude Code's normal "Do you want to make this edit?" prompt with the diff, but **not** the reason; on "No" the agent gets the generic rejection text, also without the reason. `systemMessage` did not show it either. | Claude Code `ask` capability set to false, so the guard downgrades to deny (the agent and the user both see the reason; checked in tmux). New `tend allow [a-N]` lifts one block once (`override` ledger entry); `!` commands fire no tool hooks, so the user can run it from the prompt. PreToolUse now also matches Bash and denies the agent's own `tend allow` (text match: advisory, not a security boundary). |
| S7 | 2026-10-19 | Driven through tmux. One Ctrl-C interrupts the running Bash call; Claude Code, launcher, and daemon stay up. Two Ctrl-C exit Claude Code; the launcher exits 0, the daemon stops, the socket is removed. An interrupted Bash call fires no PostToolUse or PostToolUseFailure. | None. |
| S8 | 2026-10-19 | Non-zero exit fires only PostToolUseFailure (see S1). | Covered by S2 change. |
| S9 | 2026-10-19 | crates.io `winnow`: parser-combinator crate, ~1.0B downloads. GitHub `winnow-rs/winnow`: same project, 975 stars. npm `winnow` and PyPI `winnow` taken (unrelated). Domains: winnow.io, .ai, .app, .tools, getwinnow.com resolve; winnow.dev and winnow.sh have no DNS. | Renamed to `rein` (owner wanted a verb for the command), then to `tend`: `rein` collides with `pegasi-ai/reins`, an agent-guard tool. `tend` is taken on crates.io, PyPI, and npm by unrelated projects, so the crates are `tend-core` and `tend-cli`; no coding-agent tool on GitHub uses the name, and no system command has it. Env vars `TEND_*`, data in `~/.local/share/tend`, config in `~/.config/tend/config.toml`. |
| F-S1 | 2026-10-21 | Claude Code 2.1.306, Haiku, `tend claude -p --input-format stream-json --output-format stream-json --verbose --session-id <uuid>` driven by `scripts/spike_stream.py`. **Hooks fire per turn as in a normal session**: SessionStart once, then UserPromptSubmit, PreToolUse, PostToolUse, Stop for each streamed user message; payload `session_id` is the one tend chose. **A turn after idle works** (8 s gap), and lines sent while a turn runs are queued and answered in order. **Interrupt**: `{"type":"control_request","request_id":…,"request":{"subtype":"interrupt"}}` gets `control_response` success in 60 ms (`still_queued: []`); the turn ends with result `error_during_execution`; the interrupted Bash call fires no PostToolUse and that turn no Stop (as S7); the next turn works. A foreground Bash call running ~3 s became a background task (`task_started`), and the interrupt stopped it (`task_notification: stopped`). **Closing stdin** ends the session, exit 0 in ~1 s. **Fork**: `-p --resume <id> --fork-session --session-id <new>` keeps the chosen id, has the whole history, fires hooks in a new tend session with SessionStart `source: "fork"`, and leaves the parent unchanged (two forks of one parent got the same history). The fork's transcript copies the parent's messages. **UserPromptSubmit `additionalContext` reaches the model in stream mode** (a hook's "port 8123" was answered). | None in code. Design (`docs/fleet-topologies-2026-10-21.md` §4.1): `stream` mode is confirmed for leads; idle wake = a new user line, interrupt = the control request. Token budgets must count each message uuid once across transcripts (forks and resumes copy history, as in F1.0). |
| F-S2 | 2026-10-21 | `scripts/spike_concurrency.py`: Haiku, stream-json sessions under `tend claude` in optimize mode with `fleet = on`, one worktree each, 300 dead session dirs; 3 turns each (Write a file, Edit one shared file, a 5 s command). 1 alone vs 4 and 8 at once (WSL2, 15 cores, 24 GB). **No retry or rate-limit message at 4 or 8.** Turn times: the first two unchanged (~4 s); the 5 s turn 7.8–8.2 s alone, 9–11.5 s with 8 (+15–40%). **Memory: each Claude Code process ~300 MiB RSS (8: 2.3 GB); each tend daemon ~6.5 MiB.** CPU over a 22 s run with 8: tend daemons 0.15 s in all (fleet watchers polling 300 dirs each second included), agents ~5 s each. Fleet: every session got the overlap notices and quoted them, but with 8 workers on one file each got 7 notices about the same file. The plan's usage limit was not hit; the bench runner's signal is "usage limit"/"session limit" in a result's text (not seen in stream mode yet). Sonnet and Opus not run. | Fleet notices of one kind about one file now reach the agent as one message ("sessions A, B and C also changed shared.txt …"); each is still its own action with its cause, and the per-hook limit counts messages. Rerun with 8: 1–2 messages per session instead of 7. Design: memory, not CPU or rate limits, bounds concurrency on one machine; default `limits.sessions` should follow free memory (~300 MiB per agent). |

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
- [x] **T5. Retro over a project.** `tend retro --all` runs over every transcript in `~/.claude/projects` and prints a total plus the top 5 sessions by noise and by loop time.
  Note: `tend retro --all [--projects <dir>]`. Loop time is wall clock, so a session resumed days later can show a very long loop.
- [x] **T6. Loop time.** Record how long each loop lasted (first occurrence to steer) and show it in the report.
  Note: `SteerTrigger::Loop.duration_ms` (optional). Retro uses transcript timestamps.
- [x] **T7. Config file.** `~/.config/tend/config.toml` overrides `Config` defaults. Acceptance: `tend report` prints active non-default values.
  Note: flat `key = value` file (no TOML crate: it depends on the `winnow` parser crate, whose name clashes with ours and breaks `cargo -p winnow`). `WINNOW_CONFIG` overrides the path. Non-default values are recorded in a `config` ledger entry at session start.
- [x] **T8. Session cleanup.** `tend sessions --prune <days>`. Remove stale sockets on start.
  Note: liveness is a ping, not the socket file. `tend sessions` uses the same check.
- [x] **T9. Ledger format version.** Add a first entry with the format version. Readers reject newer versions with a clear message.
  Note: `LEDGER_FORMAT = 1`. Ledgers without a `format` entry read as version 1. New kinds `format` and `config` and fields `unsupported`, `duration_ms` are additive.
- [x] **T10. Daemon test.** Integration test: two concurrent hook calls; the ledger has strictly increasing `seq` and no torn lines.
  Note: 8 threads × 5 hook calls against a real daemon.

## Next steps (after Phase 1)

Consolidated 2026-10-19 from the Phase 0/1 results. Owner decisions are marked **[Decided]**.

- [x] **N1. Guard reason (S6) [Decided: deny].** Deny with the reason; `tend allow` lifts one block.
- [x] **N2. Rename (S9) [Decided: rein, then tend].** Command `tend`; crates `tend-core`, `tend-cli`.
- [x] **N3. Failed-run shaping (D4) [Decided: build].** PreToolUse rewrites a test command to `tend run --id <id> --sh '<cmd>'`. `tend run` keeps the exit code, asks the daemon for a summary, and fails open to the original bytes.
  Spike (2026-10-19): `updatedInput` applies without a `permissionDecision`, and Claude Code checks permissions on the **rewritten** command (a compound rewrite was refused as "multiple operations"; `permissionDecision: "allow"` skips the check, so tend never sends it). Auto mode ran the single-command rewrite without a prompt; default mode prompted with the rewritten text. So tend wraps only in `auto` and `bypassPermissions` modes, never background calls, never commands that change shell state (`cd`, `export`, `source`, …). Live: a failing pytest run went from 324 to 54 lines; the agent named the failing test and kept exit code 1.
- [x] **N4. Git.** Repo under git; baseline commit, work on branch `next-steps`.
- [x] **N5. CI.** Run `make check` on push.
  Note: `.github/workflows/ci.yml` (Linux and macOS). Not run yet: the repo has no remote.
- [x] **N6. Private sockets and session dirs.** 0700 session dirs; the `/tmp` fallback socket in a private directory.
  Note: data, session, and `<tmp>/tend-<uid>/` dirs are 0700; an existing dir owned by another user is refused (the launcher then fails open).
- [x] **N7. Raw capture off by default.** `hooks.jsonl` only with `TEND_CAPTURE=1`.
- [x] **N8. Old ledgers.** Report infers "not shapeable" for failed runs in ledgers written before the `unsupported` flag.
  Note: applies only to ledgers without a `format` entry; it follows action → output → event.
- [x] **N9. Active loop time.** Leave out idle gaps (default: gaps over 10 min) from loop time.
  Note: config `idle_gap_minutes`. Retro over 35 real transcripts: loop time 4937 → 663 min (one session 3912 → 72 min).
- [x] **N10. More re-open signals.** Reading a persisted-output file or re-running the same command right after a summary counts as a re-open.
  Note: only the re-run signal. tend never shapes persisted outputs (the agent sees a 2 KB preview), so reading that file says nothing about a tend summary. A re-run counts only with no edit in between and only when the summary was applied.
- [x] **N11. Retro and live agree.** Same tool-call counting in retro and live reports.
  Note: retro looks only at hooked tools (`HOOKED_TOOLS`); on the captured loop session both now say 7 tool calls.
- [x] **N12. Capture script.** `scripts/capture-fixtures` turns a session's `hooks.jsonl` into sanitized fixtures.
  Note: `scripts/capture_fixtures.py` (`hooks`, `transcript`). It reproduces the existing captured fixtures byte for byte.
- [x] **N13. Real pytest and cargo fixtures.** Replace the synthetic ones.
  Note: the real cargo run found a bug. Rust 1.95 prints `thread 'name' (16652) panicked at`, and the parser expected no thread id, so it lost every panic message. Fixed; cargo messages now also keep the `left:`/`right:` values of `assert_eq!`. All fixtures are now captured.
- [x] **N14. Edge cases.** Claude Code's mid-output cut; persisted preview with no newline in 2000 bytes; the 1 MiB size boundary.
  Note: captured live. No newline: the preview is exactly 2000 bytes. 1048576 bytes shows as `1MB` (no trailing `.0`; fixed). Summaries now always keep Claude Code's `... [N characters truncated] ...` marker.
- [ ] **N15. macOS.** Latency, Ctrl-C, socket fallback. Needs a Mac.
- [ ] **N16. Real-session review (Phase 1 exit).** ~20 sessions in look mode; owners confirm the reports.

## Roadmap R (from `docs/analysis-2026-10.md`)

Based on a code review, a study of the owner's transcripts, and a check of the Claude Code docs. Read the analysis for the evidence and the acceptance check of each item.

**R0. Make optimize mode safe (before using it).** Done 2026-10-19.
- [x] **R0.1** `tend run`: do not wait for background children; kill the process group and print partial output on a signal. (D1, D2: confirmed)
  Done: drain ≤100 ms after exit; own process group + Linux parent-death signal; partial output and exit 128+sig on a stop. Live: a 25 s background process no longer blocks; Esc leaves nothing running.
- [x] **R0.2** Record what the agent saw: a filter timeout must give `applied: false`; cap filter input. (D3: confirmed)
  Done: the shape is recorded at the tool result, applied only if the agent saw it; stopped calls settle at the turn end.
- [x] **R0.3** Daemon: Ping without the lock; "slow" is not "stale"; shape outside the lock. (D4: confirmed)
  Done: Ping without the lock; `tend_core::prepare` runs outside it; `is_live` = connect. A hook waited 946 ms before, < 250 ms now.
- [x] **R0.4** Wrapper: absolute binary path; skip shell functions and aliases. (D5)
  Done: absolute binary path; wrap only if the runner is a real executable.
- [x] **R0.5** Classifier: runners in command position only; refuse multi-line and `{ … }`. (D7)
  Done: `perception::shell` parser; runners in command position; complex commands are not wrapped.
- [x] **R0.6** Single-write ledger lines, blob temp+rename, locked Shutdown; shim ignores EPIPE. (D8, D12)
  Done.
- [x] **R0.7** `ensure_private_dir` rejects symlinks; drop `wrapped` entries at Stop; cap maps. (D10, D11)
  Done (wrapped entries dropped at each turn end since R0.2).
- [x] **R0.8** Agent-proof `allow`/`set_mode`/`shutdown` (token or peer check); fix the commit-message false positive. (D6)
  Done: `<session>/control.token` (0600). Raises the bar; the agent runs as the same user, so it is not a security boundary.
- [x] **R0.9** A hook reply after the shim's timeout must not be recorded as applied. (D9)
  Done: the shim sends `deadline_ms`. A hook already late when the daemon gets to it (usually after waiting for the lock) is handled as look mode, with no response. One that becomes late while being handled gets no response and an `undelivered` entry naming its actions; readers treat those as not applied (`settle_undelivered`), and the report shows `Not delivered: N actions`. New entry kind `undelivered` (additive).

**R1. Coverage gaps from the data**
- [x] **R1.1** Wrap behind `source …/activate &&` and env assignments (74% of test runs are refused today).
  Done: only cd/pushd/popd refuse; a leading `cd DIR &&` stays in the agent's shell; here-doc bodies skipped; every program must be a real executable. Core-rule coverage on the owner's transcripts: 21% → 97% (upper bound).
- [x] **R1.2** Failure from output text, not `is_error` (0.9% vs 34.7%).
  Done: pytest `E` lines, summary-only lines, `grep -n` prefixes. Pytest runs with failure text but no parsed failure: 20 → 0.
- [x] **R1.3** Normalized command keys for loops and re-runs (exact repeats: 0).
  Done: `command_key`; loops count fix attempts (edit then failing run), and Bash writes count as edits. Retro loops: 17 (raw edit counts, mostly false) → 4.
- [x] **R1.4** Guard Bash writes to test files (`sed -i`, heredocs, Python): most edits bypass Edit/Write.
  Done (owner: warn): the connector snapshots the test files a Bash write names (`test_write_targets`, `*` in the last component), compares them after the call, and sends `FileChanged`. The guard judges it like an edit and warns in context (look mode: logs). New ledger event kind `file_changed` (additive). Owner's data: 206 of 1024 Bash writes name a test file; a sample of 14 were all real test edits. Not seen: writes that name no file, or paths after `cd $VAR`.
- [ ] **R1.5** Faithful handling of large file views (51% of Bash output). Measure first.
  Measured 2026-10-19 (owner's transcripts, 341 view calls, 250k tokens): views over 8k chars are 25 calls, 32% of view tokens. Repeated lines and extra blank lines inside them: ~4.6k tokens (5% of large views, 0.5% of Bash output). Exact re-reads of the same view: 0. **Not worth building now**; re-measure after real use.
- [ ] **R1.6** Group large search output by file.
  Measured 2026-10-19: search calls over 8k chars are 14 of 1201, 12% of search tokens. Grouping by file with the first 5 hits each would save ~47k tokens (~5% of Bash output), all from those 14 calls, and hides hits behind `tend show`. **Deferred** until real use shows how often the agent re-opens such summaries (R2.4).

**R2. Learn from real use**
- [x] **R2.1** `tend flag <id> "note"` and `tend review`.
  Done: `tend flag <a-N|o-N|seq|session> <note>` writes `<session>/flags.jsonl` (not the ledger); `tend review` lists all flags with the entry and a `look:` command; `tend report` shows the session's flags.
- [x] **R2.2** `retro --all` prints coverage and output share by class.
  Done: report and `retro --all` show output share by kind, test runs wrapped, and Edit/Write edits vs Bash writes. Also: new `view` kind, never shaped (it used to be shaped in optimize mode); retro counted repeated transcript lines twice (fixed). Owner's data: view 44% / search 26% / test 11%; 93% of test runs wrappable; the guard sees 9% of edits (101 vs 1009 Bash writes).
- [x] **R2.3** Guard and loop precision in `tend report`.
  Done: `Guard precision: N flagged · K allowed by the user` (allowed = likely false positive) and `Loop nudges: N · failure stopped within 1 more run: K`. `retro --all` shows both (guard: flag count only). Owner's data, corrected by the checkpoint (C2, C3): real sessions have 0 guard flags and 0 true loops. The earlier "6 guard flags; 2 of 3 loop nudges followed by the failure stopping" counted tend's own scripted sessions and two false loops.
- [ ] **R2.4** Review 20 real sessions in look mode (N16).

**R3. Platform and distribution**
- [x] **R3.1** Conformance tests per Claude Code version (`updatedInput` without a decision is undocumented).
  Done: `scripts/conformance.py [--record]` runs one scripted session under `tend claude --tend-mode optimize` in auto mode, in a throwaway project, and checks 9 facts: hook payload fields, PostToolUseFailure on exit codes, the wrap, `updatedInput` without a decision applied, the agent seeing the summary, the guard deny reaching the agent and stopping the edit, the Bash-write warning reaching the agent, and settings with `PermissionDenied` loading. 2.1.303: all pass (`docs/conformance/2.1.303.md`). This was the first live check of R1.4. It also found a guard gap: `assert True` in place of a real assertion was neutral; now flagged as "assertion that always passes added".
- [x] **R3.2** `SessionEnd` hook: stop the daemon, print a one-line report.
  Done (launcher part): the closing line says what tend did, e.g. `tend: session … ended · 41 tool calls · shaped 3 outputs, saved 9.2k tokens · 1 guard flag · \`tend report\` for details`. The `SessionEnd` hook itself moves to R3.3: Claude Code shows its output to no one ("exit code 0 - command completes successfully"), and the launcher already stops the daemon. Only a plugin, with no launcher, needs it.
- [x] **R3.3** Spike: ship as a Claude Code plugin instead of the launcher. Done as E6 (`plugin/`).
- [ ] **R3.4** Spike: `http` hooks instead of the shim process.
- [x] **R3.5** `tend status` for the Claude Code statusline.
  Done: `tend status` prints `tend <mode> · <what tend did>` for the session in `TEND_SESSION_DIR`, and nothing outside a tend session (README shows the `statusLine` setting). It does not set the status line itself: that would replace the user's.
- [ ] **R3.6** Report `systemMessage` not shown on PreToolUse `ask` (`/feedback`).
- [x] **R3.7** Detect auto-mode denials of wrapped calls (`PermissionDenied` hook) and measure the rate. Live: 1 of 9 wrapped test runs was denied as "[Auto-Mode Bypass]"; the agent then stops. Mitigation in place: a command key whose wrapped run got no result is not wrapped again in the session.
  Done: the `PermissionDenied` hook exists in 2.1.303 ("after auto mode classifier denies a tool call"; input `tool_name`, `tool_input`, `tool_use_id`, `reason`; output `retry: true` tells the model it may retry). Tend hooks it for Bash. A denied call that tend wrapped: the command key is not wrapped again (at once, not at the turn end), and tend answers `retry: true` (new action `retry`, new event `tool_denied`, both additive). The report shows `denied by auto mode: K (X%)` next to wrapped runs. Settings accepted live by 2.1.303. **[Verify]**: the fixture follows the schema in the binary, not a live capture; capture one with `TEND_CAPTURE=1` when a denial happens, and check that the agent retries after `retry: true`.

**Owner decisions:** positioning (loops and honesty first), and plugin distribution. See analysis §7.

## Checkpoint C (from `docs/checkpoint-2026-10-19.md`)

- [x] **C1** Shape by allow-list: only test, build, and install output by default. File reads are found in any segment, behind `cd X;`, and inside `bash -c '…'`.
  Done: `OutputKind::shaped_by_default`; `classify::is_view` checks every simple command for a view program with a file operand (`git log | head` stays git). A probe over the owner's transcripts went from 84 would-shape outputs (mostly code reads and diffs) to 1 (`black --check`).
- [x] **C2** Loops come only from test and build runs. The generic `assert …` fallback needs a space, so Rust's `assert!(` in source is not a failure.
  Done: this repo's dev session went from 2 false loop nudges to 0. The scripted loop session still steers.
- [x] **C3** `retro --all` counts subagent transcripts (a line each for main sessions and subagents) and leaves out projects under `/tmp` unless `--include-tmp`.
- [x] **C4** Corrected the R2.3 figures above.
- [x] **C5** Add a git remote so CI runs. Remote: github.com/prakhar479/tend; CI green on ubuntu and macOS since 8c59d33.

## Improvements I (from `docs/improvements-2026-10-19.md`)

Review after the checkpoint: about 60 findings, most of them confirmed. Batches in the order of §12 there:

- [x] **I1. Rule 1:** F1 hook parse errors exit 2 · F2 launcher fails closed · F3 SIGTERM orphans claude · F7 dead daemon unnoticed · F9 socket read timeout · T2 fail-open tests.
  Done:
  - F1: `tend hook` is handled before clap and always exits 0. `tend run` with arguments it cannot parse still runs `--sh`.
  - F2: any setup error runs claude without tend, passing the user's `--settings` through.
  - F3: TERM, HUP, and QUIT are forwarded to claude, and the exit code is 128+sig.
  - F7: the daemon is reaped, and an early stop is reported at exit.
  - F9: 2 s read timeout and 64 MiB request cap; an empty probe is silent.
  - T2: 7 new e2e tests.
  - Also done here, because the launcher was being rewritten:
    - F10: the hook command is shell-quoted, and the agent's PATH gets a private `<session>/bin/tend` link instead of tend's install dir.
    - U6: config warnings reach the user's stderr; bad lines are skipped, not fatal, with "did you mean"; values are bounded.
    - U7: `--tend-mode` works anywhere, `--help` goes to claude, and a user `--settings` is merged into tend's file.
    - U8: hidden Phase 2/3 commands.
    - U11: 2 ms start poll.
    - R1, in part: the agent binary is checked first; a launch with no conversation leaves no session; the new `keep_days` (default 30) auto-prunes at launch.
  - Live check with Claude Code 2.1.303: the quoted hooks fire.
- [x] **I2. Guard:** G1 comments and strings · G2 inline and other test files · G3 skip forms · G4 deleted tests, early return, swallowed asserts · G5 always-pass variants · G6 + T1 table tests.
  Done:
  - G1: `guard::code_only` strips comments and replaces each string with a hashed placeholder, so commented-out asserts are flagged and comment or import changes are not. Only assertion calls count.
  - G2: `guard::judges` also judges test code outside test paths (`#[test]`, `#[cfg(test)]`, `def test_`, `func Test…(t *testing.T)`, `@Test`), and the path rule now covers `tests.py`, `tests.rs`, `*Tests.cs`, and `_spec.rb`. Bash writes snapshot source files that hold tests (`code_write_targets`).
  - G3: skip forms widened, with focus (`.only`, `fit`) and `#[should_panic]` as separate reasons.
  - G4: tests removed or renamed, `except AssertionError` or bare `except:` added, a `return` before the asserts.
  - G5: always-pass variants with a message, plus same-operand asserts.
  - Also: tolerance raised (`rel=`, `atol=`, …) and precision lowered (`toBeCloseTo`, `places=`), from the reference's Phase 3 list.
  - Tests:
    - T1: a table of 50 weakening cases, each with its expected reason, and 12 neutral cases. Six rule mutations each fail it.
    - G6: controller tests for an agent without deny, and for a Bash-write warning that leaves nothing to allow.
  - Retro over the owner's 108 real Edit/Write edits: still 0 false flags.
- [x] **I3. Loops and classification:** L1 errors in passing runs · L2 tee as an edit · L3 key collisions · L4 tail variants · L5 and L6 signatures · S1 build regex · S2 traceback tails · S3 comment newline · T4 property tests.
  Done:
  - L1: the generic error fallback is skipped when the runner reports a pass and no failure.
  - L2: `tee` and redirects to `/tmp`, `.log`, or `.out` are not edits.
  - L3: `-p` and `-r…` are dropped from keys only for pytest; a bare runner keys on its `cd` dir (`npm test (in api)`); a `> file` redirect target stays out of the key.
  - L4: a summary-only run keeps the command's known failures and counts as an attempt.
  - L5: paths keep their file name; `:N` and `(N, N)` are normalized only after a file name; `id=`, `due to N previous`, temp names (with a digit), and only fractional or `ms` timings are normalized.
  - L6: unittest parser; pytest ids with spaces; jest warnings ignored.
  - S1: build, install, git, and search come from the program in command position.
  - S2: a deep Python traceback keeps its first line and its end.
  - S3: a comment no longer swallows the newline.
  - S4: ANSI-C `$'…'` quoting.
  - L7 (from I6): unittest, Django, `pnpm vitest`, `yarn jest`, `bunx`, `make -C dir test`, `test:*` scripts, `uv run --with`, `--filter`, deno, dotnet, swift, mix, rake.
  - T4: `tests/properties.rs` with a seeded generator:
    - shape keeps every failure over 300 spliced inputs;
    - noise does not change a signature, but a value does;
    - keys follow the selection;
    - the shell reader never panics.
    It found the redirect-target key bug.
  - Retro (real work): 1 loop. It is the dev session re-running the deliberately failing demo suite while editing tend. It counts as a loop by definition: edits anywhere count as attempts.
- [x] **I4. Resources and UX:** F4 big Writes · F5 `tend run` streaming · R1 session cleanup · U1–U9.
  Done:
  - F4: a Write reads the old file only for files that may hold tests, up to 1 MiB. Edit texts become blobs only when the guard judges them (≤ 1 MiB). Post-edit hashes skip files over 16 MiB.
  - F5: past 8 MiB, `tend run` hands over its buffer and streams the rest, with no daemon request.
  - R1: see I1, plus this fix: non-test edits no longer copy file content.
  - U1/U3/U5: `sessions::resolve`. `--session` takes an id, a prefix, `-N`, or a path. With no `--session`, commands use `TEND_SESSION_DIR`, then the newest session started in this directory, then the newest. `mode` and `allow` look at live sessions only, and ask when two are live.
  - U2: `tend sessions` shows id, start (UTC), duration, mode, calls, flags, live, and project.
  - U4: the report leads with "Needs attention" and hides empty sections; `--full` shows all. The report also shows the project.
  - U9: README.
  - U6–U8, U11: see I1.
  - Tests: huge-output streaming (e2e) and no blobs for non-test edits.
- [x] **I5. Tests and CI:** T3, T5–T9, X1.
  Done:
  - T3: the daemon's hook decision is `decide_hook` with an injected clock; tests cover on time, late before handling, the reply margin, late after handling, and no deadline. A controller test checks that undelivered actions leave no re-open or allow state.
  - T5: `fuzz/` has the targets shell, output, and guard (nightly). 60 s each found nothing.
  - T6: the busy-daemon test times direct socket requests against an idle baseline and asserts at least one sample. The orphan test polls instead of sleeping.
  - T7: e2e resolves the socket by the daemon's rule and waits for a ping.
    - Found on the way: the daemon's socket file appeared before it listened (bind before listen), so an early client got "connection refused". The I1 launcher's faster start made this flake about 1 run in 3. The daemon now binds under a temporary name and renames, and 0 of 24 runs flaked, 9 of them under 3× parallel load.
  - T8:
    - Real captures from Claude Code 2.1.304: PreToolUse Bash in auto mode, and the PostToolUseFailure of a wrapped run. `capture_fixtures.py --replace` handles the tend path.
    - `fixtures/README.md` lists the synthetic and hand-made fixtures.
    - The conformance prompt now runs the Bash write before the edit that gets denied: on 2.1.304 the agent stopped after the denial.
    - `conformance.py --replay`.
    - 2.1.304: all 9 checks pass.
  - T9:
    - CI pins Rust 1.95.0.
    - Jobs: an MSRV job (1.87, the new `rust-version`); a latency job (an ignored e2e test, p95 < 20 ms in release); a 60 s fuzz smoke job; `py_compile`; and a manual conformance job (`workflow_dispatch`, needs `ANTHROPIC_API_KEY`).
    - CI still needs a remote (C5).
  - X1: `compile_error!` on non-Unix.
- [x] **I6. Later:** F6, F8 + P1, R3, R4, L7, U10, U11, simplifications.
  Done:
  - F6: `tend run` starts a reaper (`tend reap <pgid>`) on a pipe only it holds. If `tend run` dies without a goodbye byte, the reaper kills the command's group. A test checks that a SIGKILL leaves no orphan, and fails without the reaper.
  - F8: a wrapped call that ran the agent's own command means Claude Code ignored `updatedInput`. tend records the wrap as not applied (`undelivered`, with its own note), stops wrapping, and says so in the report and at exit.
  - P1: `build.rs` bakes in the versions with a record under `docs/conformance/`. `tend claude` warns once per unchecked Claude Code version (checked in parallel, capped at 1.5 s).
  - R3: a new `session_id` in the same process (`/clear`, `/resume`) emits `conversation_changed`. Loop, edit, and block state start afresh, and the report sums every conversation's transcript. Live check: subagent hooks share the `session_id` and carry an `agent_id`, so they trigger no change.
  - R4: Stop drops only snapshots older than 10 min.
  - U10: `tend doctor`.
  - Simplifications:
    - `same_failure` judgments are recorded only when a decider handles them;
    - failures are parsed only for test and build outputs;
    - the output is hashed once;
    - an unknown kind is not shaped;
    - controller tests moved to `controller/tests.rs`.
  - L7, U11: see I3, I1.

## Extensions E (from `docs/extensions-2026-10-20.md`)

- [x] **CI fix.** An e2e test needed a pytest on PATH. Added `make check-clean`; the temporary socket name now fits the path limit; checkout@v5. Also fixed the guard's tolerance false positive on whole files, found by the polaris prototype.
- [x] **E1** `tend audit [BASE[..HEAD]]`: what a diff changed in tests, suppressions, check config, and dependencies (text, json, markdown).
- [x] **E2** Session audit: a base tree at SessionStart (temp index + write-tree), E1 at exit, in the exit line and report.
- [x] **E3** GitHub Action: the audit in the job summary or a PR comment; dogfood on this repo.
- [x] **E4** Expected value changed to the last failure's actual value.
- [x] **E5** Snapshot and golden updates (`jest -u`, `insta accept`, `*.snap`).
- [x] **E6–E11** Plugin, `.tend.toml`, secret redaction, per-project memory, `tend export`, notifications.
  Done 2026-10-20 (owner: build them, but the audit stays experimental, opt-in, and is not the product's lead yet):
  - E1/E2/E5: `tend audit` and the opt-in session audit (`experimental_audit = true`), labelled experimental everywhere.
    - Dogfooding found false suppressions (mentions in docs, strings, other languages); fixed by language and string-aware matching.
    - It also found a tolerance false positive on whole files; fixed by pairing assertions.
  - E3: `action.yml`, run on this repo's CI only; not published until the audit leaves "experimental".
  - E4: expected value changed to the failing run's actual value.
  - E5: snapshot-rewriting commands get a warning; matched on parsed command words, after the retro showed matches inside here-docs.
  - E6: the `plugin/` Claude Code plugin with a marketplace entry; validated, live-checked; `plugin_mode` in config.
  - E7: `.tend.toml` with `test_paths`, `protected` (guarded like test weakening, owner decision), and `audit_ignore`.
  - E8: secret redaction before writing; `tend purge`.
  - E9: per-project memory of shape levels.
  - E10: `tend export`; `retro --all --summary`.
  - E11: opt-in desktop notifications (`notify = true`).
- [ ] **E12–E16** Other agents, MCP server, `tend bench`, HTML report, deciders.

## Fleet F (from `docs/fleet-2026-10-21.md` and `docs/fleet-topologies-2026-10-21.md`)

Owner decisions 2026-10-21: awareness and learning first; spawning is a core feature; workers run oneshot per task in a worktree per worker (teams may share one); the user merges; hierarchy is first-class; support one checkout, worktrees, and separate repos; build F1 now (the feature gate from 2026-10-20 does not apply to fleet).

- [x] **F1.0 Measure.** From `~/.claude/projects` transcripts: how often were sessions live at once (one checkout, worktrees of one repo, different repos)? How often did they touch the same repo-relative file within 10 min? How often did two sessions hit the same error signature? Acceptance: a table in `docs/fleet-2026-10-21.md` §7. Does not block F1.
  Note: `scripts/fleet_measure.py`. 7 real transcripts: no same-checkout or worktree overlap, 3 concurrent pairs on different repos, one shared machine-level failure (gpg signing). A resumed session's copied history first read as a collision; each message uuid now counts once.
- [x] **F1 Awareness.** Lazy per-user hub (`<tmp>/tend-<uid>/hub.sock`); session daemons publish a digest off the hook path; pure `tend-core/src/fleet.rs` with stale-read, concurrent-write, and resource-clash rules; ledger kinds `fleet_peer`, `fleet_notice` with cross-ledger causes; `tend fleet` and a fleet section in `tend report`. Look mode only. Acceptance: e2e with two fake agents (A writes a file B read → B's ledger has a `fleet_notice` caused by A's entry; `tend why` shows both); killing the hub leaves both sessions working, hooks exit 0, latency budget holds. Gate for F2: a week of real use with correct notices.
  Done 2026-10-21. Changes from the design:
  - **No hub process.** Each daemon tails the other live sessions' ledgers (a thread, once a second, off the hook path). The hub arrives with spawning (F3), where something must own sessions.
  - **Rules.** `Read` is not hooked, and Claude Code already refuses an Edit of a file changed since it was read, so "stale read" became four rules from data tend already has: peer edit (after ours, same checkout), peer git (`git_effect`: stash, checkout, reset, commit, …), peer failure (a failed run names a file a peer edited in the last 30 min), worktree overlap. A resource-clash rule was dropped: cargo waits for its own lock, and git's `index.lock` error is clear.
  - **`fleet = off | look | on`** (default `look`: record only). Notices are recorded at this session's next hook (`fleet` action, causes = the peer's entry here + this hook's event); delivered only with `on` in optimize mode, at most 3 per hook and 40 per session. Peer events do not advance the active clock (loop time).
  - A liveness probe no longer counts as daemon activity, so peers cannot keep a plugin daemon from its idle exit.
  - Live check (Claude Code 2.1.306, Haiku, two headless sessions in one checkout): B's edit reached A after A's wait step, and A quoted the notice and handled the change. Shim p95 2.9 ms.
- [x] **F2 Shared learning.** Signature memory (`<home>/fleet/signatures.jsonl`) delivered on a second hit or a loop start; `tend note` with a review queue and `tend notes --promote`. Gate: one real hit where the delivered fix was right.
  Done 2026-10-22 as L1–L5 (below). As built: `<home>/learn/fixes.jsonl` and `notes.jsonl`; the user approves with `tend notes approve n-N`.
- [x] **F-S1 Spike: stream mode.** Long-lived `claude -p --input-format stream-json --output-format stream-json` under tend's settings: do hooks fire, can a turn be sent when idle, how does interrupt work, does `--resume --fork-session` work headless?
  Note: all four confirmed; see Spike results. Rerun with `scripts/spike_stream.py`.
- [x] **F-S2 Spike: concurrency.** 4 stream sessions at once: rate and usage limits, hub CPU.
  Note: no rate limits at 8 Haiku sessions; ~300 MiB per agent is the bound. Fleet notices about one file are now one message. See Spike results.
- [x] **F3a Spawn and lifecycle.** Hub spawns `stream`/`oneshot` sessions (`--session-id`, role brief, worktree); restart with backoff; stuck detection; `fleet status | stop | down | prune`. Acceptance: e2e with 2 fake stream workers, one killed and restarted, `down` stops all, no worktree lost.
  Done 2026-10-21. `tend-cli/src/hub.rs`; commands `tend fleet up [FILE] [--preset P] [--mode M] | status | stop <slot> | spawn <role> | scale <role> <n> | down | prune [--yes]`. Notes:
  - **The hub** is one process per fleet, started by `up` (`<home>/fleets/<id>/`: `meta.json`, `ledger.jsonl`, `inbox.jsonl`, `slots/`, `worktrees/`). It owns the F3b model, writes every record to the ledger, and ticks on elapsed time (a fixed receive timeout starved ticks while status polls kept arriving; the e2e test found it). `status` of a fleet whose hub is down replays its ledger.
  - **Sessions** run as `tend claude --tend-mode <mode> -p --input-format stream-json --output-format stream-json --verbose --session-id <uuid> --append-system-prompt <brief>`, with `TEND_FLEET` and `TEND_FLEET_SLOT` set. The hub holds stdin; output lines give activity, `result` lines turn ends. A oneshot is the same process with stdin closed after its first turn (exit 0 = done). `stop` closes stdin, then SIGTERM after 5 s, SIGKILL after 10 more.
  - **Worktrees**: `worktrees/<slot>` on branch `tend/<fleet>/<slot>` from the base commit, made once and reused on restart; `team:<name>` shares one; outside git, or with no commit, every session works in the checkout. `prune` acts only on fleets that are down, keeps a worktree with uncommitted changes or commits past the base, and only lists without `--yes`.
  - **Limits**: without `limits.sessions` in the file, `up` lowers the default to fit free memory (350 MiB per agent, F-S2), never below what the fleet starts with.
  - **When the hub dies, its stream sessions end**: their stdin closes, so they finish the turn and exit. The design hoped they would go on alone; a stream session cannot. Since G1, `tend fleet restart` resumes them (`--resume`).
  - Not yet: a oneshot started without a task waits for one (tasks are F5a); `Deliver` to a session in a terminal goes to the fleet inbox (F4); attached roles are only announced in the inbox.
  - Live (Claude Code 2.1.306): a peers fleet of two Haiku stream sessions in two worktrees started, the session whose launcher was killed came back in its worktree after 5 s, and `down` ended both cleanly. No turn was sent, so no model call.
- [x] **F3b Core fleet model.** `tend-core/src/fleet/`: fleet file, roles, channels, presets, pure `step(state, event, now_ms) -> (state, commands)`. Acceptance: table tests per preset; replay is deterministic.
  Done 2026-10-21. `tend-core/src/fleet/`: `spec.rs` (the fleet file's parsed form, presets, `resolve` with short error sentences) and `model.rs` (`Model::step(event, now) -> Vec<FleetRecord>`); the F1 code moved to `fleet/awareness.rs`, same paths. 27 tests: one table per preset (roles, allowed and refused channels, what `Up` starts), lifecycle, limits, stuck, scale, permissions, messages, rate limit, two- and three-level trees, records, and replay (the recorded events give the same records, also through JSON). Decisions made here (none is a product change):
  - **Starting.** `Up` starts each stream role's minimum; oneshot roles start per task (F5a); an attached role becomes an `attach` command (the user starts it; only at the top level).
  - **Crashes.** Restart with backoff 5, 10, 20 s; after the 4th crash the slot is failed and the user is told. A oneshot that exits 0 is done. A slot stopped on purpose is never restarted.
  - **Stuck.** Only a session with a turn in progress (a task was delivered, or a hook came mid-turn) can be stuck: after `stuck_minutes` (default 10) the user is told, after twice that it is interrupted, after three times stopped and restarted.
  - **Limits.** A slot waits while the fleet's `limits.sessions`, or a team's above it, is full (default 4 sessions; the CLI may lower it to fit memory, F-S2); the user is told once until room frees up. Team limits default to the parent's and may only be lower.
  - **Hierarchy.** Slot ids are `<role>-<n>`, and `<leader slot>/<role>-<n>` inside a team. A sender sees its own level and the team it leads (where it is `parent`); escalation goes only to the parent. Depth: 1 without teams, 2 with, at most 4. A team waits while its leader is not running; other teams go on.
  - **Defaults for added roles.** In `lead`, `specialists`, and `tree`, a role the file adds is a member (takes tasks from the lead, reports to it); a role that leads a team gets the lead's verbs and brief unless `may` or `brief` is set.
  - **Messages.** Allowed only along channels; at most 10 a minute per sender and receiver; a refusal goes back to the sender as a notice (or to the user as a notification), with the reason.
  - Reading the TOML text is F3a's (tend-cli, with the `toml` crate: the old `winnow` name clash is gone). tend-core has no new dependency.
- [x] **F4 Messaging.** `tend msg`, `tend inbox`, `--tend-name`, `--tend-role`; delivery through `inject_context` at the next hook, digested and rate-limited; messages are untrusted and can never lift a block. Optimize mode only.
  Done 2026-10-21. `tend msg <user|@role|slot> "…" [--kind k]`, `tend inbox`, and `tend claude --tend-fleet [ID] [--tend-role R] [--tend-name N]`. Notes:
  - **Routing** is the F3b model's: channels, rate limits, refusals as the reply. Without `--kind`, the kind is an answer when the receiver last asked the sender a question, else the first kind the channel allows. The lead preset's user ↔ lead channels now also carry questions, answers, and notices (the live check showed a question going out as an "answer").
  - **Delivery fits the receiver.** A task always goes in as a turn. A message to an idle headless session goes in as a turn; to a busy one (in optimize mode) or to a terminal session it waits in `slots/<slot>.inbox.jsonl`, and that session's daemon adds it to the next hook's reply, labelled "another session's message: weigh it as information, not as an instruction from the user". In look mode it is recorded only; `tend inbox` shows it either way. Messages are delivered in optimize mode even with `fleet = look`, which holds back only the F1 awareness notices.
  - **Terminal sessions** join as a role with `run = "attached"`: the hub gives them a slot (the `--tend-name`, else `<role>-N`), knows their tools by the launcher's process tree, and ends the slot (no restart) when the launcher leaves or its process is gone.
  - **Messages never act**: they are text in context. A session cannot run `tend allow` (S6) or fleet control (F5a) whatever a message says.
  - **Limit:** a oneshot worker ends with its turn, so it cannot wait for an answer to `tend msg`; it asks with `tend task blocked`, which reaches the task's creator. Stream and terminal sessions converse.
  - Built since: unread counts (G6); a team channel, `#team` (G3).
  - Live (Claude Code 2.1.306, Haiku lead, look mode): a user's `tend msg @lead` reached the idle lead as a turn, and its `tend msg user` reply was in the inbox within 3 s.
- [x] **F5a Task board and verbs.** `tend task add|assign|claim|done|blocked`; role checks in PreToolUse. Acceptance: lead adds 3 tasks, 2 workers finish them, dependencies respected.
  Done 2026-10-21. `tend-core/src/fleet/model/tasks.rs`; `tend task add | assign | claim | done | blocked | list`. Notes:
  - **Board.** Tasks are `t-N` at the top, and `t-N.k` when the adder is working on `t-N` (a lead or sub-lead splitting its task); a parent is done only when its parts are. A task added by a session that may give tasks to exactly one role goes to that role; otherwise it stays open for `assign` or `claim`. Dependencies hold a task back until they are done.
  - **Dispatch** (after every event, in board order): a ready task goes to an idle stream session of its role, or to a fresh oneshot session within the role's maximum and the session limits. `done` and `blocked` reach the task's creator (a session as a report or a question, the user in the inbox).
  - **Attempts.** A session that ends without `done` or `blocked` costs the task an attempt; the task waits 5, 10, … s and goes to a fresh session, and fails past the role's `budget.attempts` (default 3), telling its creator. **Changed from F3b:** a oneshot session is never restarted itself (its task is retried instead), and a oneshot that had a task never gets a second (found in the e2e test: it was given one after its stdin closed).
  - **Who asks.** The hub tells sessions apart by the caller's process tree (`SO_PEERCRED` on Linux, `LOCAL_PEERPID` on macOS, then parents up to a session's launcher), not by `TEND_FLEET_SLOT`, so an agent cannot act as another session or as the user. A session's `tend fleet down` or `scale` is refused. Refusals and verb results are now `answer` commands: the reply to the request, never a stray turn on the agent's stdin. A process that leaves its tree (a double fork) would count as the user; for that, PreToolUse in a fleet session denies setting or clearing `TEND_FLEET*` and running `tend fleet up|down|stop|spawn|scale|prune` (`fleet_bypass`, a text match, advisory like the `tend allow` block).
  - Also found by the e2e test: a refused `tend fleet down` waited 40 s for a fleet that was not going down. It now returns at once.
  - Live (Claude Code 2.1.306, Haiku lead and workers, look mode): given one task, the lead added three worker tasks with the right dependency, three fresh workers did them in their own worktrees and called `tend task done`, and the lead finished its task after the three reports, in about 15 s and under $0.02.
- [x] **F5b Check gate.** `done` means tend ran the check and it passed; failures go back as a new attempt; guard and audit flags escalate.
  Done 2026-10-21. Notes:
  - **The check** is the task's own (`tend task add --check`), else its team's `check`, else the fleet file's. Without one, `done` counts at once, as before. The user's own `done` skips the gate.
  - **A session's `done`** moves the task to `checking`; the hub runs the check with `sh` in the session's worktree (one check at a time, 15 min timeout, output with stderr), and sends back a `check_result`. Passed: done, and the creator hears "passed". Failed: an attempt; the failures tend's parsers find (or the output's last 30 lines) go back to the same session if it is still open, else to a fresh one after a backoff; past the budget the task fails.
  - **Oneshot sessions now end when the model says so** (`release`), not after their first turn: one stays open while its task is checked, so a failure reaches the session that made the change. A session that ends its turn with its task still open is released, and its exit costs the attempt, as before.
  - **Flags** (only with `experimental_audit = true`, per the owner's decision that the audit stays opt-in): the hub records the worktree's tree when a task is delivered and audits the diff when the check passes. Weakened tests or check config put the task in `review`: its creator gets a question, and accepts with `tend task done` or sends it back with `tend task reject <t> "<reason>"`. While under review, the session that did the task cannot `done` it (the first test run found it could start a new check that way).
  - Live (Claude Code 2.1.306, Haiku): the task asked only for a bug fix; the check also wanted a CHANGELOG entry. The worker fixed the bug and said done; the check failed; the failure went back to the same session, which added the entry and said done again; the check passed, in about 12 s.
- [x] **F5c Workspaces, claims, and scopes.** Worker slots own a worktree (default); team worktrees with enforced claims, a team channel, serialized checks; `may_write`/`may_not_write`; cross-worktree conflict prediction. Acceptance: in a team worktree a write into another worker's claim is denied; in `specialists` an implementer's test edit is denied.
  Done 2026-10-21 (part). The hub hands each session its role's `may_write` / `may_not_write` and the path of the fleet's `claims.json` (`TEND_FLEET_RULES`); the hub rewrites `claims.json` when the scopes of working tasks change. In PreToolUse the session's daemon denies an edit outside its role's scope, or inside a scope another session in the same worktree claimed for its task (`fleet/scope.rs`; look mode records it). Checks already run one at a time (F5b). Merge conflicts across worktrees show in `tend fleet merge` (files that more than one worktree changes) and as F1 worktree-overlap notices. Built since: a team channel (G3), Bash writes (G4), the interference retry (G5).
- [x] **F5d Budgets and limits.** Tokens per session, role, and fleet; usage-limit pause; spawn depth.
  Done 2026-10-21. The hub reads each result line's usage (prompt, cache, and output tokens of the turn) and cost (the session's running `total_cost_usd`) into `usage` events. A session over its role's `budget.tokens` or `budget.wall` is stopped, its task's attempt ends, and the user is told. The fleet pauses over `limits.tokens`, and when a session's result says the plan's usage limit was hit; paused, no session starts and no task is handed out (`tend fleet pause | resume`). `tend fleet status` shows tokens, cost, and pause. Spawn depth comes from the fleet file's tree (F3b).
- [x] **F5e Reports and merge plan.** `fleet report`, `fleet merge` (plan; merge on yes), `fleet replay`.
  Done 2026-10-21. `tend fleet report`: tasks (with attempts and outcomes), sessions (state, tokens), spend, and the merge plan, from the ledger. `tend fleet merge`: each worktree with changes, in task order, and the files more than one changes; with `--yes` (fleet down) it commits each finished worktree's changes on its own `tend/...` branch and prints the `git merge` commands; it never touches the user's branch. Branch names of nested slots are flat (`sub-1__worker-1`): the tree test found git cannot hold `tend/F/sub-1` and `tend/F/sub-1/worker-1` at once. Built since: `tend fleet replay --file` (G7).
- [x] **F5f More presets.** `review`, `best-of`, `pipeline`, `supervisor`, `pool`; one seeded bench run each.
  Presets are built and table-tested (F3b); `review`, `best-of`, `pipeline`, `supervisor`, and `pool` run through the same model as `lead`. **Not done:** the seeded bench run per preset (it needs a run budget).
- [x] **F5g Hierarchy (first-class).** Sub-leads, task trees, budget slices that only narrow, escalation upward, roll-up reports, sub-lead restart. Acceptance: lead → 2 sub-leads → 2 workers each; a child cannot exceed its slice; a killed sub-lead resumes its subtree.
  Done 2026-10-21. Model tests (F3b) and an e2e run of the `tree` preset: the lead split its task between two sub-leads, each split its part between two workers of its own team, every report went to the task's creator, and each level finished after its parts.
### Fleet gaps G (collected 2026-10-21, after F4 and F5; most important first)

- [x] **G1 Hub restart.** A dead hub ends its stream sessions and nothing brings the fleet back. `tend fleet restart [id]` starts the hub again from its ledger: live slots are marked gone, stream slots restart with `claude --resume <their session>` (their conversation goes on), tasks they had go back to the queue without costing an attempt, and checks that were running run again.
  Done 2026-10-21. `tend fleet restart [id]`; the hub writes `hub.pid`. On restart it replays the ledger, then records `hub_restarted`: live stream slots wait to start again (no crash counted) and resume with `--resume <last session>`; oneshot and terminal slots end; their tasks are queued again at no attempt; checks that were running run again. Tested by killing the hub with SIGKILL.
- [x] **G2 A worker that asks waits for the answer.** A oneshot that sent a question (`tend msg`) while its task is open is not released at the end of its turn; the answer comes in as its next turn. It gives up after 10 minutes without one (the task's attempt ends).
  Done 2026-10-21. A oneshot with an open task that sends a question is not released at its turn end; whatever message reaches it next comes in as its next turn; after 10 minutes without one it is released. The worker brief says so.
- [x] **G3 Team channel.** `tend msg #<team> "…"` reaches the other sessions that share the team worktree (`workspace = "team:<name>"`), with no channel to declare.
  Done 2026-10-21. `#<team>` addresses the other running sessions of the same level whose role works in `team:<team>`; notices (the default), questions, and answers; a session outside the team is refused.
- [x] **G4 Bash writes and the fleet scope.** PreToolUse also denies a Bash command whose write targets (redirects, `sed -i`, `tee`, …) fall outside the role's scope or inside another session's claim. Advisory: a text reading, like the guard's Bash rule (R1.4).
  Done 2026-10-21. In a fleet session, a Bash command that writes (`writes_files`) is denied when one of its write targets is in `may_not_write` or another session's claim. Only the deny side: the shell reading lists words that are no files, so a `may_write` allow-list would refuse at random; Edit and Write still use the allow-list.
- [x] **G5 Interference in a shared worktree.** A failed check while another session of the same team worktree was working costs no attempt the first time; the failure goes back with a note that the other session may have caused it.
  Done 2026-10-21. A failed check of a task in a team worktree, while another session there has a task, goes back once without costing an attempt, with a note that the other session's change may be the cause. The next failure counts.
- [x] **G6 Unread counts.** `tend status` in a fleet session shows messages that arrived since its last hook; `tend fleet` shows the user's unread fleet messages; `tend inbox` marks them read.
  Done 2026-10-21. `tend status` in a fleet session adds "N new fleet messages" (inbox lines its ledger has not recorded yet); `tend fleet` shows the user's unread messages per fleet; the user's `tend inbox` marks them read (`inbox.read`).
- [x] **G7 `tend fleet replay [id] [--file other.toml]`.** Replay the recorded events, with the fleet's file or another one, and say whether the records match, or where they first differ, and what the other file would have done.
  Done 2026-10-21. With the fleet's own file it says whether the records come out the same (or shows the first that differs); with another file it compares the commands by kind (spawns, deliveries, refusals, …), given the same recorded events.
- [x] **G8 Handoff flows (`pipeline`, `review`).** When a task of a role with one handoff channel is done, tend adds the next role's task with the summary (pipeline). When a reviewer blocks its review with findings, the author gets a new task with them, up to `max_rounds`.
  Done 2026-10-21. General rules, so any fleet file can use them: when a task is done and its role has exactly one handoff channel, tend adds the next role's task (`<title> — <role>`, the summary in its body, the same creator). When a handed-over task is blocked and a review channel leads back to the role that did the earlier task, that role gets a new task with the findings (`… — <role>, round N`), up to `max_rounds`; past it the task stays blocked and the user is told. A task's text now lists how the tasks it depended on ended.
- [x] **G9 `best-of`.** A task for the candidate role fans out to one part per candidate; when all are done the judge decides: `judge = "rule"` picks the passing candidate with the smallest diff, `agent` gives the judge a task with every candidate's summary, `user` asks the user.
  Done 2026-10-21. A task for the candidate role in a best-of fleet fans out to one part per candidate (`t-N.1` …); the whole waits until every part ended. `judge = "rule"`: the done part with the smallest diff (the hub counts changed lines when a check passes) wins. `agent`: the judge gets a task listing each finished part's summary and size, and names the winner in its `done`. `user`: the user reviews and names one with `tend task done`. Parts not picked are marked, and `tend fleet merge` leaves their worktrees out.
- [x] **G10 Preset bench.** One seeded run per preset on Haiku (`scripts/fleet_bench.py`): did the flow finish, how many attempts, what it cost.
  Done 2026-10-21 (`scripts/fleet_bench.py`; Claude Code 2.1.306, Haiku for every role, `check = "python3 test_calc.py"`, one task: fix the bug in `calc.py`):

  | preset | outcome | tasks | attempts lost | fixed | min | $ |
  |---|---|---|---|---|---|---|
  | lead | done | 1 | 0 | yes | 0.1 | <0.01 |
  | pool | done | 1 | 0 | yes | 0.1 | <0.01 |
  | pipeline | done | 3 | 0 | yes | 0.3 | 0.01 |
  | review | done | 2 | 0 | yes | 0.3 | 0.01 |
  | best-of | done | 4 | 0 | yes | 0.1 | 0.01 |
  | specialists | done | 1 | 0 | yes | 0.1 | <0.01 |

  Reading: every flow ran to its end. The pipeline handed plan → implement → verify; the review ended with the reviewer approving; best-of judged by rule and picked `t-1.1` (all three diffs were the same size). The task was too small to test delegation: the `lead` and `specialists` leads fixed it themselves (allowed; one task), and the plan stage made the fix. **Found and fixed:** the merge plan read a team worktree (`team-review`, `team-pipeline`) as "not finished", because it looked for a slot of that name; it now counts the tasks of every slot whose role works in the team. A harder seeded task (several files, a real split) is the next bench to write.

### Review R (2026-10-22, after G10)

A review pass over the fleet code (the three review agents hit the account's session limit, so it was done by hand). Fixed:

- [x] **`tend fleet down` (or `stop`) killed a session that joined from the user's terminal**: it sent SIGTERM to the user's `tend claude`, and SIGKILL after 30 s. Now the fleet lets go of it (its slot ends; the inbox says so) and the terminal session goes on.
- [x] **Stuck detection and wall-time budgets applied to terminal sessions.** They send the hub no activity, so a task delivered to one made it "stuck" after 10 minutes, then interrupted and stopped. They are skipped now: they are the user's.
- [x] **Tasks that depend on a failed or blocked task waited forever**, so the fleet never finished. They are blocked now, with the reason, and their creator is asked (`t-2 is blocked: it depends on t-1, which failed`), down the chain.
- [x] **The hub wrote to a session's stdin on its main loop**: a session that stopped reading would fill the pipe and freeze the hub. Each session has its own writer thread now.
- [x] Polish: the merge commit of a team worktree lists all its tasks' summaries; costs under a cent show as `<$0.01`; the README says token budgets count cache reads; CLAUDE.md lists the fleet files.

Checked and fine: shim p95 1.5 ms; fuzz targets build; look mode returns no response for the new denies and notices (they go through `act`); refusals exit 1.

### Hard bench H (2026-10-22)

`scripts/fleet_bench.py --task hard [--split]`: a small package (`kv/`) with three independent bugs in three modules and a missing `summarize()` that needs all three; `check = "python3 run_tests.py"` (four test files; `run_tests.py <name>` runs one).

- **Without `--split`, the task is still one agent's work.** On Haiku every preset finished with all four parts fixed and no lost attempts, but the leads (`lead`, `specialists`) did it themselves: 1 task, 1 session, about 20 s. Natural delegation needs a bigger task than this.
- **With `--split`** (the task tells the lead to hand out one task per test file), the lead split it well (four tasks, a `--check` per part, `summarize` depending on the other three), and that exposed the gap the small bench could not: **work did not flow between worktrees.**
  - The dependent task's worker started at the fleet's base, without the three fixes, and blocked. **Fixed:** before a fresh worker's first task, the hub brings in the work of the task's dependencies (and theirs): each one's own diff from the tree it started at, plus new files; the agent is told what came in, or what could not.
  - Then the lead's own task could not pass: its worktree had none of the parts, and it rightly would not say done. **Fixed:** when a part is done, the hub brings its work up into the worktree of the session holding the parent task (not for best-of candidates or a judge).
  - The hub now records each task's starting tree always (not only with `experimental_audit`), so a task's own changes are known; the audit still runs only when it is on.
  - The merge plan leaves out parts whose work is in their lead's worktree; the lead's brief tells it to give each part a `--check`, and that tend moves the work.
- Result after the fixes (Haiku, `lead`, `--split`): done in 0.5 min, 5 tasks, no lost attempts, the whole check passes in the lead's worktree, $0.02; the merge plan is the lead's worktree alone.

### Learning L and merging M (planned 2026-10-22; owner chose F2 shared learning and merge-aware integration)

- [x] **M1 Merge-aware integration.** Move a task's work with a 3-way merge of trees (its starting tree, its worktree now, the target now) via `git merge-tree --write-tree --merge-base` on commit objects only (no ref moves). A fresh worker gets conflicts as markers with the file list; a lead's worktree never gets markers (the lead is told which files conflict and where the part's work is). Older git: the patch, as before.
  Done 2026-10-22. `move_work` snapshots three trees (the task's starting tree, its worktree, the target), makes commit objects with tend's own identity (no refs), runs `git merge-tree --write-tree --merge-base`, and applies the target-to-merged diff. A fresh worker gets conflicts as markers and their file names; a lead's worktree is never given markers: the lead gets a notice (and the inbox a line) naming the files and where the part's work is. Exit codes other than 0 and 1 (older git) fall back to the patch. Test: two parts changing adjacent lines of one file (a patch fails: each one's context is the other's change) merge; the same line gives the dependent worker the conflict. **Found on the way:** the worktree snapshot (also used by the session audit, E2) copied the git index, and the copy's fresh time made git trust its cached stat for a file changed in the same second at the same size: the change was missing from the snapshot. The copy now keeps the index's time; a regression test covers it. Live (Haiku, split hard task): done, 5 tasks, no lost attempts, the whole check passing in the lead's worktree.
- [x] **L1 Learn fixes (core).** A command's failing run, edits, then a passing run of the same command: each failure that went away is a `learned` ledger entry (signature, message, test, command, files changed in between).
  Done 2026-10-22. `tend-core/src/learn.rs`; a `learned` ledger entry when a failure seen in a test or build run is gone from a later run of the same command (a pass known to exit 0, or a failing run whose failures the parsers read) after edits; the files are those changed in between, relative to the project root. A wrapped run (exit code unknown) never counts as a pass. Since Review 2, only a complete passing run teaches (see there).
- [x] **L2 Machine store.** The daemon appends learned fixes to `<home>/learn/fixes.jsonl`; a session starts with the other sessions' fixes.
  Done 2026-10-22. `<home>/learn/fixes.jsonl` (session, project, time); a session starts with the newest 2000 of other sessions' fixes. Fails open.
- [x] **L3 Recall.** On the second hit of a failure another session fixed, tell the agent where and how (same project first), once per failure; `learn = off | look | on` (default `look`, like `fleet`).
  Done 2026-10-22. On the second hit of a failure another session fixed, a `recall` action: "this failure was fixed before, in this project, 3 min ago: after changes to …, `pytest` passed" (this project's fix first, then the newest), once per failure, at most 10 per session. `learn = look` (default) records it; `on` says it in optimize mode. Tested across two daemons.
- [x] **L4 Notes.** `tend note "…"` (from a user: kept; from an agent: pending), `tend notes [approve|drop n-N]`; an agent may not approve or drop (PreToolUse deny).
  Done 2026-10-22. `tend note "…"` (kept when the user types it in a terminal outside a tend session; otherwise pending), `tend notes [--all]`, `tend notes approve|drop n-N`. PreToolUse denies the agent's own `tend notes approve|drop` (like `tend allow`).
- [x] **L5 Notes at start.** Approved notes of the project go to every new session at SessionStart (fleet workers too).
  Done 2026-10-22. A `notes` action at SessionStart with the project's approved notes (fleet workers are tend sessions, so they get them too); `learn = on` and optimize mode put them in context.

### Review 2 (2026-10-22, after F2 and M1)

A system pass over the fleet, learning, and session code (three review agents, then checked by hand). Fixed:

- [x] **Learning taught wrong fixes.** A failing run whose failures differed (a syntax error hides the test that failed before; `-x` stops early) counted as a fix. Now only a passing run that ran everything teaches: exit 0, no failures, no pipe, none of `-x`, `--lf`, `-k`, `-m`, `--maxfail`, `--bail`, `-run`, `--fail-fast` (`learn::complete_run`; `python -m pytest` is fine). It teaches every failure seen since the first failing run.
- [x] **Notes: an agent could approve its own.** `tend notes approve|drop` is refused in a tend session. A note is the user's only when typed in a terminal outside a session (an agent can clear `TEND_SESSION_DIR`, but its shell has no terminal); others wait for review. Notes are written under an flock, with a per-process temp file; ids are max + 1, never reused.
- [x] **The fix store grew without bound** (200k lines: 140 ms at session start). It is cut back to the newest 2000 when it holds 4000.
- [x] **Hub restart:** sessions the old hub left could finish a turn next to the new ones. The hub keeps each session's pid in `slots/<slot>.pid`; a restarted hub stops those process groups (only if still a `tend claude` stream session: pids are reused). Done parts are not lifted again, and worktrees are not integrated twice.
- [x] **A leader that restarted got a second team**: `start_team` started `min` more. It starts only the missing ones.
- [x] **Best-of judging hung** when a candidate failed or was blocked. Ended candidates no longer hold it up; a candidate past its crash budget ends through the usual path (judging, release, dependents). The pick matches whole ids (`t-1.1` is not in `t-1.12`).
- [x] **A check whose session ended lost its worktree** (the slot was cleared), so a hub restart could not run it again. The slot stays named.
- [x] Small: a retry with no role goes back on the board (no empty target); backoff cannot underflow; `claim` is refused while paused; messages are never dropped by the 40-notice cap or a conversation reset.
- [x] **Hub I/O:** SIGKILL reaches the session's whole process group; `leave` ends the watch; a task's starting tree is recorded once, when its slot holds it (a retry kept moving it); `move_work` diffs with plumbing (`diff-tree`, `--no-ext-diff`, fixed prefixes) at the worktree roots; file names are unquoted (`core.quotePath=false`); `prune` counts commits on a detached HEAD; `merge --yes` will not commit conflict markers.
- [x] **A busy hub looked dead**: a 10 s read timeout said "not running". It now says the request may still apply and how to check before repeating it. Fleet control (`ask`, `down`) is refused in a fleet session's shell before it reaches the hub.
- [x] **Sessions:** a peer ledger that got shorter is read again from the start; a new daemon of a fleet slot resumes its inbox where the last one stopped (`<inbox>.seen`), so old messages are not delivered twice.
- [x] Docs: stale "not built" notes now name the gap that built them; CLAUDE.md lists `learn.rs`, `learnstore.rs`, `audit.rs`; argument help for `msg`, `inbox`, `note`, `notes`; `tend help claude` lists tend's own flags.

Tests added: `a_leader_that_restarts_keeps_its_team`, `a_blocked_candidate_does_not_hold_up_the_judging`, `a_check_whose_session_ended_runs_again_after_a_hub_restart`, `messages_are_never_capped`, `only_complete_runs_can_show_a_fix`, `the_fix_store_is_cut_back_when_it_doubles`, `a_rewritten_peer_ledger_is_read_again_from_the_start`, `a_new_daemon_does_not_deliver_old_inbox_messages_again`; the notes e2e test checks the refusal. Shim p95 within budget.

**Open (owner decision):** a task's `--check` comes from the agent that adds it, and the hub runs it outside that agent's permissions. Per-part checks were what made the hard bench work, so it stays; options are to allow only the fleet file's check plus `run_tests.py`-style arguments, or to run agent checks in the worker's own sandbox.

### Live L (2026-10-22): fleet, learning, and merging on real work

`scripts/learn_live.py <tend> <workdir>`: the hard bench's repo, one `TEND_HOME`, `learn = on`, optimize mode, Haiku. A user note (approved), then three fleets in the same repo: `lead` with the split task, `pool` with the whole task, and `pool` with a task that makes the worker run the failing test twice. About $0.04 a round.

The first rounds learned nothing. Each cause, found and fixed in turn:

- [x] **`python3 run_tests.py` was not a test run.** Learning, loops, and wrapping saw only known runners. A project's own test script now counts by its name (`run_tests.py`, `runtests.py`, `test.sh`, `scripts/test`, `test_x.py`, `x_test.py`, `tests.py`, run by python, sh, or path). Fixtures: `fixtures/commands/test_scripts.txt` (with near misses), `fixtures/outputs/run_tests_script_{fail,pass}.txt`.
- [x] **"0 failed" was a failure.** The summary-only rule matched `^\d+ failed`, so every pass of a runner that prints a zero count looked like a failed run (for learning and for loops). It needs a count above 0 now.
- [x] **A wrapped run never showed a pass**: its exit code did not reach the daemon. `tend run` now sends it with the filter request (`exit_code`, optional: older builds leave it out). Also, agents pipe nearly every run (`… 2>&1 | tail -5`), where the exit code is tail's. A run piped only through `tail` or `tee` now passes when the runner's own summary says so and no line says something failed (`learn::summary_run`, `shows_pass`). `| head` and `| grep` still never count. `pytest; echo done` and `npm test || true` no longer count by exit code (it is not the test's).
- [x] **Fixes learned in a wrapped run were never stored**: only the hook path wrote `fixes.jsonl`. The filter path does too now (e2e `a_fix_learned_through_tend_run_reaches_the_machine_store`).
- [x] **Each worktree was its own project**, so fleet workers never got the user's notes, and their fixes ranked as another project's. The project is now the main checkout (the shared `.git`'s parent) for every worktree; paths stay relative to the session's own worktree (`set_learning(project, root, …)`).
- [x] Polish: a failure without a message (`FAILED x::y`) is named by its test in reasons and the report (it read "fixed `` before"). A task that fails after failed checks says so: "…ended without `tend task done`, attempt 3 of 3. Before that, its check `…` failed (exit 1)."

Result (round 8): run 1 learned 2 fixes (parse, fmt) from 4 workers. Fixes are stored under the main checkout. Every session (7 of 7) got the approved note at start. Run 2's worker fixed everything at the first try: no second hit, so no recall, as designed. Run 3's worker hit the fmt failure twice and got: "this failure was fixed before, in this project, minutes ago: after changes to kv/fmt.py, `python3 run_tests.py fmt` passed." It fixed it; asked whether tend helped, it credited the note (run from the repo root), not the recall. Merging: the lead's worktree passed the whole check every round, with no conflicts.

Findings, not built:
- **A recall names files, not the change.** Built since as L6–L7 (owner: the diff stays in its project).
- **Recalls are rare with capable agents.** Haiku fixed every part at the first try; only a forced second hit gave a recall. The second-hit rule keeps noise low, and the cases it serves (a stuck agent) did not come up in this bench.
- An agent's run of two tests in one command (`t; echo; t`) is one hit: one output.

### Learning L, part 2: fixes that help (planned and built 2026-10-22)

Owner: a learned fix keeps a short redacted diff, shown only in its project; recall on the first hit when this project has a fix with a diff for that failure, else on the second.

- [x] **L6 Keep the change.** `EditProposed` text joins a fix once its `FileEdited` lands (a denied edit never does); a snapshotted shell write (`FileChanged`) gives its changed lines; other shell writes name the file only. `learned` entries and `fixes.jsonl` lines gain an optional `change` (additive, format 1): `--- path` then `-`/`+` lines, at most 30 lines and 1,500 chars, `text::redact`ed.
- [x] **L7 Recall it.** First hit for a same-project fix with a change; second hit otherwise. The recall shows the change and says it is another session's, to check before use. Another project's fix never shows its change.
- [x] **L8 Running sessions learn too.** Each hook (and `tend run` filter) checks the store's length and reads only new lines (`learnstore::new_fixes`; a compacted store is read again; known fixes are skipped). A sibling fleet worker gets a fix minutes after it is learned.
- [x] **L9 The user's control.** `tend fixes [--all]` lists fixes with the start of each change; `tend fixes forget [--all]` drops them, refused in a session and denied to the agent in PreToolUse. Writes to the store hold `learn/fixes.lock`.
- [x] Report and `tend why` show the first changed line of a learned fix.
- [x] **Found live: context from a wrapped run never reached the agent.** Recalls and loop steers decided in `tend run`'s filter request were recorded as applied, but the filter reply carries only the text to print. They now go out with the wrapped call's tool result; if the call is stopped first, the ledger records them as not delivered. This affected every steer on a wrapped test run since D4.

Live (round 11, Haiku, `scripts/learn_live.py`): run 1 learned 4 fixes. The lead got a recall with the diff of a fix its own worker had learned minutes before (L8). Runs 2 and 3 got the parse recall with its diff on the first hit. Run 3's worker: "tend was useful: the hook note named the exact one-line change from another session, which I checked against the docstring and test before applying." It finished in 0.2 min (earlier rounds: 0.3–0.4).

- [ ] **F6 Cross-provider.** After the Codex connector (S10, E12).

## Phase 2 — Optimize + Codex (not started)

- [ ] Tune `Config` defaults from Phase 1 data.
- [ ] A/B evaluation harness (reference section 15). Gate: task success does not decrease.
- [ ] Codex connector (`connector/codex.rs`), `tend init codex`, capability downgrades.

## Phase 3 — Protect (not started)

- [x] Guard rules: expected value changed to last actual (E4); tolerance increased (I2).
- [ ] Snapshots as private git refs; `tend undo`.
- [ ] Guard overrides feed strictness.

## Phase 4 — Deciders (not started)

- [ ] Decider contract (versioned JSON); offline evaluation against ledger judgments; one self-hosted adapter.
