"""Copied-binary regression: native/display thinking is never token admission.
Run from an isolated checkout outside the live workspace:
  python3 -I -B Cli/tests/RuneContextTruth.py /path/to/rune
No live HOME, API, router, or original-session tool execution is used.
"""
import json
import os
from pathlib import Path
import queue
import shutil
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[2]


def host(binary, folder):
    home = folder / "H"
    env = dict(os.environ, HOME=str(home), RUNE_HOME=str(home),
               TMPDIR=str(folder / "T"), XDG_CONFIG_HOME=str(home / "c"),
               XDG_CACHE_HOME=str(home / "cache"), XDG_DATA_HOME=str(home / "d"))
    env.pop("RUNE_PROCESS_TREE", None)
    (folder / "T").mkdir(exist_ok=True)
    events = queue.Queue()
    error = (folder / "host.err").open("a")
    process = subprocess.Popen([str(binary), "-host", "-db", str(home / "rune.db"),
                                "-script", str(folder / "provider.json")],
                               cwd=folder, env=env, stdin=subprocess.PIPE,
                               stdout=subprocess.PIPE, stderr=error, text=True, bufsize=1)

    def read():
        for line in process.stdout:
            events.put(json.loads(line))
    threading.Thread(target=read, daemon=True).start()

    def send(value):
        process.stdin.write(json.dumps(value) + "\n")
        process.stdin.flush()

    def wait(predicate):
        deadline = time.monotonic() + 45
        seen = []
        while time.monotonic() < deadline:
            event = events.get(timeout=max(.1, deadline - time.monotonic()))
            seen.append(event)
            if predicate(event):
                return event, seen
        raise AssertionError("fixture timed out: " + repr(seen[-3:]))

    def close():
        if process.poll() is None:
            send({"cmd": "shutdown", "id": 999})
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.terminate()
                process.wait(timeout=10)
        error.close()

    wait(lambda e: e.get("t") == "ready")
    return send, wait, close


def run_case(root, binary, name, display, native):
    folder = root / name
    folder.mkdir()
    home = folder / "H"
    home.mkdir()
    project = folder / "P"
    project.mkdir()
    (home / "rune.toml").write_text('[classifier]\nenabled=false\n[compact]\nfallback_context_tokens=10000\n')
    (folder / "provider.json").write_text(json.dumps({"responses": [{"chunks": [{"text": "OFFLINE_REACHED"}]}]}))
    send, wait, close = host(binary, folder)
    try:
        send({"cmd": "session.open", "id": 1, "project": str(project), "fresh": True})
        ack, _ = wait(lambda e: e.get("t") == "ack" and e.get("id") == 1)
        assert ack["ok"], ack
        sid = ack["data"].get("session", ack["data"].get("id"))
        send({"cmd": "tool.list", "id": 2})
        tools, _ = wait(lambda e: e.get("t") == "ack" and e.get("id") == 2)
        description = next(t["description"] for t in tools["data"]["tools"] if t["name"] == "terminal_use")
        for absent in ["end your turn", "polling", "model cost"]:
            assert absent not in description, absent
        send({"cmd": "session.context", "id": 20, "session": sid})
        context, _ = wait(lambda e: e.get("t") == "ack" and e.get("id") == 20)
        system = context["data"]["system"]
        for expected in ["polling/wait loop", "wastes model cost, and can become another background process", "loops that perform the task's actual work are different"]:
            assert expected in system, expected
        assert system.count("Every call starts with action") == 1
        # The parameter carries its own one-line description again (owner,
        # 2026-10-31): models that read only the JSON schema filled `action`
        # with a generic word ("run", "read") when it had none. The agent rule
        # stays stated once, in the system prompt above.
        described = [tool["parameters"]["properties"]["action"].get("description")
                     for tool in context["data"]["tools"]]
        assert all(d == "One short sentence: what you are about to do, and to what." for d in described), described
    finally:
        close()
    session = next(home.glob("projects/*/sessions/" + str(sid)))
    settings = {"provider": "offline", "model": "fixture", "max_output": 1024}
    meta = json.loads((session / "meta.json").read_text())
    meta.update(title="Context truth fixture", model="fixture", settings=json.dumps(settings))
    (session / "meta.json").write_text(json.dumps(meta))
    thought = "Original generated reasoning text. " * 20000
    user = {"id": 1, "turn": 1, "role": "user", "content": "Original instruction.",
            "media": [], "created_at": 1700000000001}
    assistant = {"id": 2, "turn": 1, "role": "assistant", "content": "Earlier answer.",
                 "media": [], "created_at": 1700000000002,
                 "reasoning": thought if display else None,
                 "continuation": {"provider": "offline", "model": "fixture", "data": {"reasoning_content": thought}} if native else None}
    (session / "session.jsonl").write_text(json.dumps(user) + "\n" + json.dumps(assistant) + "\n")
    db = sqlite3.connect(home / "rune.db")
    db.execute("UPDATE sessions SET title=?,settings=? WHERE id=?", (meta["title"], json.dumps(settings), sid))
    db.execute("INSERT OR REPLACE INTO counters(name,value) VALUES('message',2)")
    db.commit()
    db.close()
    send, wait, close = host(binary, folder)
    try:
        send({"cmd": "session.resume", "id": 3, "session": sid, "project": str(project)})
        ack, _ = wait(lambda e: e.get("t") == "ack" and e.get("id") == 3)
        assert ack["ok"], ack
        send({"cmd": "prompt", "id": 4, "session": sid, "text": "Continue"})
        _, seen = wait(lambda e: e.get("t") == "done")
        (folder / "events.json").write_text(json.dumps(seen, indent=2))
    finally:
        close()
    rows = [json.loads(line) for line in (session / "session.jsonl").read_text().splitlines()]
    assert not any(row["role"] == "error" for row in rows)
    assert any(row["role"] == "assistant" and row["content"] == "OFFLINE_REACHED" for row in rows)
    assert not any(e.get("t") == "compacted" for e in seen), "text length is not measured usage"
    print("PASS", name, "provider reached, no fabricated context/compaction; background guidance present", flush=True)


def main():
    root = Path(tempfile.mkdtemp(prefix="CT.", dir=ROOT / "target/test-tmp"))
    binary = root / "rune"
    shutil.copy2(sys.argv[1], binary)
    for name, display, native in [("both", True, True), ("native", False, True),
                                   ("display", True, False), ("neither", False, False)]:
        run_case(root, binary, name, display, native)
    print("Artifacts:", root, flush=True)


if __name__ == "__main__":
    main()
