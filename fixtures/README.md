# Fixtures

| Path | Content | Status |
|---|---|---|
| `outputs/pytest_fail.txt` | pytest -v: 300 passed, 2 failed | Synthetic |
| `outputs/cargo_fail.txt` | cargo test: 148 passed, 2 failed | Synthetic |
| `outputs/go_test_fail.txt`, `go_test_v_fail.txt` | go test, plain and `-v`: 3 failures (one subtest, one panic) | Captured 2026-10-19 (go 1.22) |
| `outputs/jest_fail.txt` | jest 29 `--verbose`: 3 failed, 30 passed | Captured 2026-10-19 |
| `outputs/vitest_fail.txt` | vitest 2.1 `--reporter=verbose`: 3 failed, 30 passed | Captured 2026-10-19 |
| `outputs/repeated_traces.txt` | Python worker: the same traceback 50 times | Captured 2026-10-19 |
| `claude_code/*.json` | Claude Code hook payloads | Captured 2026-10-19, Claude Code 2.1.303 (TASKS.md S1) |
| `transcripts/loop_session.jsonl` | Claude Code transcript: 4 failing runs, 3 edits | Captured 2026-10-19 (TASKS.md S5). Only user/assistant lines and the fields rein reads |

Rules:

- Every parser change needs a fixture here and a test.
- Captured payloads come from `<session>/hooks.jsonl` (run the session with `REIN_CAPTURE=1`). Make fixtures with `scripts/capture_fixtures.py`; it removes personal paths and ids and refuses output that still has `$HOME` or `$USER`. Read the result before you commit it.
- In captures, paths are `/home/dev/shop`, the session id is `9b1d6f2e-0000-4000-8000-000000000001`, and tool-use ids are `toolu_0N`.
