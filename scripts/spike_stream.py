#!/usr/bin/env python3
"""F-S1: drive one long-lived `tend claude -p` stream-json session.

Usage: TEND_HOME=<dir> TEND_CAPTURE=1 scripts/spike_stream.py <tend binary> <workdir>
(results in docs/TASKS.md, Spike results, F-S1). <workdir> holds out.jsonl and err.txt
and its `repo/` subdirectory is the agent's working directory.

Turns: 1) a Bash call, 2) another after an idle gap, 3) a long call interrupted
mid-run, 4) a turn after the interrupt. Then close stdin and time the exit.
Every stdout line is logged with its time and type to out.jsonl.
"""
import json, os, subprocess, sys, threading, time, uuid, queue

TEND = sys.argv[1]
S = os.path.abspath(sys.argv[2])
os.makedirs(os.path.join(S, "repo"), exist_ok=True)
SID = str(uuid.uuid4())
t0 = time.time()
log = open(os.path.join(S, "out.jsonl"), "w")
events = queue.Queue()

cmd = [TEND, "claude", "--tend-mode", "look", "-p",
       "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
       "--model", "haiku", "--permission-mode", "bypassPermissions", "--session-id", SID]
p = subprocess.Popen(cmd, cwd=os.path.join(S, "repo"), stdin=subprocess.PIPE,
                     stdout=subprocess.PIPE, stderr=open(os.path.join(S, "err.txt"), "w"),
                     text=True, bufsize=1)

def reader():
    for line in p.stdout:
        t = round(time.time() - t0, 2)
        try:
            v = json.loads(line)
        except Exception:
            v = {"type": "unparsed", "text": line[:200]}
        log.write(json.dumps({"t": t, "msg": v}) + "\n"); log.flush()
        events.put((t, v))
    events.put((round(time.time() - t0, 2), {"type": "eof"}))

threading.Thread(target=reader, daemon=True).start()

def note(s):
    t = round(time.time() - t0, 2)
    print(f"[{t:7.2f}] {s}", flush=True)
    log.write(json.dumps({"t": t, "note": s}) + "\n"); log.flush()

def send(obj):
    note("send " + json.dumps(obj)[:120])
    p.stdin.write(json.dumps(obj) + "\n"); p.stdin.flush()

def user(text):
    send({"type": "user", "message": {"role": "user", "content": text}})

def wait_for(pred, timeout, label):
    end = time.time() + timeout
    while time.time() < end:
        try:
            t, v = events.get(timeout=max(0.05, end - time.time()))
        except queue.Empty:
            break
        kind = v.get("type"), v.get("subtype")
        if kind[0] in ("result", "control_response", "eof", "system"):
            note(f"got {kind} {json.dumps(v)[:200]}")
        if pred(v):
            return v
    note(f"TIMEOUT waiting for {label}")
    return None

is_result = lambda v: v.get("type") in ("result", "eof")

note(f"session id {SID}")
user("Run the Bash command `echo one`, then reply with exactly: ok1")
wait_for(is_result, 120, "result 1")
note("idle 8 s")
time.sleep(8)
user("Run the Bash command `echo two`, then reply with exactly: ok2")
wait_for(is_result, 120, "result 2")

user("Run this Bash command and wait for it: python3 -c 'import time; time.sleep(40); print(\"slept\")'  Then reply with exactly: ok3")
# Wait until the tool call has started (an assistant message with tool_use).
wait_for(lambda v: v.get("type") == "assistant" and any(
    c.get("type") == "tool_use" for c in v.get("message", {}).get("content", [])), 120, "tool_use 3")
time.sleep(4)
send({"type": "control_request", "request_id": "int-1", "request": {"subtype": "interrupt"}})
wait_for(is_result, 60, "result after interrupt")

user("Reply with exactly: ok4")
wait_for(is_result, 120, "result 4")

note("closing stdin")
p.stdin.close()
try:
    rc = p.wait(timeout=60)
    note(f"exit {rc}")
except subprocess.TimeoutExpired:
    note("did not exit within 60 s; killing")
    p.kill()
print(SID)
