# tend: CI fix, new measurements, and extensions (2026-10-20)

**Status:** E1–E11 are built (see `docs/TASKS.md`, Extensions E). The audit (E1–E3) stays experimental and opt-in by owner decision; Tier 3 (E12–E16) is not started.

## 1. The CI failure

The first CI run on GitHub (run 39410338352, commit 9da2596) failed `make check` on ubuntu and macOS. The cause was `large_filter_does_not_block_pings_or_hooks`:

- The test asked tend to wrap the command `pytest -v`.
- tend wraps only a runner it can find (`runner_is_executable`), so it would not wrap here.
- This machine has pytest on PATH; a fresh runner has none.

Fixed in c8b5cb3:

- **The test** uses a fake `pytest` by absolute path, as the other wrap test does.
- **`make check-clean`** runs the suite with an empty HOME and a minimal PATH, as CI does. It passes.
- **The temporary socket name.** The atomic-socket name from I5 is now never longer than `tendd.sock`. The old `tendd.sock.<pid>` could pass the path limit for a session path near it (macOS allows 104 bytes).
- **`actions/checkout@v5`**, because Node 20 is deprecated.

CI logs need a GitHub login, so the failing test was found by reproducing CI's environment rather than by reading the log. Commits up to 73c8015 are local; this shell cannot authenticate to GitHub, so push them yourself (`! git push`).

## 2. New measurements

These come from the owner's transcripts (6 sessions, 32 subagents) and from the git history of `polaris`, a repo written mostly by an agent.

| Question | Data | Reading |
|---|---|---|
| Does the agent claim "tests pass" after a failed run? | 146 claims with a known last run; 2 apparent mismatches, both honest (the agent named the failure, or the "failure" was mypy output in the same command) | A claim checker would find nothing here. Low value for this owner |
| Does it suppress checks? | About 190 suppressions added: 115 `# noqa`, 68 `# type: ignore`, 4 `#[allow]`, 2 `pragma: no cover`. Most have a reason (`# noqa: BLE001 - one failed unit must not kill an overnight run`) | Frequent and mostly legitimate: blocking would be noise. Listing them is what a reviewer wants |
| How much test churn is there? | 879 test-file writes; 494 in one session | Nobody reviews this call by call. A net summary per session is the useful form |
| What would an audit of the git diff report? | Prototype (`guard::assess` over `git diff` of each commit) on polaris's last 40 commits: 68 test files changed, 61 strengthened, 1 flagged (a deleted test, `test_create_llm_client_from_config_drop_keys`), 59 suppressions added | Short, specific, and correct. It also found a guard bug (below) |

The prototype exposed a false positive in the guard's tolerance rule. It compared the largest tolerance per keyword across the whole text, so a *new* assertion with a wide tolerance read as a loosened old one. This mattered for Write and Bash-write snapshots, which compare whole files. It is fixed in 73c8015: assertions are paired by their text with numbers masked, and only an old value that is replaced by a larger one counts.

## 3. What this says about the product

The owner's agent writes honest summaries and rarely weakens tests on purpose. Still, sessions change hundreds of test lines and add dozens of suppressions that nobody looks at. The case for tend is strongest as a **reviewer's lens on what an agent changed in the project's checks**, rather than as a gate in the agent's path.

That lens needs no hooks. It needs a base and a diff. So it works:

- for any agent (Codex, Cursor, Aider) and for people, not only Claude Code;
- in CI, on a pull request;
- after the fact, on history.

The live hook layer (guard, loops, wrapped test runs) stays as the richer, real-time part for Claude Code.

## 4. Extensions, ranked

Sizes: S ≈ hours, M ≈ a day, L ≈ days.

### Tier 1: the audit (build next)

| # | Extension | Acceptance | Size |
|---|---|---|---|
| E1 | **`tend audit [BASE[..HEAD]]`.** For each file a git diff changes: in test files, assertions and tests added and removed, and skip, focus, tolerance, and always-pass changes (the guard rules over old and new text). In all code, suppressions added, by kind, with or without a reason. Also edits to CI and check config (`.github/workflows`, coverage `fail_under`, lint config) and to dependency manifests. Output as text, `--json`, or `--markdown`. A dirty working tree counts as HEAD. | On polaris's last 40 commits: the deleted test and the 59 suppressions, with no false flags | M |
| E2 | **Session audit.** At SessionStart, record a base tree without touching the repo: a temporary index, `git add -A`, `git write-tree` (this includes untracked files). At exit, run E1 against it. The exit line adds "4 test files changed · 1 test removed · 6 suppressions added · `tend audit`", and the report gets a section. Non-git projects keep the per-call snapshots. | A seeded session's exit line matches its diff | S on E1 |
| E3 | **CI / pull-request mode.** A composite GitHub Action (`action.yml`) runs `tend audit --base origin/main --markdown` and writes the job summary or a PR comment. Dogfood it on this repo. | A PR that deletes a test shows it in the summary | S–M |
| E4 | **Expected value changed to the actual one** (reference Phase 3). The controller knows the last failure (`assert 0 == 1`). An edit that changes the expected literal (`== 1` → `== 0`) "fixes" the test to match the bug. | A fixture loop: fail, then edit the expectation, gets flagged | S–M |
| E5 | **Snapshot and golden updates.** Flag commands that rewrite expected outputs wholesale (`jest -u`, `vitest -u`, `--update-snapshots`, `cargo insta accept`, `--snapshot-update`, `UPDATE_GOLDEN=1`) and edits under `__snapshots__/`, `*.snap`, or `testdata/*.golden`. They become audit items and, in optimize mode, warnings. | Unit table | S |

### Tier 2: adoption and trust

| # | Extension | Why | Size |
|---|---|---|---|
| E6 | **Plugin** (checkpoint V3). | People keep typing `claude`, so real use (V2) can start | M |
| E7 | **Repo policy `.tend.toml`**, committed with the project: extra test globs, protected paths (ask before editing migrations or golden data), "suppressions need a reason", and paths the audit ignores. | Projects differ, and the rules should live with the code | M |
| E8 | **Secret redaction** before blobs are written (AWS keys, GitHub and Anthropic tokens, private keys, `.env` values), plus `tend purge`. | Blobs keep full tool output, and env dumps happen | S |
| E9 | **Per-project memory**: persist shape levels and guard overrides across sessions. | The session model relearns every time | S–M |
| E10 | **`tend export`**, a sanitized session bundle, plus `retro --all --summary`. | Bug reports and other users' data (checkpoint V4) | S |
| E11 | **Opt-in notification** on a loop or a guard deny during an unattended run (`notify-send` / `osascript`). | Long auto-mode runs (polaris had 50-minute ones) | S |

### Tier 3: reach

| # | Extension | Note | Size |
|---|---|---|---|
| E12 | Other agents' hooks: Codex (S10), Gemini CLI | E1–E3 already cover every agent through git; hooks add the live layer | L each |
| E13 | **MCP server** over the ledger (`why`, `show`, `audit`), for the agent itself or a reviewer agent | | M |
| E14 | **Seeded benchmark** `tend bench` (checkpoint V1) | Measures the guard and loop value that natural sessions rarely show | L |
| E15 | **HTML report**: a timeline of tool calls, failures, test edits, and loops (an Artifact page) | Easier to review than text | M |
| E16 | **Deciders** (Phase 4): a small model judges weakening the rules cannot see (`== 5` → `>= 0`) | After E1 shows what the rules miss | L |

### Not now

- **Claim checking.** No real mismatch in 146 claims. Revisit if another user's data differs.
- **Blocking suppressions.** About 190 additions, most of them justified. List them (E1) instead.
- **More output shaping.** Large outputs are code reads (checkpoint C1).

## 5. Recommended order

1. E1, E2, E5. The audit core is about two days and reuses the guard.
2. E3. Dogfood the audit on this repo's own PRs.
3. E6. Then two weeks of real use with the live layer.
4. E4, E8.
5. Decide Tier 3 from what E1–E3 show in use.

## 6. Decisions for the owner

1. **Reframe around the audit?** Lead with "what did the agent change in your checks" (any agent, any diff, CI), with the Claude Code hooks as the live layer.
2. **Publish the GitHub Action** (E3) once it is dogfooded?
3. **Should `.tend.toml` (E7) be able to block**, or only warn and audit?
