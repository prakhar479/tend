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
- [ ] **F2 Shared learning.** Signature memory (`<home>/fleet/signatures.jsonl`) delivered on a second hit or a loop start; `tend note` with a review queue and `tend notes --promote`. Gate: one real hit where the delivered fix was right.
- [ ] **F-S1 Spike: stream mode.** Long-lived `claude -p --input-format stream-json --output-format stream-json` under tend's settings: do hooks fire, can a turn be sent when idle, how does interrupt work, does `--resume --fork-session` work headless?
- [ ] **F-S2 Spike: concurrency.** 4 stream sessions at once: rate and usage limits, hub CPU.
- [ ] **F3a Spawn and lifecycle.** Hub spawns `stream`/`oneshot` sessions (`--session-id`, role brief, worktree); restart with backoff; stuck detection; `fleet status | stop | down | prune`. Acceptance: e2e with 2 fake stream workers, one killed and restarted, `down` stops all, no worktree lost.
- [ ] **F3b Core fleet model.** `tend-core/src/fleet/`: fleet file, roles, channels, presets, pure `step(state, event, now_ms) -> (state, commands)`. Acceptance: table tests per preset; replay is deterministic.
- [ ] **F4 Messaging.** `tend msg`, `tend inbox`, `--tend-name`, `--tend-role`; delivery through `inject_context` at the next hook, digested and rate-limited; messages are untrusted and can never lift a block. Optimize mode only.
- [ ] **F5a Task board and verbs.** `tend task add|assign|claim|done|blocked`; role checks in PreToolUse. Acceptance: lead adds 3 tasks, 2 workers finish them, dependencies respected.
- [ ] **F5b Check gate.** `done` means tend ran the check and it passed; failures go back as a new attempt; guard and audit flags escalate.
- [ ] **F5c Workspaces, claims, and scopes.** Worker slots own a worktree (default); team worktrees with enforced claims, a team channel, serialized checks; `may_write`/`may_not_write`; cross-worktree conflict prediction. Acceptance: in a team worktree a write into another worker's claim is denied; in `specialists` an implementer's test edit is denied.
- [ ] **F5d Budgets and limits.** Tokens per session, role, and fleet; usage-limit pause; spawn depth.
- [ ] **F5e Reports and merge plan.** `fleet report`, `fleet merge` (plan; merge on yes), `fleet replay`.
- [ ] **F5f More presets.** `review`, `best-of`, `pipeline`, `supervisor`, `pool`; one seeded bench run each.
- [ ] **F5g Hierarchy (first-class).** Sub-leads, task trees, budget slices that only narrow, escalation upward, roll-up reports, sub-lead restart. Acceptance: lead → 2 sub-leads → 2 workers each; a child cannot exceed its slice; a killed sub-lead resumes its subtree.
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
