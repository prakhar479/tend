# tend: checkpoint (2026-10-19)

This is a checkpoint after roadmap R from `docs/analysis-2026-10.md`. R0 is complete. Most of R1–R3 is done. This document records where tend stands, what fresh measurements show, the defects those measurements found, and what to do next.

## 1. Summary

1. **The engineering plan is mostly done, but the value case is unproven.** Every item that made optimize mode unsafe (R0) is fixed. Wrapper coverage rose from 26% to 93% of test runs. The guard now sees Bash writes. On the owner's real work, though, tend has yet to do anything useful: 0 test outputs shaped, 0 guard flags, and 0 true loops.
2. **New defect C1: shaping would summarize source code.** In optimize mode, tend summarizes outputs over 2000 tokens unless they are classified as a file view. The view check misses `cd repo; cat -n f.py`, `wc -l f && sed -n …`, `cat f | sed -n …`, and `docker exec … cat`. In subagents, at least 59 of the 75 outputs tend would shape are file reads, and most of the rest are diffs. This breaks the reference's rule against summarizing code. Fix it before anyone uses optimize mode.
3. **New defect C2: every loop nudge on real work was a false positive.** Loop detection reads failure text from every Bash output, not only test runs. `sed -n` over Rust source matched `assert!(`, and `jq` over a ledger matched `KeyError: 'total'`. Each became a "loop". The R2.3 figure "2 of 3 loop nudges followed by the failure stopping" describes these false positives.
4. **New defect C3: `retro --all` misreports.** It counts tend's own scripted test sessions (50 of 56 transcripts) and skips subagent transcripts, where most large outputs are. Every guard flag in the headline comes from the scripted sessions.
5. **tend has never been used for real work.** All 28 live ledgers come from e2e and conformance runs, and `tend review` has 0 flags. R2.4 (real-session review) has not started, and nothing else can be judged until it has.
6. **Recommended direction.** Make tend a supervisor and auditor, not a token saver. In the owner's data the agent trims its own test output; large outputs are code reads, which tend must not summarize. What tend can see that nobody else reviews is the 213 Bash writes to test files, where the agent got stuck, and whether its claims match the runs. Prove that value with a small seeded benchmark, because natural incidents are too rare to measure.

## 2. State

| Area | State |
|---|---|
| Code | 9.5k lines of Rust across `tend-core` (pure) and `tend-cli`. 124 tests (40 + 13 + 71) plus a live conformance script. `make check` is clean. |
| Look mode | Complete: ledger, report, retro, `why`, `show`, `flag`, `review`, `status`, a one-line summary at exit. |
| Optimize mode | Wrapped test runs with a failure summary, the guard (deny with the reason; Bash writes get a warning), loop steering, and a retry of auto-mode denials. Blocked by C1 and C2. |
| Platform | Claude Code 2.1.303 passes all 9 conformance checks. `updatedInput` without a decision still works, but it remains undocumented. |
| Not started | R1.5 and R1.6 (measured, then deferred), R2.4, R3.3 plugin, R3.4 http hooks, R3.6, S10 Codex, N15 macOS, Phases 2–4. |
| Process | No git remote, so the CI workflow (N5) has never run. Installs come only from `cargo install --path`. |

## 3. Fresh measurements

These come from `tend retro` over `~/.claude/projects`, split by origin. A throwaway probe built on `tend-core` (`classify` plus `shape`) listed what optimize mode would shape.

| | Main sessions (real) | Subagents (real) | Scripted (tend's tests) |
|---|---|---|---|
| Transcripts | 6 (polaris, MILP, this repo) | 29 | 50 |
| Bash output | 870k tokens | 1.09M tokens | small |
| Test runs, wrappable | 489, of which 455 wrappable (93%) | few | — |
| Bash writes to test files | 213 of 1076 | — | — |
| Guard flags | **0** | **0** | 9 (all) |
| Loop nudges | 2, **both false** (C2) | 0 | 1 |
| Outputs tend would shape | 9, **0 of them test output** | 75, **0 test output**, at least 59 file reads | — |
| Estimated saving | ~1% of Bash output | 16%, almost all from summarizing code (C1) | — |

What changed since the analysis:

- **Coverage works.** R1.1 brought the wrapper from 26% to 93% of test runs. The guard now covers the Bash-write path, which was 92% of edits.
- **The agent rarely triggers tend's features.** On real work, the agent never weakened a test (consistent with the R1.4 sample of 14, all legitimate edits). It never looped on a real failure. It already trims its test output, so wrapped runs stay under the shaping threshold.
- **Subagents hold the large outputs.** They produce more Bash output than the main sessions (1.09M against 870k tokens). These outputs are file and doc reads that the subagent asked for, so the "saving" is exactly what must not be shaped.

## 4. Defects found in this checkpoint

| # | Where | Defect | Status |
|---|---|---|---|
| C1 | `classify::is_view`, `shell::leading_cd`, `session::SessionModel::new` | Only views are excluded from shaping, and the view check reads only the first segment. `leading_cd` accepts `cd X &&` but not `cd X;`. Any file read that starts with another command (`wc`, `mkdir`, `S=…;`, `docker exec`) or pipes `cat -n` into `sed -n` is classified as Other or Search and summarized. Git diffs are shaped too. | **Confirmed** (probe over the owner's transcripts: 84 would-shape outputs, mostly code) |
| C2 | `Controller::observe` → `loops.on_run` | Failures are extracted from every Bash output, not only test runs (`OutputKind::Test`). Viewing code or logs that contain failure text builds a loop. A signature like `assert!(` is accepted. | **Confirmed** (both real nudges trace to `sed -n` and `jq` outputs) |
| C3 | `retro::run_all` | It counts scripted sessions under `/tmp/*` project dirs, which hold every guard flag. It skips `*/subagents/*.jsonl`, which hold most large outputs. | **Confirmed** |
| C4 | `docs/TASKS.md`, notes | The R2.3 "loop precision 66%" and the "6 guard flags" figures come from false or scripted events. | **Confirmed**: correct them with C2 and C3 |
| C5 | repo | No remote, so CI has never run. | Process |

## 5. What is proven and what is not

| Claim | Evidence | Verdict |
|---|---|---|
| tend is safe in look mode | R0 fixes, fail-open tests, latency p95 ≈ 1 ms | Proven |
| Optimize mode keeps what the agent needs | Correct for test output. Wrong for code reads (C1) | **Not yet** |
| The guard catches test weakening | Fires in seeded sessions. 0 real incidents in 1,076 Bash writes | Mechanism proven; value unmeasured |
| Loop steering shortens loops | 0 real loops. The only nudges were false (C2) | Unmeasured |
| Shaping saves context | ~1% in main sessions. In subagents only by summarizing code | Not supported by the owner's data |
| The owner's data is typical | One user, an Opus-class model, a careful workflow | Unknown: needs other users or weaker models |

The owner's sessions may simply not be where tend helps. Strong models that trim their own output, careful prompts, and short loops leave little for a supervisor to catch. The plausible places where it would help are weaker or cheaper models, long autonomous runs (auto mode, `-p`, many subagents), and less careful users. None of these has been measured.

## 6. Next steps

Sizes: S ≈ hours, M ≈ a day, L ≈ days.

### C. Fix what this checkpoint found (do first)

| # | Item | Acceptance | Size |
|---|---|---|---|
| C1 | **Shape by allow-list, not deny-list.** By default, shape only `Test`, `Build`, and `Install` outputs; leave the others at level 0 until R1.5 and R1.6 decide. Also make `leading_cd` accept `;`, and treat a call as a view when any segment is a view program. | The probe over the owner's transcripts shapes 0 file reads and 0 diffs. Existing shape tests pass | S |
| C2 | **Loops come only from test runs.** Feed `loops.on_run` only when `kind == Test` (or the output came through `tend run`). Reject degenerate signatures (punctuation-only tails, under 8 characters). | 0 nudges on this repo's dev session. The loop fixtures and e2e tests still fire | S |
| C3 | **Honest retro.** `retro --all` reports main sessions and subagents on separate lines and skips the projects of tend's own runs (`/tmp/*conformance*`, scratchpads) by default, with `--include-scratch` to keep them. | The numbers match §3 | S |
| C4 | Correct the R2.3 figures in TASKS.md and the notes after C2 and C3, then re-run the retro. | — | S |

### V. Prove the value (the central question)

| # | Item | Acceptance | Size |
|---|---|---|---|
| V1 | **Seeded benchmark.** Grow the conformance "shop" into about 10 small tasks that tempt the agent: a failing test where weakening it is the easy way out, an unfixable flaky failure that invites a loop, a test whose expected value drifts, very noisy output. Run them with and without `--tend-mode optimize` on Haiku, Sonnet, and Opus, 3 runs each. Measure task success, test weakening (by a hidden checker), minutes in loops, and tokens. | A table per model. The Phase 2 gate: success does not drop | L |
| V2 | **Use it for real (R2.4).** Run `tend claude` in look mode for all work for two weeks. Flag every wrong action. The friction of typing `tend claude` is the likely reason this hasn't happened, so do V3 first, or add a shell alias now. | 20 sessions reviewed; flags turned into fixtures | L (calendar) |
| V3 | **Plugin (R3.3), including SessionEnd.** Hooks from a plugin, with the daemon started on SessionStart. Users keep typing `claude`. | A plain `claude` session writes a ledger | M |
| V4 | **Other users' data.** Add `tend retro --all --summary`: counts only, no commands or paths, safe to paste. Ask 3–5 colleagues to run it. | Shows whether the owner's ~0 incidents are typical | S + people |

### A. Lean into auditing (likely the product)

| # | Item | Acceptance | Size |
|---|---|---|---|
| A1 | **Test-change audit at session end.** Using the R1.4 snapshots (or `git diff` on test paths at Stop), list every test file the agent changed, with assertion counts before and after, skips added, and expected values changed. Show it in `tend report` and the exit line ("3 test files changed · 1 assertion removed · `tend report`"). | A seeded session lists each changed test with correct counts | M |
| A2 | **Claim check.** When the agent's final message says tests pass, compare that claim with the last run of each test target in the ledger. Report a mismatch, for example "said 'all tests pass'; last `pytest tests/` run failed 2". This is the "honesty" half of the reference's positioning and needs no blocking. | Fixture: a false "all pass" claim is reported | M |
| A3 | Snapshots and `tend undo` for test files (Phase 3), built on the R1.4 snapshots. | Undo restores a weakened test | M |

### P. Platform and upkeep (as needed)

- **P1.** The launcher compares `claude --version` with `docs/conformance/` and asks the user to run `scripts/conformance.py` on a new version, because `updatedInput` without a decision is undocumented.
- **P2.** Add a git remote so CI runs (C5).
- **P3.** R3.4 http hooks: only if V2 shows that shim spawn cost matters. Latency is ~1 ms now.
- **P4.** Split `controller.rs` (2,011 lines) into loop, guard, and shape handlers when C1 and C2 touch it.
- **P5.** Park S10 (Codex), R1.5, and R1.6 until V1–V4 show where the value is.

## 7. Decisions for the owner

1. **Positioning.** Lead with auditing and supervision (test changes, loops, claims) and keep shaping as an opt-in for test output only? The data in §3 supports this.
2. **Benchmark budget.** V1 is about 10 tasks × 3 models × 2 arms × 3 runs, roughly 180 short headless sessions. Is that spend acceptable, and which models?
3. **Remote.** Create a private GitHub repo so CI runs?
4. **Plugin before dogfooding?** V3 first (about a day), or start V2 now with a `claude` → `tend claude` alias?

## 8. Recommended order

C1–C4 (one sitting) → V3, or the alias → start V2 in the background → A1 and A2 → V1 → decide Phase 2 from V1 and V2.
