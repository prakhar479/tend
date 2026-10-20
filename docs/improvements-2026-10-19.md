# tend: improvements found in review (2026-10-19)

**Status (2026-10-20):** every item below is done (batches I1–I6 in `docs/TASKS.md`), except what needs a remote (C5: CI has not run yet) and a real PermissionDenied capture.

This review came after the checkpoint fixes (C1–C4). It had four parts: tend-core logic, the tend-cli runtime, test quality (87 hand-made mutations), and a hands-on CLI pass. Most findings are **confirmed**: each was reproduced by a test, a run, or a measurement, in throwaway scratch projects; the repo was not changed. Findings marked *plausible* come from reading the code. Sizes: S ≈ hours, M ≈ a day, L ≈ days.

Rule numbers refer to the rules in `CLAUDE.md`. Rule 1: tend never blocks or breaks the agent through its own failure. Rule 4: the ledger never claims an action that did not happen.

## 1. Apply first

These are the ten best returns for the effort. Most of them are about tend breaking its own rules.

| # | Item | Why first | Size |
|---|---|---|---|
| F1 | A malformed `tend hook …` call exits 2, and exit 2 blocks the tool call | Breaks rule 1. A plugin or a version skew would block every Bash call | S |
| F2 | The launcher fails closed: when tend's own setup fails, the agent never starts | Breaks rule 1 | S |
| F3 | SIGTERM or SIGHUP to `tend claude` leaves claude running with no supervision | Silent loss of supervision | S |
| F4 | Write and Edit read and blob whole files of any size, under the daemon lock | Lost guard denies, disk use, RAM | S |
| G1 | The guard counts asserts inside comments and strings. It misses commented-out asserts and many skip forms | The guard's central promise | S–M |
| G2 | Rust inline tests, Django `tests.py`, and .NET `*Tests.cs` are never guarded | Includes every one of tend's own tests | S |
| L1 | Loop nudge on a passing run that prints an `XxxError:` line | False steer on a green suite | S |
| L2 | `pytest | tee log` counts as a fix attempt | False loops with zero edits | S |
| S1 | The build and install regexes match anywhere (`grep -rn make src/` counts as Build, so it is shaped) | Re-opens C1 through a side door | S |
| T1 | Guard rules and fail-open paths have almost no tests: all 12 guard-rule mutations survived | Locks in the fixes above | M |

## 2. Fail-open and safety (rule 1)

| # | Where | Finding | Evidence | Fix | Size |
|---|---|---|---|---|---|
| F1 | `main.rs:159` | `Cli::parse()` runs before the hook path. `tend hook`, `tend hook PreToolUse extra`, and `tend hook --x` exit **2**, which means "block" for PreToolUse. A parse error in `tend run` also exits 2, so the test never runs. | Confirmed | Handle `argv[1] == "hook"` before clap and always exit 0. For `run`, fall back to running `--sh` as given | S |
| F2 | `launcher.rs:26-33` | `?` on `ensure_private_dir`, the token, and the settings write. With a symlinked `TEND_HOME`, `tend claude` exits 1 and claude never starts. | Confirmed | On any setup error, print one line and start claude without `--settings` | S |
| F3 | `launcher.rs:50-73` | Only SIGINT is handled. `kill -TERM <launcher>` orphans claude. If claude dies by a signal, the exit code is 1 instead of 128+sig. | Confirmed | Forward TERM, HUP, and QUIT to the child, then shut the daemon down and return 128+sig | S |
| F4 | `claude_code.rs:256,352`, `controller.rs:894` | A Write reads the old file with no cap, under the lock, for any path. Old and new texts become blobs. A 200 MB Write took the daemon to 396 MB RSS, and a concurrent guard deny was lost (the shim timed out). | Confirmed | Read and blob only test paths, capped at `MAX_SNAPSHOT_BYTES`. Elsewhere store hashes only, with a streamed hash | S |
| F5 | `run.rs:103-119` | `tend run` buffers all output in RAM: 500 MB of output used 493 MB RSS. Nothing appears until the command exits. | Confirmed | Buffer up to 8 MiB, then flush and stream the rest (no filter runs at that size anyway) | S–M |
| F6 | `run.rs:83-97` | The child runs in its own process group, and PDEATHSIG is set only on bash. After `kill -9` on `tend run`, the test's children survive outside the caller's group kill. macOS has no PDEATHSIG at all. | Confirmed (Linux) | Use `PR_SET_CHILD_SUBREAPER` and kill all descendants, or keep the caller's group. On macOS, a kqueue watchdog | M |
| F7 | `launcher.rs:65-96` | The daemon is never waited on. A dead daemon becomes a zombie, later hooks silently record nothing, and the exit line looks normal. | Confirmed | Keep the `Child`. At exit, `try_wait` and print "tend stopped at …; later calls were not recorded" | S |
| F8 | `controller.rs:446`, `claude_code.rs:419` | If Claude Code ignores `updatedInput` (undocumented), the wrap stays `applied: true`. Shape has the same blind spot. | Plausible | At PostToolUse, check that `tool_input.command` contains `run --id <id>`. If not, record not-applied, stop wrapping, and say so at exit. Also do P1: the launcher checks the claude version against `docs/conformance/` | S–M |
| F9 | `daemon.rs:101` | No read timeout or size limit per connection: 500 idle connects gave 501 threads. Every `is_live` probe logs `bad request: EOF`. | Confirmed | 2 s read timeout and a request-size cap. An empty first read is a silent probe | S |
| F10 | `claude_code.rs:470`, `launcher.rs:125` | The hook command is `"\"{exe}\" hook …"`, so a path with `"`, `$`, or a backtick breaks it. The exe dir goes first on PATH and shadows system tools for the agent. | Plausible | Use `run::sh_quote`. Put a private `<session>/bin/tend` symlink on PATH | S |

## 3. Guard accuracy

| # | Finding | Evidence | Fix | Size |
|---|---|---|---|---|
| G1 | `assert_re` counts text in comments and strings. **Missed:** `assert x` → `# assert x`, `// expect(…)`, `// assert_eq!`. **False positives:** removing `from pandas.testing import assert_frame_equal`, or rewording a comment that contains "assertion". | Confirmed | Strip comments and string literals before counting. Count call forms only: `assert\s`, `assert\w*\s*[(!]`, `expect(` | S |
| G2 | `is_test_path` is false for `src/orders.rs` (Rust inline tests), `tests.py`, `*Tests.cs`, and `_spec.rb`. | Confirmed | Also judge files whose old or new text has test markers (`#[test]`, `#[cfg(test)]`, `def test_`, `func Test`), and add those path patterns | S |
| G3 | Skip forms missed: `pytestmark = pytest.mark.skip`, `@mark.skip`, `@skip`, `expectedFailure`, `self.skipTest(`, `raise SkipTest`, `marks=pytest.mark.xfail`, `#[ignore = "…"]`, an added `#[should_panic]`, `it.skip.each`, `it.skipIf`, `test.fails`. An `.only` → `.skip` swap nets to zero. | Confirmed | Widen the skip regex. Count skips added, not the net change | S |
| G4 | Structural weakening missed: deleting or renaming a test (`def test_a` → `def _test_a`), `return` before the asserts, `try: assert … except AssertionError: pass`. | Confirmed | Add a "test definitions decreased" rule, plus rules for an added `except AssertionError` and an early `return` | M |
| G5 | Always-pass variants missed: `assert True, "msg"`, `self.assertTrue(True)`, `assert 1 == 1`, `expect(1).toBe(1)`, `assert_eq!(1, 1)`, `assert!(true, "x")`. | Confirmed | Allow a trailing message. Add a same-operand check (no back-references in the regex crate, so compare in code) | S |
| G6 | `controller.rs:768`, `:936`: no test covers a warning that stays out of `blocked`, or Deny becoming Warn without the deny capability. | Mutations survived | Controller tests | S |

Also consider the reference's own Phase 3 rules: an expected value changed to the last actual value, and a raised tolerance. They fit here once G1–G5 are done.

## 4. Loops and failure signatures

| # | Finding | Evidence | Fix | Size |
|---|---|---|---|---|
| L1 | The generic fallback treats any `XxxError:` line as a failure, even when the run passed (`pytest -s` printing `ConnectionError … 3 passed`). This gives a loop steer on green tests. | Confirmed | Skip the fallback when a passing summary is present and no failing one is | S |
| L2 | `writes_files` counts `tee` and `> file`, so `pytest | tee /tmp/t.log` ×4 is a loop with zero edits. | Confirmed | Ignore writes in the runner's own pipeline and redirects of its output | S |
| L3 | `command_key` merges different runs: `cargo test -p a` and `-p b`, `go test -run=TestA` and `-run=TestB`, `cd api && npm test` and `cd web && npm test`. Results: a lost loop, a false re-open, and one denial disabling wraps for every package. | Confirmed | Skip `-p` only for pytest. Limit the `-r…` noise to pytest. Prefix the key with the leading cd dirs | S |
| L4 | Alternating `| tail -3` and `| tail -1` hides a loop: the summary-only run makes the key forget the test signature. | Confirmed | Treat a summary-only run as partial evidence: an attempt, not a reset | S–M |
| L5 | The normalizer merges different failures: `(3, 2)` vs `(4, 0)`, `/api/v1/orders` vs `/api/v3/items`, `{"qty":1}` vs `{"qty":7}`, `'5s'` vs `'7s'`. It splits equal ones: `due to 3 previous errors` vs `due to 2 previous errors`. | Confirmed | Apply the line:col, path, and `:\d+` rules only next to filenames. Add `id='\d+'` and `due to \d+ previous` | S |
| L6 | Unittest `FAILED (failures=1)` becomes test `(failures=1)`. Pytest ids with spaces are dropped. A passing jest run with `● Validation Warning` counts as a failure. | Confirmed | Fix the `FAILED` regex, add a unittest parser, and ignore `● … Warning` / `● Console` | S |
| L7 | Runners missed: `python -m unittest`, `manage.py test`, `pnpm vitest`, `yarn jest`, `bunx vitest`, `make -C api test`, `npm run test:unit`, `uv run --with X pytest`, `pnpm --filter`, `deno test`, `dotnet test`. | Confirmed | Extend `runner_of` (skip `-C`, `--filter`, `--with`; accept `test:*`), with fixtures | M |

## 5. Shaping and classification

| # | Finding | Evidence | Fix | Size |
|---|---|---|---|---|
| S1 | `BUILD` and `INSTALL` match anywhere: `grep -rn make src/`, `rg gcc`, `git commit -m 'make it faster'`, and `grep 'pip install' docs` are Build or Install, and so are shaped under C1. | Confirmed | Match the program in command position over parsed segments, as `runner_of` does | S |
| S2 | An important block keeps its *first* 30 (or 12) lines, so deep Python tracebacks lose the innermost frame, where the bug is. With a custom exception, the error line itself is lost. | Confirmed | For `Traceback` and `at …` stacks, keep the first line plus the last N lines, with a marker | S |
| S3 | A `#` comment consumes the newline (`shell.rs:77`). `cd api  # go\npytest` misses the runner. `cd x # c\nsed -i … tests/t.py` skips the Bash-write guard. | Confirmed | Stop before the `\n`. One line | S |
| S4 | `$'a\'b'` swallows the rest of the line into one word. | Confirmed | Parse ANSI-C quoting, or mark it complex | S |

## 6. Resources and data

| # | Finding | Evidence | Fix | Size |
|---|---|---|---|---|
| R1 | Sessions are never cleaned up. Every `tend claude --version` or `tend claude mcp …` makes a session. A failed launch (`claude` not found) leaves an empty session, and it becomes the "latest" for `tend report`. | Confirmed | Auto-prune at launch (`keep_days`, default 30). Delete a session with 0 events at exit. Check the agent binary before creating anything | S |
| R2 | Disk use: ~845 B of ledger per tool call. Blobs are ~1.04× all output plus every Edit/Write old and new text (see F4). | Measured | F4 plus R1. Optionally compress blobs | S |
| R3 | `/clear`, `/resume`, and `/compact` change `session_id` or the SessionStart `source`. tend keeps one ledger and carries loop and edit state across. | Plausible | Start a new segment and reset per-conversation state on a new `session_id` or a non-startup source | M |
| R4 | `Stop` clears `snapshots` and `wrapped` while a background subagent's Bash call may still be running. | Plausible | Clear only entries older than the turn, or keep them until PostToolUse | S |

## 7. CLI and UX

| # | Finding | Fix | Size |
|---|---|---|---|
| U1 | `tend report --session` wants a path. The ids that `tend sessions` prints fail with "open 1792…/ledger.jsonl". | Accept an id, a unique prefix, a path, or `-1`/`-2` everywhere (`report`, `why`, `show`, `flag`, `allow`, `mode`) | S |
| U2 | `tend sessions` shows raw paths and entry counts only. | Show columns: start time, project (cwd from `session_started`), mode, duration, tool calls, flags, live or ended | S |
| U3 | `tend report`, `why`, and `show` default to the newest session anywhere, often an empty one. | Default to the newest session for the current directory, and name it in the first line | S |
| U4 | The report prints ~12 lines of zeros ("Re-opens: 0 · Loops: 0 · Edit cycles: 0 …"). | Lead with "Needs attention" (guard flags, loops, undelivered, flags), then the numbers, and hide zero sections behind `--full` | S |
| U5 | Outside a live session, `tend mode` and `tend allow` print raw socket errors. | "No live tend session (start one with `tend claude`)". When two are live, ask which one | S |
| U6 | Config errors go only to `daemon.log`. One unknown key discards every valid key. Zero values (`shape_threshold_tokens = 0`, `idle_gap_minutes = 0`) are accepted. | Validate in the launcher and print to stderr. Apply valid keys and warn on unknown ones ("did you mean"). Bound the values | S |
| U7 | `tend claude -p hi --tend-mode optimize` passes `--tend-mode` to claude. `tend claude --help` shows tend's help. A user's own `--settings` probably drops tend's hooks. | Pull out `--tend-mode` anywhere. Pass `-h` through. Merge or warn on a user `--settings` | S |
| U8 | `--help` lists the unbuilt `codex`, `init`, and `undo`. | Hide them until they exist | S |
| U9 | The README says "Phase 0/1 starter" and tells you to build and put `target/release` on PATH. | Update the status. Install with `cargo install --path crates/tend-cli` | S |
| U10 | There is no self-check. | `tend doctor`: claude version against conformance, binary on PATH matches the build, statusline set, data dir permissions, config valid | S–M |
| U11 | Launcher startup adds 23 ms, mostly a 20 ms poll sleep. | Poll every 2 ms (about 3 ms total) | S |

## 8. Tests and CI

| # | Item | Size |
|---|---|---|
| T1 | **Guard table test.** One weakening and one neutral case per rule and language, from real diffs in `fixtures/guard/`, plus `is_test_path` cases. All 12 rule mutations survive today. | M |
| T2 | **Fail-open e2e tests.** `tend run` and the shim against a missing socket, and against a listener that never answers: raw bytes back, the exit code kept, within the timeout. No daemon contact for a signaled run or one over 8 MiB. The launcher with a broken `TEND_HOME` still runs the agent. | M |
| T3 | **Undelivered branches.** Move the daemon's late and undelivered decision into a pure function with an injected clock. Test both branches, the margin, and the `last_summary` and `last_blocked` resets. | M |
| T4 | **Property tests.** For shape: `extract_failures(shape(x)) ⊇ extract_failures(x)`, line counts add up, and the cut marker is kept. For signatures: noise insertions do not change them and value changes do. For `command_key`: a different selection gives a different key. | M |
| T5 | **Fuzz** `shell::parse`, `leading_cd`, `command_key`, and `test_write_targets` (`cargo-fuzz` is installed), plus a differential test against Python `shlex`. | M |
| T6 | **Flaky tests.** `large_filter_does_not_block_pings_or_hooks` failed 1 run in 6 under load and is vacuous when it takes no samples. The orphan test relies on fixed sleeps. | S |
| T7 | **e2e hardcodes `session/tendd.sock`.** The daemon falls back to `/tmp/tend-<uid>/` for long paths, so 2 tests fail with a 57-character TMPDIR (macOS has ~8 bytes of margin). | S |
| T8 | **Real captures** for the synthetic fixtures: PermissionDenied, a PreToolUse Bash payload, PostToolUseFailure for a wrapped run, a subagent transcript, jest output cut by Claude Code. List the hand-made ones in `fixtures/README.md`. | S each |
| T9 | **CI.** It has never run (C5). Pin the toolchain, set `rust-version = "1.87"` (`std::io::pipe`), add a shim p95 benchmark gate (rule 7), a 60 s fuzz smoke job, and a conformance `--replay` mode over a committed capture plus a manual live job. | M |

## 9. Portability

- **X1.** tend-cli does not build on Windows, and nothing says so. Add `#[cfg(not(unix))] compile_error!(…)` and declare `rust-version` (S).
- **X2.** macOS: see F6 (no PDEATHSIG) and T7 (socket path margin). N15 still needs a Mac.

## 10. Simplify

- **Same-failure judgments.** `same_failure` is asked only when the signatures already match, so it always answers "same": 580 entries for 20 failures × 30 runs. Record it only when a decider is registered (`controller.rs:553`, `broker.rs:96`).
- **Wasted parsing.** `prepare` runs `extract_failures` for every kind, then drops the result for non-test kinds: 184 ms per 16 MB view. Pass the kind in.
- **Double hashing.** `on_bash` and `prepare` both hash the output. Hash once.
- **Wrong fallback.** `session.rs:58` falls back to `DEFAULT_LEVEL` for an unknown kind, against the C1 allow-list. Use 0.
- **Unused flag.** `CommandLine.complex` is never read outside `shell.rs`. Use it (S4) or drop it.
- **Big controller.** Split `controller.rs` (2,060 lines) into loop, guard, and shape handlers when F, G, and L touch it.

## 11. Measurements (release build, WSL2)

| What | Result |
|---|---|
| Shim, PreToolUse | p50 0.8–1.0 ms, p95 1.0–1.6 ms (up to 10k calls) |
| Shim, PostToolUse 25 KB | p50 1.6 ms, p95 1.8 ms |
| Shim with no daemon (fail open) | 0.65 ms |
| 32 parallel 25 KB posts | p95 19 ms, max 80 ms, all recorded |
| 16 parallel 1 MB posts | p50 334 ms, over the 250 ms timeout (only Write content gets this large) |
| Daemon RSS | 3.7 MB idle, 9.7 MB at 4k and at 20k hooks (bounded); spikes to file size on a large Write (F4) |
| `tend report` on 10k calls | 60 ms, 30 MB |
| `cargo test`, warm | 2.0 s (e2e 1.6 s); 15 runs in a row passed on an idle machine |

## 12. Suggested batches

1. **Rule 1 (one sitting):** F1, F2, F3, F7, F9, plus T2 for them.
2. **Guard (a day):** G1–G6 with T1. This is where tend's value claim lives (checkpoint §6 A1).
3. **Loops and classification (a day):** L1–L6, S1–S3, with T4.
4. **Resources and UX (a day):** F4, F5, R1, U1–U9.
5. **Tests and CI:** T3, T5–T9, X1, once a remote exists.
6. Later: F6, F8 + P1, R3, L7, U10, the simplifications.
