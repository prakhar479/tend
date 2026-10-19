# rein: analysis and roadmap (2026-10-19)

Status after Phase 0, Phase 1, and next steps N1–N14. This document is the basis for the roadmap in `docs/TASKS.md` ("Roadmap R").

## 1. Summary

1. **Optimize mode has safety bugs. Fix them before anyone uses it.** The `rein run` wrapper hangs when a test starts a background process. When the wrapper is killed, the agent loses all output and the test process keeps running. The ledger can claim a summary that the agent never saw. Look mode (the default) is not affected.
2. **rein's value is not where we assumed.** In the owner's sessions, test output is 8.4% of Bash output. The agent already trims 99% of test runs with `| tail`, `-q`, or `| grep`. File views (`sed -n`, `cat`, `head`) are 51% of Bash output, and 70% in subagents. Search output is 18%.
3. **The guard sees under a fifth of edits.** Agents edit mostly through Bash (`sed -i`, heredocs, Python `open(...,'w')`), not through the Edit and Write tools. About 150 of those Bash writes targeted test files.
4. **Exit codes are hidden.** Only 0.9% of test runs are marked as errors, yet 34.7% show failure text: `cmd | tail` returns the exit code of `tail`. Failure detection must read the output. Loop detection must key on a normalized test target, because exact command repeats never happen.
5. **The wrapper misses most test runs.** It refuses 74% of them, mostly because of a leading `source .venv/bin/activate &&`, which is not a real state change for later calls.
6. **Tokens matter as context, not money.** 98.9% of prompt tokens are cache reads. The case for rein is context-window pressure and attention, plus loops and honesty, rather than spend. That matches the reference's "lead with loops and honesty".
7. **The name `rein` collides.** `pegasi-ai/reins` (392★) is an agent guard tool with the tagline "Stop AI agents from doing things you didn't ask for", and the reference's landscape table already lists "Reins". This is a decision for the owner (§7).

## 2. Evidence

| Source | What | Limits |
|---|---|---|
| Code review | Full read of rein-core and rein-cli, with experiments against a release daemon (scripts in the session scratchpad) | One reviewer pass |
| Transcript study | 6 main sessions plus 29 subagent transcripts in `~/.claude/projects`, 2026-10-15 to 2026-10-19, 2358 turns, 2357 tool calls | One user. One session holds 62% of turns. Read as a signal, not a law |
| Docs check | Claude Code hooks, permissions, plugins, statusline, tools and env-var references | Partly summarized fetches. Re-check exact wording before relying on it |
| Benchmarks | Release shim against a real daemon, 500 calls each | One machine (WSL2) |

## 3. What the data says

### 3.1 Where Bash output goes (main sessions)

| Class | Calls | Share of Bash tokens | Median chars | p90 | Over 8k chars |
|---|---|---|---|---|---|
| File views (`sed -n`, `cat`, `head`) | 581 | **50.8%** | 1848 | 5934 | 32 |
| Search (`grep`, `ls`, `find`) | 356 | 17.7% | 1105 | 3130 | 5 |
| Other | 299 | 9.5% | 506 | 2799 | 1 |
| Scripts (Python heredocs) | 327 | 9.0% | 423 | 1904 | 1 |
| **Tests** | 444 | **8.4%** | 401 | 1216 | **0** |
| Git | 79 | 2.3% | 422 | 2193 | 0 |
| Builds | 94 | 2.2% | 435 | 1366 | 1 |

- Bash is 93.7% of all tool-result tokens and 91.7% of the re-read weight. Read is 2.6%.
- 1.8% of Bash calls exceed rein's shaping threshold (2000 tokens), and they carry 16.6% of Bash tokens. In subagents it is 15.7% of calls and 55% of tokens.
- 67.6% of Bash calls already limit their own output (`| head` 42%, `2>&1 |` 28%, `| tail` 23%, `| grep` 23%, `-q` 22%).

### 3.2 Test runs

- Runners: pytest 356, cargo test 46, make check 25.
- 99% use a pipe and 91% are chained with `&&` or `;`.
- 54% start with `source <venv> &&` (55% of test-output tokens). 7% set environment variables. 4% contain `cd`.
- The wrapper's state-change rule refuses 74% of test runs (73% of their tokens).

### 3.3 Repetition, loops, and edits

- Exact same command within the next 3 Bash calls: **0**. With commands normalized (pipes, `source`, and flags removed): 93 repeats, 19 with no edit in between. 41 test targets were run 3 or more times, and one 32 times.
- Loop candidates (the same failing target 3 or more times, with edits in between): 9, with chain lengths 3–4, and one of 9 or more.
- Edit-tool edits: 89 (Write 64, Edit 25). Bash writes: 202 heredocs to files, 141 `sed -i`, and up to about 552 Python writes. About 150 Bash writes went to test paths. No Edit-tool change to a test removed an assertion.

### 3.4 Time and tokens

- Median gap between tool calls: 9.2 s (p90 56 s). 45 gaps were over 10 min, about 135 h idle in total, so the active loop clock (N9) was needed.
- Prompt tokens: 858M, of which 849M were cache reads (98.9%). Output tokens: 2.29M.

### 3.5 Latency (release build)

| Hook | p50 | p95 |
|---|---|---|
| PreToolUse, Bash (now on every Bash call) | 0.8 ms | 1.0 ms |
| PostToolUse, 25 KB payload | 1.1 ms | 1.5 ms |

Target: p95 under 20 ms. It holds, unless the daemon is busy (see D4).

## 4. Defects

Ranked by harm. **Confirmed** means reproduced by running it. **Plausible** means certain or likely from reading the code.

| # | Where | Defect | Status |
|---|---|---|---|
| D1 | `run.rs` `execute` | `read_to_end` waits for the pipe to close, not for the shell to exit. A test that starts a server, a docker container, or anything with a trailing `&` hangs the wrapper until the Bash timeout. The agent gets nothing. | **Confirmed** (4 s wait for a 4 s background sleep) |
| D2 | `run.rs` | Killing `rein run` (timeout, Esc) prints nothing and orphans the test process. Without the wrapper, the agent would see partial output. The user also sees no live output during long runs. | **Confirmed** (no output; `sleep` kept running) |
| D3 | `run.rs` + `Controller::filter` | If the filter reply misses its 2 s timeout, `rein run` prints the original output, but the ledger already says `shape … applied`. `last_summary` is also set, so the next run counts as a false re-open. This breaks rule 4. | **Confirmed** (57 MB output, 300 ms timeout) |
| D4 | `daemon.rs` | Ping and every hook share one mutex. A large filter holds it for longer than the shim's 250 ms and `is_live`'s 200 ms. Parallel hooks then fail open and are lost, including a guard deny. A second `rein claude` can unlink a busy session's socket as "stale", and `prune` can misjudge it as dead. | **Confirmed** (354 ms ping during a 37 MB filter) |
| D5 | `run.rs` | `bash -c` loses the shell snapshot's functions and aliases (`mytest: command not found`). The bare `rein` in the rewrite depends on the snapshot's `PATH`. | **Confirmed** for functions; PATH plausible |
| D6 | controller + daemon | The agent can lift a guard without `rein allow`. The socket path is in its environment, so a Python one-liner can send `allow`, `set_mode`, or `shutdown`, and quoting tricks bypass the regex. A false positive also exists: `git commit -m "… rein allow …"` is denied. | Plausible (from reading) |
| D7 | `classify.rs` | Runner detection matches anywhere in the command, so `git commit -m "fix pytest"` gets wrapped. The state-change regex misses newline-separated `cd`, `export`, and `{ cd …; }`. | Plausible |
| D8 | `store.rs` | `writeln!` makes two `write` calls, so a crash can tear a ledger line. Blobs are written in place and never repaired. Shutdown exits without taking the lock. | Plausible |
| D9 | `daemon.rs` | When the shim times out, the controller has already recorded guard, wrap, or shape as `applied`. | Plausible |
| D10 | `paths.rs` | `ensure_private_dir` follows symlinks. A pre-made `/tmp/rein-<uid>` symlink makes rein chmod its target. | Plausible |
| D11 | controller | `wrapped`, `blocked`, `allowed`, and `outputs` never shrink. Wraps leak when no PostToolUse arrives. | Plausible, low |
| D12 | `shim.rs` | `println!` panics on EPIPE and exits 101, which breaks rule 1. | Plausible, low |

## 5. Platform facts (docs check)

- **Documented now:** permissions are checked on the `updatedInput` command. **Not documented:** `updatedInput` without a `permissionDecision`, which rein relies on. The documented examples pair it with `allow` or `ask`. This is a fragility risk; add a conformance test per Claude Code version.
- Hook events rein does not use yet: `SessionEnd` (clean shutdown and final report), `PostToolBatch`, `PermissionRequest`, `PermissionDenied`, `Notification`, `SubagentStart`, `SubagentStop`, `CwdChanged`, `FileChanged`.
- `systemMessage` is documented as shown to the user on any platform, but it did not appear on a PreToolUse `ask`. Report this as a Claude Code bug with `/feedback`.
- `updatedToolOutput` works for all built-in tools when it matches the tool's output schema. Read and Grep shapes are not documented; capture them before building on them.
- Hook type `http` exists, with the same JSON, fail-open by default, and `onFailure: block` available. It could replace the shim process. A failure shows a notice to the user, so measure it first.
- Plugins can ship hooks, merged with user and project hooks, with `CLAUDE_PLUGIN_DATA` for state and `session_id` in every payload. A plugin could replace `rein claude --settings`, so users keep typing `claude`.
- Output limits: Bash keeps about 30,000 chars inline, then persists output with a 2,000-char preview. A failed Bash result keeps about 10,000 chars as a head-and-tail excerpt (matches the S1 capture). `BASH_MAX_OUTPUT_LENGTH` and `bashOutputMaxChars` change these limits. Hook strings are capped at 10,000 chars.
- Bash calls run in separate processes with a shell snapshot. The working directory persists, and environment variables do not, which is why `source venv && …` is safe to wrap.
- The statusline gets session JSON, not hook data. A rein statusline would read the daemon's state through its socket or a file.

## 6. Roadmap

Each item has an acceptance check. Sizes: S ≈ hours, M ≈ a day, L ≈ days.

### R0. Make optimize mode safe (before using it)

| # | Item | Acceptance | Size |
|---|---|---|---|
| R0.1 | `rein run`: wait for the shell to exit, then drain the pipe for at most 100 ms. Run the child in its own process group. On SIGTERM, SIGINT, or SIGHUP, kill the group and print what was captured. | Tests: a background child does not delay return; a kill prints partial output and leaves no orphan | M |
| R0.2 | Record what the agent saw: `rein run` sends an ack after printing, or PostToolUse compares hashes and records `applied: false` with a reason. Cap filter input with head and tail. | Test: a filter timeout gives `applied: false`, and the next run is not a false re-open | M |
| R0.3 | Daemon: answer Ping without the lock. `is_live` treats "connected but slow" as live. Shape outside the lock, or cap its input. | Test: ping during a 37 MB filter answers in under 20 ms; a busy session's socket survives a second launch | M |
| R0.4 | Wrapper: use the absolute path of the rein binary. Don't wrap a command whose first word is a shell function, alias, or not on PATH. | Test with a function-defined runner | S |
| R0.5 | Classifier: detect runners only in command position. Refuse to wrap multi-line commands and `{ … }` groups. | Unit tests for `git commit -m "pytest"` and newline `cd` | S |
| R0.6 | Ledger: build the whole line and write it once; write blobs to a temp file and rename; take the lock before exiting on Shutdown. Shim: ignore EPIPE and always exit 0. | Kill test during writes: no torn lines | S |
| R0.7 | Housekeeping: `ensure_private_dir` rejects symlinks; drop `wrapped` entries at Stop; cap the controller maps. | Unit tests | S |

### R1. Close the coverage gaps the data shows

| # | Item | Acceptance | Size |
|---|---|---|---|
| R1.1 | **Wrap behind `source …/activate &&` and environment-variable assignments.** These don't change state for later calls. | Coverage on the owner's transcripts: from 26% to over 80% of test runs | S |
| R1.2 | **Detect failures from the output, not `is_error`.** Use `extract_failures`, plus runner summary lines (`N failed`, `FAILED`, `test result: FAILED`). Store `failed_by_text`. | On the owner's transcripts, failures found ≈ the 34.7% with failure text | S |
| R1.3 | **Normalize command keys** for loops and re-runs: drop pipes, `-q`/`-v`, `2>&1`, and a leading `source`/`cd`. | The re-run signal fires on the 19 no-edit repeats; loop candidates match §3.3 | M |
| R1.4 | **Guard Bash writes to test files.** After each Bash call, diff the test files the command touched (from its text and the paths in `sed -i`, `>`, `open(...,'w')`) against a snapshot taken at PreToolUse. In look mode, report. In optimize mode, warn through `additionalContext` (the write already happened). | A `sed -i` that removes an assert in `tests/` is flagged | M |
| R1.5 | **Faithful file views.** For `sed -n`, `cat`, and `head` over 8k chars, collapse only exact repeats (T3) and long runs of blank or generated lines. Never summarize code. Measure first: how often does the agent re-read the same range? | Retro shows savings with zero changed code lines | M |
| R1.6 | **Search output.** For `grep -r`/`rg` over 8k chars, group by file with a per-file count and the first N hits, keeping `rein show`. | Re-open rate below 10% in A/B | M |

### R2. Learn from real use (Phase 1 exit)

| # | Item | Acceptance | Size |
|---|---|---|---|
| R2.1 | `rein flag <id> "note"` and `rein review`: mark a wrong action or missed failure; list flags across sessions. | Flags turn into fixtures with `scripts/capture_fixtures.py` | S |
| R2.2 | `retro --all` prints coverage: the share of test runs wrapped, the share of edits the guard saw, and output share by class (§3.1). | Matches the numbers in this document | S |
| R2.3 | Guard and loop precision: the share of guard flags that were allowed, and of loop nudges that were followed by a different approach. | Shown in `rein report` | S |
| R2.4 | Review 20 real sessions in look mode (N16). | Owner agrees with the reports, or the flags are fixed | L |

### R3. Platform and distribution

| # | Item | Acceptance | Size |
|---|---|---|---|
| R3.1 | Conformance tests per Claude Code version: replay captured payloads, check `updatedInput` without a decision, `updatedToolOutput` shapes, and persisted-output text. | A CI job flags drift | M |
| R3.2 | A `SessionEnd` hook: stop the daemon and print a one-line report. | No orphan daemons after `/exit` | S |
| R3.3 | Spike: ship rein as a Claude Code plugin (hooks plus a SessionStart that starts the daemon, located by `session_id`). | `claude` works with rein without the launcher | M |
| R3.4 | Spike: `http` hooks instead of the shim process. Measure latency and the notices shown on failure. | Decision recorded | S |
| R3.5 | Statusline: `rein status` prints mode, savings, and the last action for the statusline. | Shown live in Claude Code | S |
| R3.6 | Report the `systemMessage`-on-ask behavior to Claude Code with `/feedback`. | Filed | S |

### R4. Phase 2–4, in the reference's order, once R0–R2 are done

- A/B harness (30–50 fixed tasks, with and without optimize). The gate: task success does not drop.
- Tune the defaults from R2 data. The data suggests the 2000-token threshold rarely fires in the owner's sessions; check it against other users before changing it.
- Codex connector (needs the Codex CLI; S10).
- Guard rules from the reference (an expected value changed to the last actual one, raised tolerances), snapshots and `rein undo`, overrides feeding strictness.
- Deciders after offline evaluation.
- Interoperate with RTK filters (reference §6) instead of writing a filter per command: R1.5 and R1.6 overlap RTK's ground.

### Dropped or lowered

- *Shaping Read results* (previous P3): 2.6% of tokens in the main sessions. Revisit only if R2.2 shows otherwise in other users' data.
- *Re-read-weighted savings in the report* (previous F2): 98.9% of re-reads are cache reads, so the cost signal is weak. Report the context share instead.
- *Default permission mode wrapping* (previous H5): lower than R1.1, because most users of `-p` and auto mode get coverage from R1.1 first.

## 7. Decisions for the owner

1. **Name.** `pegasi-ai/reins` is an active agent-guard project with a near-identical name, and the reference lists "Reins" as a competitor. Keep `rein`, or rename again while it is cheap (one commit)?
2. **Positioning.** The data says the agent already trims its test output, and loops are rare but real. Lead with loops, honesty, and the guard (the reference's decision log), and treat token savings as secondary?
3. **The guard on Bash writes (R1.4).** In optimize mode the write has already happened when rein sees it. Is a warning in context enough, or should rein snapshot test files and offer `rein undo` (moving part of Phase 3 forward)?
4. **Distribution (R3.3).** Plugin instead of launcher, if the spike works?

## 8. Recommended order

R0 (all) → R1.1, R1.2, R1.3 → R2.1, R2.2 → two weeks of real use in look mode → R1.4–R1.6 by measured value → R3.1, R3.2 → R4.
