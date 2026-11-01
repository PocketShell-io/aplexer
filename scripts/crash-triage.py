#!/usr/bin/env python3
"""Auto-triage for aplexer crash/OOM warnings (`a warnings`).

Run periodically (see scripts/crash-triage.service/.timer). For every
unacknowledged warning:

* known-benign classes (a worker that was torn down while `exiting`, the
  `/proc` ESRCH race while a session finishes) are acknowledged and logged;
* everything else (OOM, "worker died ... while running", "limits fail
  closed ..." and unknown failures) is a real crash: one Hub intake is filed
  for the batch (project `aplexer`, deterministic key, so a retry never
  duplicates it) and only THEN are those warnings acknowledged.

`a ack` moves the sidecar to warnings/acked/; no session data or logs are
deleted. Nothing is acknowledged if the intake could not be filed, so a
real crash can never be silently dropped.
"""
import hashlib
import json
import os
import re
import subprocess
import sys
import time

BENIGN = [
    re.compile(r"worker died unexpectedly while exiting"),
    re.compile(r"inspect descendants: read /proc/\d+/task.*No such process"),
]
LOG = os.path.expanduser("~/.local/state/aplexer/triage.jsonl")


def run(*cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def ack(ids):
    for sid in ids:
        r = run("a", "ack", sid)
        if r.returncode != 0:
            print(f"ack {sid} failed: {r.stderr.strip()}", file=sys.stderr)


def main():
    r = run("a", "warnings", "--json")
    if r.returncode != 0:
        print(r.stderr, file=sys.stderr)
        return 1
    data = json.loads(r.stdout or "[]")
    warnings = data if isinstance(data, list) else data.get("warnings", [])
    benign, real = [], []
    for w in warnings:
        (benign if any(p.search(w["detail"]) for p in BENIGN) else real).append(w)
    if real:
        ids = sorted(w["session"] for w in real)
        key = "crash-triage-" + hashlib.sha256(",".join(ids).encode()).hexdigest()[:16]
        lines = [
            f"- {w['kind']} {w['engine']} {w['workspace']}:{w['tag']} "
            f"session {w['session']}: {w['detail']}" for w in real
        ]
        text = (
            "aplexer auto-triage: unacknowledged crash/OOM warnings that are not a "
            "known-benign class. Find the cause and fix it; do not just ack.\n"
            + "\n".join(lines)
        )[:8000]
        ac = ("Root cause identified for each session (logs/state kept under "
              "~/.local/state/aplexer), fix or limit landed on main with a "
              "regression test, deployed, and false positives fixed in "
              "src/warnings.rs.")
        c = run("hub", "intake", "create", "--project", "aplexer",
                "--title", f"aplexer crash triage: {len(real)} real crash(es)",
                "--text", text, "--acceptance-criteria", ac, "--key", key)
        if c.returncode != 0 and "exist" not in (c.stderr + c.stdout).lower():
            print("hub intake create failed; leaving warnings unacked: "
                  + c.stderr.strip(), file=sys.stderr)
            real = []
    done = benign + real
    if done:
        os.makedirs(os.path.dirname(LOG), exist_ok=True)
        with open(LOG, "a") as f:
            for w in done:
                f.write(json.dumps({"at": int(time.time()), "class":
                        "benign" if w in benign else "intake", **w}) + "\n")
        ack([w["session"] for w in done])
    return 0


if __name__ == "__main__":
    sys.exit(main())
