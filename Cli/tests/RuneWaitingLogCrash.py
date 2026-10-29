"""Kill only an isolated fixture host, restart, and recover its waiting log.
python3 -I -B Cli/tests/RuneWaitingLogCrash.py [path/to/rune]
"""
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(Path(__file__).resolve().parent))
import RuneTerminal


def main():
    if not RuneTerminal.LINUX:
        print("SKIP this fixture requires Linux typed-input wait detection", flush=True)
        return
    root = Path(tempfile.mkdtemp(prefix="WaitingLogCrash.", dir=ROOT / "target/test-tmp"))
    home = root / "Home"
    home.mkdir()
    project = root / "Project"
    project.mkdir()
    binary = root / "rune-fixture"
    shutil.copy2(sys.argv[1] if len(sys.argv) > 1 else ROOT / "target/debug/rune", binary)
    fixture = root / "provider.json"
    fixture.write_text('{"responses":[]}')
    (home / "rune.toml").write_text('[terminal]\ninput_wait_delay = "200ms"\ncommand_timeout = "10s"\n[classifier]\nenabled = false\n')
    env = dict(os.environ, RUNE_HOME=str(home), HOME=str(home))
    env.pop("RUNE_PROCESS_TREE", None)
    hosts = []
    logs = []

    def start():
        stderr = (root / ("host-%d.err" % len(hosts))).open("w")
        logs.append(stderr)
        host = subprocess.Popen([str(binary), "-host", "-db", str(home / "rune.db"), "-script", str(fixture)],
                                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr,
                                text=True, bufsize=1, env=env)
        events = queue.Queue()
        def reader():
            for line in host.stdout:
                events.put(json.loads(line))
        threading.Thread(target=reader, daemon=True).start()
        hosts.append(host)
        return host, events

    def send(host, command):
        host.stdin.write(json.dumps(command) + "\n")
        host.stdin.flush()

    def wait(host, events, predicate):
        end = time.monotonic() + 30
        seen = []
        while time.monotonic() < end:
            try:
                event = events.get(timeout=0.2)
            except queue.Empty:
                continue
            seen.append(event)
            if event.get("t") == "permission_request":
                send(host, {"cmd":"permission.reply", "id":900, "req":event["req"], "decision":"allow_once"})
            if predicate(event):
                return event
        raise AssertionError("host fixture timed out: " + json.dumps(seen[-10:]))

    try:
        host, events = start()
        wait(host, events, lambda e: e.get("t") == "ready")
        send(host, {"cmd":"session.open", "id":1, "project":str(project), "fresh":True})
        ack = wait(host, events, lambda e: e.get("t") == "ack" and e.get("id") == 1)
        assert ack["ok"], ack
        session = ack["data"].get("session", ack["data"].get("id"))
        send(host, {"cmd":"tool.invoke", "id":2, "session":session, "name":"terminal_use", "args":{"action":"wait in fixture", "command":"echo CRASH-SAVED; printf 'Durable question? '; read answer; echo end"}})
        result = wait(host, events, lambda e: e.get("t") == "tool_result")
        assert "Waiting for input" in result["content"], result
        folder = next(home.glob("projects/*/sessions/%s" % session))
        log = next(folder.glob("tools_result/terminal_use/bg_*.txt"))
        task = log.stem
        expected = "CRASH-SAVED\nDurable question?"
        assert log.read_text() == expected
        host.kill()
        host.wait(timeout=10)
        assert log.read_text() == expected
        print("PASS real crash: fixture host killed while waiting, output remains on disk", flush=True)

        host, events = start()
        wait(host, events, lambda e: e.get("t") == "ready")
        send(host, {"cmd":"session.resume", "id":3, "session":session})
        ack = wait(host, events, lambda e: e.get("t") == "ack" and e.get("id") == 3)
        assert ack["ok"], ack
        wait(host, events, lambda e: e.get("t") == "done")
        send(host, {"cmd":"tool.invoke", "id":4, "session":session, "name":"manage_process", "args":{"action":"recover fixture log", "process_id":task}})
        result = wait(host, events, lambda e: e.get("t") == "tool_result")
        assert "Process: Lost" in result["content"], result
        body = result["content"].split("</tool_notice>", 1)[1].strip()
        assert body == expected == log.read_text(), (body, log.read_text())
        assert body.count("Durable question?") == 1
        print("PASS restart: manage_process reads Lost task's exact saved output without the old screen or conversation", flush=True)
    finally:
        for host in reversed(hosts):
            if host.poll() is None:
                host.terminate()
                try:
                    host.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    host.kill()
                    host.wait(timeout=10)
        for log in logs:
            log.close()
    print("Artifacts:", root, flush=True)


if __name__ == "__main__":
    main()
