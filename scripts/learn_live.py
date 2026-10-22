#!/usr/bin/env python3
"""Live check of shared learning (F2) and merging (M1) in fleets.

Usage: scripts/learn_live.py <tend binary> <workdir> [--model haiku] [--timeout 400]

One repo (the hard bench's `kv/`), one TEND_HOME, `learn = on`, optimize mode.
- The user keeps a note (pending without a terminal, then approved).
- Run 1: `lead`, split task. Workers fail, edit, pass: learned fixes.
- Run 2: `pool`, the same task from the same base, in the same repo: do its sessions
  get the note, and are fixes recalled when they hit a known failure twice?
Prints what each run learned, recalled, and was told, and whether one worktree passes.
"""
import argparse, glob, json, os, re, shutil, subprocess, sys, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
ap = argparse.ArgumentParser()
ap.add_argument("tend")
ap.add_argument("workdir")
ap.add_argument("--model", default="haiku")
ap.add_argument("--timeout", type=int, default=400)
args = ap.parse_args()

# The hard bench's repo and task (scripts/fleet_bench.py).
src = open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "fleet_bench.py")).read()
ns = {}
exec(src[src.index("HARD_TASK ="):src.index("# preset:")], ns)
HARD, HARD_TASK = ns["HARD"], ns["HARD_TASK"]
SPLIT = (" Do not change code yourself: hand out one worker task per test file with "
         "`tend task add` (summarize depends on the other three), and finish when they are done.")
NOTE = "tests import kv from the repo root: run them as `python3 run_tests.py <name>` from there."

W = os.path.abspath(args.workdir)
shutil.rmtree(W, ignore_errors=True)
repo, home = os.path.join(W, "repo"), os.path.join(W, "home")
os.makedirs(home)
for path, text in HARD.items():
    os.makedirs(os.path.dirname(os.path.join(repo, path)) or repo, exist_ok=True)
    open(os.path.join(repo, path), "w").write(text)
open(os.path.join(repo, ".gitignore"), "w").write("__pycache__/\nfleet-*.toml\n")
config = os.path.join(W, "config.toml")
open(config, "w").write("learn = on\n")
git = ["git", "-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"]
for c in (["init", "-q"], ["add", "."], ["commit", "-qm", "base"]):
    subprocess.run(git + c, cwd=repo, check=True, capture_output=True)
env = dict(os.environ, TEND_HOME=home, TEND_CONFIG=config)
env.pop("TEND_SESSION_DIR", None)
env.pop("TEND_FLEET", None)
env.pop("TEND_FLEET_SLOT", None)


def t(*a):
    r = subprocess.run([args.tend, *a], cwd=repo, env=env, capture_output=True, text=True,
                       stdin=subprocess.DEVNULL)
    return r.stdout + r.stderr


def sessions():
    return set(glob.glob(os.path.join(home, "sessions", "*")))


def bodies(dirs):
    out = []
    for d in sorted(dirs):
        try:
            for line in open(os.path.join(d, "ledger.jsonl")):
                try:
                    out.append((os.path.basename(d), json.loads(line)["body"]))
                except (ValueError, KeyError):
                    pass
        except OSError:
            pass
    return out


def fleet(preset, task, check=None):
    fl = (f'preset = "{preset}"\ncheck = "python3 run_tests.py"\n'
          + "".join(f'[role.{r}]\nmodel = "{args.model}"\npermission_mode = "bypassPermissions"\n\n'
                    for r in (["lead", "worker"] if preset == "lead" else ["worker"]))
          + "[limits]\nsessions = 4\n")
    open(os.path.join(repo, f"fleet-{preset}.toml"), "w").write(fl)
    before = sessions()
    t0 = time.time()
    print(t("fleet", "up", f"fleet-{preset}.toml", "--mode", "optimize").strip())
    entry = "lead" if preset == "lead" else "worker"
    print(t("task", "add", task, "--role", entry, *(["--check", check] if check else [])).strip())
    board = ""
    while time.time() - t0 < args.timeout:
        time.sleep(5)
        board = t("task", "list")
        states = re.findall(r"^  (t-[\d.]+)\s+(\w+)", board, re.M)
        if states and all(s in ("done", "failed", "blocked", "review") for _, s in states):
            break
    report = t("fleet", "report")
    wts = sorted(glob.glob(os.path.join(home, "fleets", "*", "worktrees", "*")))
    passing = [w for w in wts
               if subprocess.run(["python3", "run_tests.py"], cwd=w, capture_output=True).returncode == 0]
    # What each worker said when it finished (run 3 says whether tend helped).
    said = []
    for led in glob.glob(os.path.join(home, "fleets", "*", "ledger.jsonl")):
        if os.path.getmtime(led) < t0:
            continue
        for line in open(led):
            ev = json.loads(line).get("body", {}).get("event", {})
            if ev.get("type") == "task_done":
                said.append(ev.get("summary", ""))
    t("fleet", "down")
    new = sessions() - before
    b = bodies(new)
    actions = [(s, x["action"]) for s, x in b if x.get("kind") == "action"]
    row = {
        "preset": preset,
        "minutes": round((time.time() - t0) / 60, 1),
        "board": re.findall(r"^  (t-[\d.]+)\s+(\w+)", board, re.M),
        "sessions": len(new),
        "learned": [(s, x["message"][:60], x["command"], x["files"]) for s, x in b if x.get("kind") == "learned"],
        "recalls": [(s, a["text"]) for s, a in actions if a.get("type") == "recall"],
        "said": said,
        "notes_given": sum(1 for _, a in actions if a.get("type") == "notes"),
        "worktrees_passing": [os.path.basename(w) for w in passing],
        "cost": (re.search(r"\$(\d+\.\d+)", report) or [None, None])[1],
        "merge_notices": [l for l in report.splitlines() if "conflict" in l.lower()],
    }
    return row, board, report


print(t("note", NOTE).strip())
print(t("notes").strip())
print(t("notes", "approve", "n-1").strip())

rows = []
# Run 3 makes a second hit happen (an agent whose first try did not work), so the
# recall is delivered; its transcript shows what the agent did with it.
AGAIN = ("Make tests/test_{m}.py pass. Before you change anything, run `python3 run_tests.py {m}`, "
         "then run it again as a separate command. In your `tend task done` summary, say in one "
         "line whether tend told you anything useful, and what.")


def learned_module():
    """A module whose fix run 1 learned (the runs vary), for run 3 to hit."""
    try:
        for line in open(os.path.join(home, "learn", "fixes.jsonl")):
            word = json.loads(line)["command"].split()[-1]
            if word in ("parse", "fmt", "stats", "report"):
                return word
    except OSError:
        pass
    return "fmt"


for i, (preset, task, check) in enumerate((("lead", HARD_TASK + SPLIT, None), ("pool", HARD_TASK, None),
                                           ("pool", AGAIN, "python3 run_tests.py {m}"))):
    if i == 2:
        m = learned_module()
        task, check = task.format(m=m), check.format(m=m)
    print(f"\n== run {preset}", flush=True)
    row, board, report = fleet(preset, task, check)
    print(board)
    print(json.dumps(row, indent=1), flush=True)
    rows.append(row)
    # Run 2 starts from the same base: run 1's work is on tend/ branches only.
    subprocess.run(["git", "status", "--short"], cwd=repo)

fixes = open(os.path.join(home, "learn", "fixes.jsonl")).read().splitlines() if os.path.exists(
    os.path.join(home, "learn", "fixes.jsonl")) else []
print(f"\nfixes.jsonl: {len(fixes)} lines; projects: {sorted({json.loads(l)['project'] for l in fixes})}")
