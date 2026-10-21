#!/usr/bin/env python3
"""F1.0: how often do Claude Code sessions overlap on this machine?

Reads top-level transcripts under ~/.claude/projects (subagents are part of their
parent session). Sessions whose cwd is under /tmp are bench/conformance runs and are
counted separately.
"""
import json, os, re, subprocess, sys, glob, collections
from datetime import datetime

GAP = 10 * 60  # seconds: a gap longer than this ends an active interval
NEAR = 10 * 60  # same-file window
root = os.path.expanduser(sys.argv[1] if len(sys.argv) > 1 else "~/.claude/projects")

def ts(s):
    return datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()

_repo_cache = {}
def repo_of(cwd):
    """(common git dir, worktree top) or (cwd, cwd) outside git / for a missing dir."""
    if cwd in _repo_cache:
        return _repo_cache[cwd]
    r = (cwd, cwd)
    if os.path.isdir(cwd):
        try:
            out = subprocess.run(["git", "-C", cwd, "rev-parse", "--path-format=absolute",
                                  "--git-common-dir", "--show-toplevel"],
                                 capture_output=True, text=True, timeout=5)
            if out.returncode == 0:
                common, top = out.stdout.split("\n")[:2]
                r = (common, top)
        except Exception:
            pass
    _repo_cache[cwd] = r
    return r

def sig(text):
    line = next((l for l in text.splitlines() if l.strip() and not l.startswith("Exit code")), "")
    line = re.sub(r"/[^\s:'\"]+", "<path>", line)
    line = re.sub(r"\d+", "N", line)
    return line.strip()[:160]

def first_ts(f):
    for line in open(f, errors="replace"):
        m = re.search(r'"timestamp":"([^"]+)"', line)
        if m:
            return m.group(1)
    return ""

# A resumed session copies earlier messages into a new file. Count each message once,
# in the file where it first appeared.
seen_uuids = set()
sessions = []
for f in sorted(glob.glob(os.path.join(root, "*", "*.jsonl")), key=first_ts):
    times, touches, errors, cwd = [], [], [], None
    for line in open(f, errors="replace"):
        try:
            d = json.loads(line)
        except Exception:
            continue
        if "timestamp" not in d or d.get("type") not in ("user", "assistant"):
            continue
        u = d.get("uuid")
        if u in seen_uuids:
            continue
        seen_uuids.add(u)
        t = ts(d["timestamp"])
        times.append(t)
        cwd = cwd or d.get("cwd")
        content = (d.get("message") or {}).get("content")
        if not isinstance(content, list):
            continue
        for c in content:
            if c.get("type") == "tool_use":
                p = (c.get("input") or {}).get("file_path")
                if p:
                    kind = "w" if c.get("name") in ("Edit", "Write", "NotebookEdit") else "r"
                    touches.append((t, p, kind))
            elif c.get("type") == "tool_result" and c.get("is_error"):
                body = c.get("content")
                if isinstance(body, list):
                    body = " ".join(x.get("text", "") for x in body if isinstance(x, dict))
                s = sig(body or "")
                if s and "<tool_use_error>" not in s and "rejected" not in s:
                    errors.append((t, s))
    if not times or not cwd:
        continue
    times.sort()
    ivs, start, last = [], times[0], times[0]
    for t in times[1:]:
        if t - last > GAP:
            ivs.append((start, last)); start = t
        last = t
    ivs.append((start, last))
    common, top = repo_of(cwd)
    rel = [(t, os.path.relpath(p, top) if p.startswith(top + "/") else p, k) for t, p, k in touches]
    sessions.append(dict(id=os.path.basename(f)[:8], cwd=cwd, common=common, top=top,
                         synthetic=cwd.startswith("/tmp"), ivs=ivs, touches=rel, errors=errors,
                         day=datetime.fromtimestamp(times[0]).date()))

def overlap(a, b):
    tot = 0
    for s1, e1 in a:
        for s2, e2 in b:
            tot += max(0, min(e1, e2) - max(s1, s2))
    return tot

for label, group in (("real", [s for s in sessions if not s["synthetic"]]),
                     ("synthetic (/tmp)", [s for s in sessions if s["synthetic"]])):
    group.sort(key=lambda s: s["id"])
    pairs = collections.Counter(); minutes = collections.Counter(); file_hits = []
    days_active = {s["day"] for s in group}; days_concurrent = set()
    for i, a in enumerate(group):
        for b in group[i + 1:]:
            o = overlap(a["ivs"], b["ivs"])
            if o <= 0:
                continue
            setup = ("same checkout" if a["top"] == b["top"]
                     else "worktrees of one repo" if a["common"] == b["common"]
                     else "different repos")
            pairs[setup] += 1; minutes[setup] += o / 60
            days_concurrent.add(a["day"])
            if setup != "different repos":
                for ta, pa, ka in a["touches"]:
                    for tb, pb, kb in b["touches"]:
                        if pa == pb and abs(ta - tb) <= NEAR and "w" in (ka, kb):
                            file_hits.append((setup, a["id"], b["id"], pa)); break
    sig_sessions = collections.defaultdict(set)
    for s in group:
        for _, e in s["errors"]:
            sig_sessions[e].add(s["id"])
    shared = {e: v for e, v in sig_sessions.items() if len(v) > 1}
    print(f"## {label}: {len(group)} sessions, {len(days_active)} active days")
    print(f"days with concurrent sessions: {len(days_concurrent)}")
    for k in ("same checkout", "worktrees of one repo", "different repos"):
        print(f"  {k}: {pairs[k]} overlapping pairs, {minutes[k]:.0f} min overlap")
    uniq = sorted({(h[0], h[1], h[2], h[3]) for h in file_hits})
    print(f"same file, write within 10 min, concurrent pair: {len(uniq)} (file, pair) hits")
    for h in uniq[:10]:
        print("   ", h)
    print(f"error signatures seen in 2+ sessions: {len(shared)} of {len(sig_sessions)}")
    for e, v in sorted(shared.items(), key=lambda kv: -len(kv[1]))[:8]:
        print(f"    {len(v)}x  {e}")
    print()
