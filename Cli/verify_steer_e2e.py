#!/usr/bin/env python3
"""Drive the INSTALLED rune binary over its host protocol and prove that three
mid-turn steers land as three separate user messages in real history.

This talks to the real process, not to an in-process engine: it is the only way
to be sure the artifact on disk behaves the way the tests claim.
"""
import json, os, shutil, subprocess, sys, tempfile, time

RUNE = os.path.expanduser("~/.local/bin/rune")
base = tempfile.mkdtemp(prefix="rune-e2e-")
project = os.path.join(base, "project")
os.makedirs(project, exist_ok=True)
with open(os.path.join(project, "file.txt"), "w") as f:
    f.write("hello\n")

fixture = {
    "responses": [
        {"chunks": [{"tool_call": {"id": "t1", "name": "inspect_file",
                                   "args": {"action": "read the file", "path": "file.txt"}}}]},
        {"chunks": [{"text": "answered all three"}]},
    ]
}
script = os.path.join(base, "fixture.json")
with open(script, "w") as f:
    json.dump(fixture, f)

env = dict(os.environ, RUNE_HOME=base)
proc = subprocess.Popen(
    [RUNE, "-host", "-db", os.path.join(base, "rune.db"), "-script", script],
    env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
    stderr=None, text=True, bufsize=1,
)

def send(cmd):
    proc.stdin.write(json.dumps(cmd) + "\n")
    proc.stdin.flush()

def next_event(deadline=20.0):
    end = time.time() + deadline
    while time.time() < end:
        line = proc.stdout.readline()
        if not line:
            raise SystemExit("FAIL: host closed stdout early")
        return json.loads(line)
    raise SystemExit("FAIL: timed out waiting for an event")

def until(kind, deadline=20.0, auto_permit=True):
    end = time.time() + deadline
    while time.time() < end:
        e = next_event(end - time.time())
        if auto_permit and e.get("t") == "permission_request":
            send({"cmd": "permission.reply", "id": 9000 + e.get("req", "")[:0] or 0,
                  "req": e["req"], "decision": "allow_once"})
            continue
        if e.get("t") == kind:
            return e
    raise SystemExit(f"FAIL: never saw {kind}")

failures = []
def check(label, ok, detail=""):
    print(f"  [{'PASS' if ok else 'FAIL'}] {label}" + (f" -- {detail}" if detail else ""))
    if not ok:
        failures.append(label)

try:
    ready = until("ready")
    print("host up:", ready.get("version"))

    send({"cmd": "session.open", "id": 1, "project": project})
    ack = until("ack")
    check("session.open", ack.get("ok") is True, str(ack.get("error", "")))
    session = ack.get("data", {}).get("session", ack.get("session", 1))
    session = session if isinstance(session, int) else 1

    send({"cmd": "prompt", "id": 2, "session": session, "text": "read file"})
    until("tool_call")
    check("tool call started", True)

    for i, text in enumerate(["satu", "dua", "tiga"]):
        send({"cmd": "prompt.steer", "id": 10 + i, "session": session, "text": text})
    for i in range(3):
        ack = until("ack")
        check(f"steer {i+1} accepted", ack.get("ok") is True, str(ack.get("error", "")))

    until("done")
    check("turn finished", True)

    send({"cmd": "session.history", "id": 50, "session": session})
    ack = until("ack")
    msgs = ack.get("data", {}).get("messages", [])
    if not msgs:
        msgs = ack.get("messages", [])
    spoken = [m["content"] for m in msgs if m.get("role") == "user"]
    turns = [m.get("turn") for m in msgs if m.get("role") == "user"]

    print("  user messages:", spoken)
    print("  user turns   :", turns)
    check("3 steers are 3 separate messages", spoken == ["read file", "satu", "dua", "tiga"], str(spoken))
    joined = [s for s in spoken if "\n" in s]
    check("no joined message", not joined, str(joined))
    check("each steer has its own turn",
          all(b is not None and a is not None and b > a for a, b in zip(turns, turns[1:])),
          str(turns))
    check("one assistant answer after the steers",
          msgs[-1].get("role") == "assistant" and msgs[-1].get("content") == "answered all three",
          str(msgs[-1]))
finally:
    try:
        send({"cmd": "shutdown", "id": 999})
        proc.wait(timeout=10)
    except Exception:
        proc.kill()
    shutil.rmtree(base, ignore_errors=True)

print()
if failures:
    print(f"RESULT: FAIL -> {failures}")
    sys.exit(1)
print("RESULT: ALL CHECKS PASSED")
