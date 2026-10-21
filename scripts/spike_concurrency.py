#!/usr/bin/env python3
"""F-S2: several long-lived `tend claude` stream-json sessions at once.

Usage: scripts/spike_concurrency.py <tend binary> <workdir> [--sessions 4] [--model haiku]

One repo with a worktree per session, `fleet = on`, optimize mode, and 300 dead
session dirs for the fleet watchers to step over. First one session alone (baseline),
then N at once, each with the same three turns. Prints per-turn times, any retry or
rate-limit messages, CPU and memory of tend daemons and agents, and fleet notices.
Results: docs/TASKS.md, Spike results, F-S2.
"""
import argparse, json, os, queue, shutil, subprocess, sys, threading, time, uuid

ap = argparse.ArgumentParser()
ap.add_argument("tend")
ap.add_argument("workdir")
ap.add_argument("--sessions", type=int, default=4)
ap.add_argument("--model", default="haiku")
args = ap.parse_args()

W = os.path.abspath(args.workdir)
shutil.rmtree(W, ignore_errors=True)
os.makedirs(W)
HOME = os.path.join(W, "home")
os.makedirs(os.path.join(HOME, "sessions"))
for i in range(300):
    os.makedirs(os.path.join(HOME, "sessions", f"0000000000-{i}"))
with open(os.path.join(W, "config.toml"), "w") as f:
    f.write("fleet = on\n")
ENV = dict(os.environ, TEND_HOME=HOME, TEND_CONFIG=os.path.join(W, "config.toml"))

def git(cwd, *a):
    subprocess.run(["git", "-C", cwd, "-c", "user.email=t@t", "-c", "user.name=t",
                    "-c", "commit.gpgsign=false", *a], check=True, capture_output=True)

REPO = os.path.join(W, "repo")
os.makedirs(REPO)
git(REPO, "init", "-q")
with open(os.path.join(REPO, "shared.txt"), "w") as f:
    f.write("".join(f"line {i}\n" for i in range(1, 9)))
git(REPO, "add", ".")
git(REPO, "commit", "-qm", "base")

TURNS = [
    "Use the Write tool to create notes_{n}.txt containing the single line: worker {n}. Then reply with exactly: ok",
    "Use the Edit tool on shared.txt to replace 'line {n}' with 'worker {n} was here'. Then reply with exactly: ok",
    "Run this Bash command: python3 -c 'import time; time.sleep(5); print(\"built\")'  Then reply with every message starting with 'tend:' you saw in this conversation, verbatim, or the word none.",
]

class Session:
    def __init__(self, name, cwd, n):
        self.name, self.cwd, self.n = name, cwd, n
        self.events = queue.Queue()
        self.turns, self.notes, self.final = [], [], None
        cmd = [args.tend, "claude", "--tend-mode", "optimize", "-p",
               "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
               "--model", args.model, "--permission-mode", "bypassPermissions",
               "--session-id", str(uuid.uuid4())]
        self.log = open(os.path.join(W, f"{name}.jsonl"), "w")
        self.p = subprocess.Popen(cmd, cwd=cwd, env=ENV, stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE,
                                  stderr=open(os.path.join(W, f"{name}.err"), "w"),
                                  text=True, bufsize=1)
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.p.stdout:
            self.log.write(line)
            try:
                v = json.loads(line)
            except Exception:
                continue
            st = v.get("subtype") or ""
            if v.get("type") == "system" and any(k in st for k in ("retry", "rate", "limit", "error")):
                self.notes.append(st)
            if v.get("type") == "result":
                self.events.put(v)
        self.events.put(None)

    def run(self):
        for t in TURNS:
            start = time.time()
            self.p.stdin.write(json.dumps({"type": "user", "message": {
                "role": "user", "content": t.format(n=self.n)}}) + "\n")
            self.p.stdin.flush()
            r = self.events.get(timeout=300)
            self.turns.append({
                "wall_s": round(time.time() - start, 2),
                "api_s": round((r or {}).get("duration_api_ms", 0) / 1000, 2),
                "ok": bool(r) and not r.get("is_error"),
                "cost": (r or {}).get("total_cost_usd", 0),
            })
            if r:
                self.final = r.get("result")
        self.p.stdin.close()
        self.p.wait(timeout=60)

def procs():
    """pid → (name, cmdline) of tend daemons and agents under this HOME."""
    out = {}
    for pid in os.listdir("/proc"):
        if not pid.isdigit():
            continue
        try:
            cmd = open(f"/proc/{pid}/cmdline", "rb").read().split(b"\0")
            env = open(f"/proc/{pid}/environ", "rb").read()
        except Exception:
            continue
        if HOME.encode() not in env and HOME.encode() not in b" ".join(cmd):
            continue
        words = [c.decode(errors="replace") for c in cmd if c]
        if len(words) > 1 and words[1] == "daemon":
            out[int(pid)] = "daemon"
        elif any(os.path.basename(w) == "claude" or w.endswith("/claude") for w in words[:2]):
            out[int(pid)] = "agent"
    return out

def cpu_rss(pid):
    try:
        stat = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
        ticks = int(stat[11]) + int(stat[12])
        rss = int(open(f"/proc/{pid}/statm").read().split()[1]) * os.sysconf("SC_PAGE_SIZE")
        return ticks / os.sysconf("SC_CLK_TCK"), rss
    except Exception:
        return None

def phase(label, n):
    sessions = []
    for i in range(1, n + 1):
        cwd = REPO if i == 1 else os.path.join(W, f"wt{i}")
        if i > 1 and not os.path.exists(cwd):
            git(REPO, "worktree", "add", "-q", cwd)
        sessions.append(Session(f"{label}-{i}", cwd, i if label != "base" else 8))
    seen, peak = {}, {"daemon": 0, "agent": 0}
    stop = threading.Event()

    def sample():
        while not stop.is_set():
            ps = procs()
            totals = {"daemon": 0, "agent": 0}
            for pid, kind in ps.items():
                v = cpu_rss(pid)
                if v:
                    seen[pid] = (kind, v[0])
                    totals[kind] += v[1]
            for k in totals:
                peak[k] = max(peak[k], totals[k])
            time.sleep(0.5)

    threading.Thread(target=sample, daemon=True).start()
    t0 = time.time()
    threads = [threading.Thread(target=s.run) for s in sessions]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    stop.set()
    wall = time.time() - t0
    cpu = {"daemon": 0.0, "agent": 0.0}
    for kind, c in seen.values():
        cpu[kind] += c
    print(f"\n== {label}: {n} session(s), {wall:.1f} s wall")
    for s in sessions:
        turns = " | ".join(f"{t['wall_s']}s (api {t['api_s']}s){'' if t['ok'] else ' ERROR'}"
                           for t in s.turns)
        print(f"  {s.name}: {turns}  retries/limits: {s.notes or 'none'}")
        print(f"    final: {(s.final or '').strip()[:300]}")
    print(f"  CPU seconds: tend daemons {cpu['daemon']:.2f}, agents {cpu['agent']:.2f}")
    print(f"  peak RSS: tend daemons {peak['daemon'] / 2**20:.0f} MiB, agents {peak['agent'] / 2**20:.0f} MiB")
    print(f"  cost: ${sum(t['cost'] for s in sessions for t in s.turns[-1:]):.4f}")
    return sessions

phase("base", 1)
# The baseline's session is gone; fleet notices come only from the concurrent phase.
phase("conc", args.sessions)

print("\n== fleet notices (from the ledgers)")
for d in sorted(os.listdir(os.path.join(HOME, "sessions"))):
    path = os.path.join(HOME, "sessions", d, "ledger.jsonl")
    if not os.path.exists(path):
        continue
    for line in open(path):
        e = json.loads(line)
        a = e["body"].get("action") or {}
        if a.get("type") == "fleet":
            print(f"  {d}: [{'told' if e['body']['applied'] else 'would tell'}] {a['text'][:150]}")
