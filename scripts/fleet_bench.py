#!/usr/bin/env python3
"""G10: one seeded run per fleet preset, on a small model.

Usage: scripts/fleet_bench.py <tend binary> <workdir> [--model haiku] [--only lead,pool]

Each preset gets the same repo (a bug in calc.py, a test, `check = "python3 test_calc.py"`)
and one task for its entry role. The run ends when every task has ended (done, failed,
blocked, or in review) or after a timeout. It prints, per preset: outcome, attempts that
did not count, tasks, minutes, and cost. Results: docs/TASKS.md, G10.
"""
import argparse, json, os, re, shutil, subprocess, sys, time

ap = argparse.ArgumentParser()
ap.add_argument("tend")
ap.add_argument("workdir")
ap.add_argument("--model", default="haiku")
ap.add_argument("--only", default="")
ap.add_argument("--timeout", type=int, default=300)
ap.add_argument("--split", action="store_true",
                help="tell the entry role to hand out one task per part, not to edit code itself")
ap.add_argument("--task", choices=["small", "hard"], default="small",
                help="small: one bug; hard: three bugs in three modules and a new function")
args = ap.parse_args()

TASK = "Fix the bug in calc.py so that `python3 test_calc.py` passes."
HARD_TASK = ("`python3 run_tests.py` fails: tests in tests/test_parse.py, tests/test_fmt.py and "
             "tests/test_stats.py fail, and report.summarize() in tests/test_report.py does not exist yet. "
             "Make all four test files pass. `python3 run_tests.py <name>` runs one file (parse, fmt, stats, report).")
HARD = {
    "kv/__init__.py": "",
    "kv/parse.py": "def parse_kv(text):\n    \"\"\"'a=1;b=2' -> {'a': '1', 'b': '2'}\"\"\"\n    out = {}\n    for part in text.split(','):\n        if part:\n            k, v = part.split('=')\n            out[k] = v\n    return out\n",
    "kv/fmt.py": "def fmt_money(x):\n    \"\"\"1234.5 -> '1,234.50'\"\"\"\n    return '%.2f' % x\n",
    "kv/stats.py": "def median(xs):\n    n = len(xs)\n    mid = n // 2\n    if n % 2:\n        return xs[mid]\n    return (xs[mid - 1] + xs[mid]) / 2\n",
    "kv/report.py": "# summarize(text): parse 'k=v;...' pairs, take the median of the values as numbers,\n# and return 'median <money>' using fmt_money.\n",
    "tests/test_parse.py": "from kv.parse import parse_kv\n\ndef test_semicolons():\n    assert parse_kv('a=1;b=2') == {'a': '1', 'b': '2'}\n\ndef test_empty():\n    assert parse_kv('') == {}\n",
    "tests/test_fmt.py": "from kv.fmt import fmt_money\n\ndef test_thousands():\n    assert fmt_money(1234.5) == '1,234.50'\n\ndef test_small():\n    assert fmt_money(3) == '3.00'\n",
    "tests/test_stats.py": "from kv.stats import median\n\ndef test_unsorted_odd():\n    assert median([3, 1, 2]) == 2\n\ndef test_unsorted_even():\n    assert median([4, 1, 3, 2]) == 2.5\n",
    "tests/test_report.py": "from kv.report import summarize\n\ndef test_summary():\n    assert summarize('a=1;b=3;c=2') == 'median 2.00'\n\ndef test_big():\n    assert summarize('x=1000;y=3000') == 'median 2,000.00'\n",
    "run_tests.py": "import importlib, sys, traceback\nnames = sys.argv[1:] or ['parse', 'fmt', 'stats', 'report']\nfailed = 0\nfor n in names:\n    try:\n        m = importlib.import_module('tests.test_' + n)\n        fns = [getattr(m, f) for f in dir(m) if f.startswith('test_')]\n    except Exception:\n        print('ERROR tests/test_%s.py' % n); traceback.print_exc(); failed += 1; continue\n    for f in fns:\n        try:\n            f(); print('ok', n, f.__name__)\n        except Exception:\n            failed += 1; print('FAILED tests/test_%s.py::%s' % (n, f.__name__)); traceback.print_exc()\nprint('%d failed' % failed)\nsys.exit(1 if failed else 0)\n",
    "tests/__init__.py": "",
}
# preset: (its roles, the entry role, extra fleet-file lines)
PRESETS = {
    "lead": (["lead", "worker"], "lead", ""),
    "pool": (["worker"], "worker", ""),
    "pipeline": (["plan", "implement", "verify"], "plan", ""),
    "review": (["author", "reviewer"], "author", "max_rounds = 2\n"),
    "best-of": (["candidate", "judge"], "candidate", 'judge = "rule"\n'),
    "specialists": (["lead", "tester", "implementer"], "lead", ""),
}
only = [p for p in args.only.split(",") if p]
W = os.path.abspath(args.workdir)


def sh(cmd, cwd, env=None, check=True):
    return subprocess.run(cmd, cwd=cwd, env=env, check=check, capture_output=True, text=True)


def run(preset):
    roles, entry, extra = PRESETS[preset]
    d = os.path.join(W, preset)
    shutil.rmtree(d, ignore_errors=True)
    repo, home = os.path.join(d, "repo"), os.path.join(d, "home")
    os.makedirs(repo)
    os.makedirs(home)
    if args.task == "hard":
        for path, text in HARD.items():
            os.makedirs(os.path.dirname(os.path.join(repo, path)) or repo, exist_ok=True)
            open(os.path.join(repo, path), "w").write(text)
        open(os.path.join(repo, ".gitignore"), "w").write("__pycache__/\n")
        check, task = "python3 run_tests.py", HARD_TASK
        if args.split:
            task += (" Do not change code yourself: hand out one worker task per test file with "
                     "`tend task add` (summarize depends on the other three), and finish when they are done.")
    else:
        open(os.path.join(repo, "calc.py"), "w").write("def add(a, b):\n    return a - b\n")
        open(os.path.join(repo, "test_calc.py"), "w").write(
            "from calc import add\n\nassert add(2, 3) == 5, 'add(2, 3) should be 5'\nprint('ok')\n")
        check, task = "python3 test_calc.py", TASK
    fleet = f'preset = "{preset}"\ncheck = "{check}"\n{extra}\n'
    for r in roles:
        fleet += f'[role.{r}]\nmodel = "{args.model}"\npermission_mode = "bypassPermissions"\n\n'
    fleet += "[limits]\nsessions = 4\n"
    open(os.path.join(repo, "fleet.toml"), "w").write(fleet)
    git = ["git", "-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"]
    sh(["git", "init", "-q"], repo)
    sh(git + ["add", "."], repo)
    sh(git + ["commit", "-qm", "base"], repo)
    env = dict(os.environ, TEND_HOME=home)
    t = lambda *a: sh([args.tend, *a], repo, env, check=False).stdout
    t0 = time.time()
    t("fleet", "up", "fleet.toml")
    t("task", "add", task, "--role", entry)
    board = ""
    while time.time() - t0 < args.timeout:
        time.sleep(4)
        board = t("task", "list")
        states = re.findall(r"^  (t-[\d.]+)\s+(\w+)", board, re.M)
        if states and all(s in ("done", "failed", "blocked", "review") for _, s in states):
            break
    minutes = (time.time() - t0) / 60
    report = t("fleet", "report")
    # Which test files pass in some worktree, and does one worktree pass them all?
    parts, whole = set(), False
    if args.task == "hard":
        for wt in glob_worktrees(home):
            ok = [n for n in ("parse", "fmt", "stats", "report")
                  if subprocess.run(["python3", "run_tests.py", n], cwd=wt, capture_output=True).returncode == 0]
            parts.update(ok)
            whole = whole or len(ok) == 4
    t("fleet", "down")
    states = re.findall(r"^  (t-[\d.]+)\s+(\w+)", board, re.M)
    root = dict(states).get("t-1", "?")
    timed_out = not states or any(s not in ("done", "failed", "blocked", "review") for _, s in states)
    m = re.search(r"attempts that did not count: (\d+)", report)
    cost = re.search(r"\$(\d+\.\d+)", report)
    fixed = (len(parts) == 4) if args.task == "hard" else "return a + b" in "".join(
        open(os.path.join(dp, f)).read()
        for dp, _, fs in os.walk(os.path.join(home)) for f in fs if f == "calc.py")
    sessions = re.search(r"Sessions: (\d+)", report)
    return {
        "preset": preset,
        "sessions": int(sessions.group(1)) if sessions else None,
        "parts_fixed": f"{len(parts)}/4" if args.task == "hard" else None,
        "one_worktree_passes_all": whole if args.task == "hard" else None,
        "outcome": "timeout" if timed_out else root,
        "tasks": len(states),
        "lost_attempts": int(m.group(1)) if m else None,
        "fixed_in_a_worktree": fixed,
        "minutes": round(minutes, 1),
        "cost_usd": float(cost.group(1)) if cost else None,
    }


def glob_worktrees(home):
    out = []
    for f in os.listdir(os.path.join(home, "fleets")):
        d = os.path.join(home, "fleets", f, "worktrees")
        if os.path.isdir(d):
            out += [os.path.join(d, w) for w in sorted(os.listdir(d))]
    return out


rows = []
for p in PRESETS:
    if only and p not in only:
        continue
    print(f"running {p} …", flush=True)
    rows.append(run(p))
    print("  ", json.dumps(rows[-1]), flush=True)
print(f"\ntask: {args.task}\n| preset | outcome | tasks | sessions | attempts lost | parts fixed | one worktree passes all | min | $ |")
print("|---|---|---|---|---|---|---|---|---|")
for r in rows:
    print(f"| {r['preset']} | {r['outcome']} | {r['tasks']} | {r['sessions']} | {r['lost_attempts']} | {r['parts_fixed'] or r['fixed_in_a_worktree']} | {r['one_worktree_passes_all']} | {r['minutes']} | {r['cost_usd']} |")
