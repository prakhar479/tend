#!/usr/bin/env python3
"""Turn real Claude Code captures into sanitized fixtures.

    # One hook payload from a session (run the session with REIN_CAPTURE=1):
    scripts/capture_fixtures.py hooks ~/.local/share/rein/sessions/<id>/hooks.jsonl \
        --event PostToolUse --match "pytest" --tool-use 1 \
        --out fixtures/claude_code/post_tool_use_bash.json

    # A whole transcript (keeps only user/assistant lines and the fields rein reads):
    scripts/capture_fixtures.py transcript ~/.claude/projects/<p>/<s>.jsonl \
        fixtures/transcripts/loop_session.jsonl

Sanitizing: the payload's cwd becomes /home/dev/shop, $HOME becomes /home/dev,
UUIDs become 9b1d6f2e-0000-4000-8000-…, tool-use ids become toolu_NN, and
message ids become msg_NN. The script refuses to write a file that still
contains $HOME or $USER. Read the result before you commit it.
"""

import argparse
import json
import os
import re
import sys

FAKE_CWD = "/home/dev/shop"
FAKE_HOME = "/home/dev"
SESSION_ID = "9b1d6f2e-0000-4000-8000-000000000001"
PROMPT_ID = "9b1d6f2e-0000-4000-8000-0000000000a1"
UUID = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")


def project_dir(path):
    """Claude Code names a project dir after its path: / and . become -."""
    return re.sub(r"[/.]", "-", path)


class Sanitizer:
    def __init__(self, cwd, fixed_uuid=None):
        self.home = os.path.expanduser("~")
        self.cwd = cwd
        self.fixed_uuid = fixed_uuid
        self.ids = {}

    def _seq(self, prefix, fmt, key):
        table = self.ids.setdefault(prefix, {})
        if key not in table:
            table[key] = fmt % (len(table) + 1)
        return table[key]

    def text(self, s):
        if self.cwd:
            s = s.replace(self.cwd, FAKE_CWD)
            s = s.replace(project_dir(self.cwd), project_dir(FAKE_CWD))
        s = s.replace(self.home, FAKE_HOME)
        if self.fixed_uuid:
            s = UUID.sub(self.fixed_uuid, s)
        else:
            s = UUID.sub(lambda m: self._seq("uuid", "9b1d6f2e-0000-4000-8000-%012d", m.group(0)), s)
        s = re.sub(r"toolu_[A-Za-z0-9]{10,}", lambda m: self._seq("toolu", "toolu_%02d", m.group(0)), s)
        s = re.sub(r"msg_[A-Za-z0-9]{10,}", lambda m: self._seq("msg", "msg_%02d", m.group(0)), s)
        return s

    def walk(self, v):
        if isinstance(v, str):
            return self.text(v)
        if isinstance(v, list):
            return [self.walk(x) for x in v]
        if isinstance(v, dict):
            return {k: self.walk(x) for k, x in v.items()}
        return v


def check_clean(text, path):
    user = os.environ.get("USER", "")
    home = os.path.expanduser("~")
    leaks = [x for x in (home, user) if x and len(x) > 2 and x in text]
    if leaks:
        sys.exit(f"refusing to write {path}: still contains {', '.join(leaks)}")


def cmd_hooks(a):
    rows = [json.loads(l) for l in open(a.hooks) if l.strip()]
    hits = [
        r for r in rows
        if r.get("event") == a.event and (not a.match or a.match in json.dumps(r["payload"]))
    ]
    if len(hits) != 1:
        sys.exit(f"{len(hits)} payloads match --event {a.event} --match {a.match!r}; need exactly 1")
    p = hits[0]["payload"]
    if "session_id" in p:
        p["session_id"] = SESSION_ID
    if "prompt_id" in p:
        p["prompt_id"] = PROMPT_ID
    if "tool_use_id" in p:
        p["tool_use_id"] = "toolu_%02d" % a.tool_use
    out = Sanitizer(p.get("cwd", ""), fixed_uuid=SESSION_ID).walk(p)
    text = json.dumps(out, indent=2, ensure_ascii=False) + "\n"
    check_clean(text, a.out)
    open(a.out, "w").write(text)
    print(f"wrote {a.out}")


USAGE = ["input_tokens", "cache_creation_input_tokens", "cache_read_input_tokens", "output_tokens"]


def cmd_transcript(a):
    lines = [json.loads(l) for l in open(a.src) if l.strip()]
    cwd = next((v["cwd"] for v in lines if isinstance(v.get("cwd"), str)), "")
    san = Sanitizer(cwd)
    out = []
    for v in lines:
        if v.get("type") not in ("user", "assistant") or "message" not in v:
            continue
        m = v["message"]
        content = m.get("content")
        if isinstance(content, list):
            content = [c for c in content if c.get("type") in ("text", "tool_use", "tool_result")]
            if not content:
                continue
        msg = {k: m[k] for k in ("id", "role", "model") if k in m}
        msg["content"] = content
        if "usage" in m:
            msg["usage"] = {k: m["usage"].get(k, 0) for k in USAGE}
        keep = {k: v[k] for k in ("type", "uuid", "parentUuid", "timestamp", "sessionId") if k in v}
        keep["message"] = msg
        out.append(json.dumps(san.walk(keep), ensure_ascii=False))
    text = "\n".join(out) + "\n"
    check_clean(text, a.dst)
    open(a.dst, "w").write(text)
    print(f"wrote {a.dst} ({len(out)} lines)")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    h = sub.add_parser("hooks", help="one payload from a session's hooks.jsonl")
    h.add_argument("hooks")
    h.add_argument("--event", required=True)
    h.add_argument("--match", default="", help="text the payload must contain")
    h.add_argument("--tool-use", type=int, default=1, help="N for the toolu_0N id")
    h.add_argument("--out", required=True)
    t = sub.add_parser("transcript", help="a sanitized transcript")
    t.add_argument("src")
    t.add_argument("dst")
    a = ap.parse_args()
    {"hooks": cmd_hooks, "transcript": cmd_transcript}[a.cmd](a)


if __name__ == "__main__":
    main()
