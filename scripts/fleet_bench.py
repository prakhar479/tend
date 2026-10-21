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
args = ap.parse_args()

TASK = "Fix the bug in calc.py so that `python3 test_calc.py` passes."
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
    open(os.path.join(repo, "calc.py"), "w").write("def add(a, b):\n    return a - b\n")
    open(os.path.join(repo, "test_calc.py"), "w").write(
        "from calc import add\n\nassert add(2, 3) == 5, 'add(2, 3) should be 5'\nprint('ok')\n")
    fleet = f'preset = "{preset}"\ncheck = "python3 test_calc.py"\n{extra}\n'
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
    t("task", "add", TASK, "--role", entry)
    board = ""
    while time.time() - t0 < args.timeout:
        time.sleep(4)
        board = t("task", "list")
        states = re.findall(r"^  (t-[\d.]+)\s+(\w+)", board, re.M)
        if states and all(s in ("done", "failed", "blocked", "review") for _, s in states):
            break
    minutes = (time.time() - t0) / 60
    report = t("fleet", "report")
    t("fleet", "down")
    states = re.findall(r"^  (t-[\d.]+)\s+(\w+)", board, re.M)
    root = dict(states).get("t-1", "?")
    timed_out = not states or any(s not in ("done", "failed", "blocked", "review") for _, s in states)
    m = re.search(r"attempts that did not count: (\d+)", report)
    cost = re.search(r"\$(\d+\.\d+)", report)
    fixed = "return a + b" in "".join(
        open(os.path.join(dp, f)).read()
        for dp, _, fs in os.walk(os.path.join(home)) for f in fs if f == "calc.py")
    return {
        "preset": preset,
        "outcome": "timeout" if timed_out else root,
        "tasks": len(states),
        "lost_attempts": int(m.group(1)) if m else None,
        "fixed_in_a_worktree": fixed,
        "minutes": round(minutes, 1),
        "cost_usd": float(cost.group(1)) if cost else None,
    }


rows = []
for p in PRESETS:
    if only and p not in only:
        continue
    print(f"running {p} …", flush=True)
    rows.append(run(p))
    print("  ", json.dumps(rows[-1]), flush=True)
print("\n| preset | outcome | tasks | attempts lost | fixed | min | $ |")
print("|---|---|---|---|---|---|---|")
for r in rows:
    print(f"| {r['preset']} | {r['outcome']} | {r['tasks']} | {r['lost_attempts']} | {r['fixed_in_a_worktree']} | {r['minutes']} | {r['cost_usd']} |")
