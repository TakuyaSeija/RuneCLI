"""An old release hands a real waiting process to the new build unchanged.
python3 -I -B Cli/tests/RuneWaitingLogUpgrade.py old-rune new-rune
Only copies and an isolated home are used; no installed live binary is run.
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
        print("SKIP live replace/wait fixture requires Linux", flush=True)
        return
    root = Path(tempfile.mkdtemp(prefix="WaitingLogUpgrade.", dir=ROOT / "target/test-tmp"))
    home = root / "Home"
    home.mkdir()
    project = root / "Project"
    project.mkdir()
    installed = root / "rune-installed"
    shutil.copy2(sys.argv[1], installed)
    fixture = root / "provider.json"
    fixture.write_text('{"responses":[{"chunks":[{"text":"Old manual result received."}]}]}')
    (home / "rune.toml").write_text('[terminal]\ninput_wait_delay = "200ms"\ncommand_timeout = "10s"\n[classifier]\nenabled = false\n')
    env = dict(os.environ, RUNE_HOME=str(home), HOME=str(home))
    env.pop("RUNE_PROCESS_TREE", None)
    events = queue.Queue()
    stderr = (root / "host.err").open("w")
    host = subprocess.Popen([str(installed), "-host", "-db", str(home / "rune.db"), "-script", str(fixture)],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr,
                            text=True, bufsize=1, env=env)
    def reader():
        for line in host.stdout:
            events.put(json.loads(line))
    threading.Thread(target=reader, daemon=True).start()

    def send(command):
        host.stdin.write(json.dumps(command) + "\n")
        host.stdin.flush()

    def wait(predicate):
        end = time.monotonic() + 60
        seen = []
        while time.monotonic() < end:
            try:
                event = events.get(timeout=0.2)
            except queue.Empty:
                continue
            seen.append(event)
            if event.get("t") == "permission_request":
                send({"cmd":"permission.reply", "id":900, "req":event["req"], "decision":"allow_once"})
            if predicate(event):
                return event
        raise AssertionError("upgrade fixture timed out: " + json.dumps(seen[-10:]))

    try:
        wait(lambda e: e.get("t") == "ready")
        send({"cmd":"session.open", "id":1, "project":str(project), "fresh":True})
        ack = wait(lambda e: e.get("t") == "ack" and e.get("id") == 1)
        assert ack["ok"], ack
        session = ack["data"].get("session", ack["data"].get("id"))
        marker = project / "ran-once.txt"
        script = "! echo once >> '%s'\necho HANDOVER-SAVED\nprintf 'Upgrade question? '; read answer; echo FINAL:$answer" % marker
        send({"cmd":"shell.submit", "id":2, "session":session, "text":script, "submission":"waiting-log-upgrade-test-token"})
        result = wait(lambda e: e.get("t") == "tool_result")
        assert "Waiting for input" in result["content"], result
        task = "bg_" + result["content"].split("bg_", 1)[1].split()[0]
        pid = result["detail"].get("pid")
        wait(lambda e: e.get("t") == "done")
        folder = next(home.glob("projects/*/sessions/%s" % session))
        path = folder / "tools_result/terminal_use" / (task + ".txt")
        before = path.read_text()
        assert marker.read_text() == "once\n"
        staged = root / "rune-new"
        shutil.copy2(sys.argv[2], staged)
        os.replace(staged, installed)
        wait(lambda e: e.get("t") == "upgrading")
        wait(lambda e: e.get("t") == "ready")
        assert host.poll() is None
        send({"cmd":"tool.invoke", "id":3, "session":session, "name":"manage_process", "args":{"action":"read upgraded waiting process", "process_id":task}})
        result = wait(lambda e: e.get("t") == "tool_result")
        assert "Process: Waiting for input" in result["content"], result
        expected = "HANDOVER-SAVED\nUpgrade question?"
        body = result["content"].split("</tool_notice>", 1)[1].strip()
        assert body == path.read_text() == expected, (before, body, path.read_text())
        assert marker.read_text() == "once\n"
        send({"cmd":"tool.invoke", "id":4, "session":session, "name":"terminal_send_input", "args":{"action":"answer upgraded process", "process_id":task, "text":"yes", "submit":"enter"}})
        wait(lambda e: e.get("t") == "tool_result")
        wait(lambda e: e.get("t") == "done")
        assert path.read_text() == "HANDOVER-SAVED\nUpgrade question? yes\nFINAL:yes\n", path.read_text()
        assert marker.read_text() == "once\n"
        print("PASS two-binary live upgrade: same task continues, old waiting output read from file, no command replay or duplicate log rows", flush=True)
        print("PASS upgraded task input/end: question updates once, final output saved", flush=True)
    finally:
        if host.poll() is None:
            host.terminate()
            try:
                host.wait(timeout=10)
            except subprocess.TimeoutExpired:
                host.kill()
                host.wait(timeout=10)
        stderr.close()
    print("Artifacts:", root, flush=True)


if __name__ == "__main__":
    main()
