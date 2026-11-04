"""Saved waiting output and the task's notification agree in a real isolated terminal.
python3 -I -B Cli/tests/RuneWaitingLogPTY.py [path/to/rune]
"""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("live", Path(__file__).with_name("RuneLiveProviderPTY.py"))
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)
os.environ.pop("RUNE_PROCESS_TREE", None)
os.environ["HOME"] = str(live.HOME)
binary = live.ROOT / ("waiting-log.exe" if live.WINDOWS else "waiting-log")
shutil.copy2(live.BIN, binary)
live.BIN = str(binary)


def notifications():
    return [
        row["content"]
        for row in live.transcript()
        if row.get("context", {}).get("kind") == "notification"
    ]


def manual_shell_output():
    """What the model was told by the person's own call: its settled result."""
    for row in live.transcript():
        context = row.get("context") or {}
        if context.get("kind") != "manual_shell":
            continue
        recorded = json.loads(context["note"])
        if recorded.get("args", {}).get("command", "").startswith("echo SAVED-WAIT"):
            return recorded.get("content")
    return None


def task_and_log():
    folder = live.session_dir(1)
    if folder is None:
        return None
    for row in live.transcript():
        if row.get("context", {}).get("kind") == "manual_shell":
            recorded = json.loads(row["context"]["note"])
            if recorded.get("args", {}).get("command", "").startswith("echo SAVED-WAIT"):
                content = recorded["content"]
                task = content.split("bg_", 1)[1].split()[0]
                return "bg_" + task, next(folder.glob("tools_result/terminal_use/bg_*.txt"))
    return None


def main():
    if not live.LINUX:
        print("SKIP typed-input wait detection requires Linux for this fixture", flush=True)
        return
    provider = live.Provider()
    (live.HOME / "rune.toml").write_text(
        '[ui]\nmouse = true\nsidebar = false\n'
        '[host]\nidle_lifetime = "1s"\n'
        '[terminal]\ninput_wait_delay = "200ms"\ncommand_timeout = "15s"\n'
        '[classifier]\nenabled = false\n'
        '[providers]\ndefault = "fake"\n'
        '[[providers.list]]\nname = "fake"\n'
        f'base_url = "http://127.0.0.1:{provider.port}/v1"\n'
        'auth = { header = "Authorization", prefix = "Bearer " }\n'
        '[[providers.list.models]]\nid = "fake-model"\nthinking = false\ncontext = 65536\n'
    )
    window = live.Window(["-project", str(live.PROJECT)], rows=60, cols=160)
    try:
        live.until(lambda: window.shows("fake-model"), "initial paint", window)
        # Nothing is asked for: the wait's own notification carries the saved
        # output, so the model answers straight from what it was handed.
        provider.queue(live.text("SAVED LOG VERIFIED."))
        script = "! echo SAVED-WAIT\nprintf 'Question? '; read answer; echo FINAL:$answer"
        window.send(b"\x1b[200~" + script.encode() + b"\x1b[201~")
        window.send(b"\r")
        live.until(lambda: window.shows("SAVED LOG VERIFIED."), "manual input wait", window)
        live.settled(window)
        task, path = task_and_log()
        # A person's own call tells the model through its settled result, so the
        # saved output and what the model reads are the same words.
        told = manual_shell_output()
        assert told is not None, "the model was never told"
        notice, text = told.split("</tool_notice>", 1)
        assert "Waiting for input" in notice, told
        expected = "SAVED-WAIT\nQuestion?"
        assert text.strip() == path.read_text() == expected, (text, path.read_text())
        screen = window.screen.text()
        assert "SAVED-WAIT" in screen and "Question?" in screen, screen
        print("PASS real PTY: terminal handoff, file and the notification carry the same waiting output once", flush=True)

        provider.queue(
            live.tool("process_control", {"action":"Answer the fixture", "process_id":task, "do":"type", "text":"yes", "submit":"enter"}),
            live.text("ANSWER SENT."))
        window.type("answer the fixture with yes")
        window.send(b"\r")
        live.until(lambda: window.shows("Allow once") and window.shows("Answer the fixture"), "input permission", window)
        window.send(b"\r")
        live.until(lambda: window.shows("ANSWER SENT."), "answer sent", window)
        live.until(lambda: path.read_text() == "SAVED-WAIT\nQuestion? yes\nFINAL:yes\n", "final log contents", window)
        assert path.read_text() == "SAVED-WAIT\nQuestion? yes\nFINAL:yes\n", path.read_text()
        # The end wakes it once. A task that began with a handoff carries its
        # log from the start in the first wake by contract (the result that
        # handed it over may be compacted away), and what the terminal echoes
        # onto the answered row replaces that row rather than adding one.
        told = notifications()
        assert len(told) == 1, told
        line, body = told[0].split("</system_notification>", 1)
        assert "Completed" in line, line
        assert "FINAL:yes" in body, body
        assert body.count("Durable") == 0
        print("PASS final disk log: answered question updates in place, and the end wake carries it", flush=True)
    finally:
        window.close()
        provider.server.shutdown()
    print("Artifacts:", live.ROOT, flush=True)


if __name__ == "__main__":
    main()
