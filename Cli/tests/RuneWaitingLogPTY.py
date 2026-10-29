"""Saved waiting output and manage_process agree in a real isolated terminal.
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
        def read_saved(h, stream):
            task, path = task_and_log()
            assert path.read_text() == "SAVED-WAIT\nQuestion?"
            live.tool("manage_process", {"action":"Read the saved waiting log", "process_id":task})(h, stream)
        provider.queue(read_saved, live.text("SAVED LOG VERIFIED."))
        script = "! echo SAVED-WAIT\nprintf 'Question? '; read answer; echo FINAL:$answer"
        window.send(b"\x1b[200~" + script.encode() + b"\x1b[201~")
        window.send(b"\r")
        live.until(lambda: window.shows("SAVED LOG VERIFIED."), "manual input wait and report", window)
        live.settled(window)
        task, path = task_and_log()
        reports = [row["content"] for row in live.transcript() if row.get("tool_name") == "manage_process"]
        assert len(reports) == 1, reports
        report = reports[0]
        assert "Process: Waiting for input" in report
        body = report.split("</tool_notice>", 1)[1].strip()
        assert body == path.read_text() == "SAVED-WAIT\nQuestion?", (body, path.read_text())
        screen = window.screen.text()
        at = screen.index("Read the saved waiting log")
        assert "SAVED-WAIT" in screen[at:] and "Question?" in screen[at:], screen
        print("PASS real PTY: terminal handoff, file and manage_process contain the same waiting output once", flush=True)

        live.ask(window, provider, "verify again without new output",
                 live.tool("manage_process", {"action":"Check unchanged waiting process", "process_id":task}),
                 live.text("NO NEW OUTPUT VERIFIED."), expect="NO NEW OUTPUT VERIFIED.")
        live.settled(window)
        reports = [row["content"] for row in live.transcript() if row.get("tool_name") == "manage_process"]
        assert not reports[-1].split("</tool_notice>", 1)[1].strip(), reports[-1]
        print("PASS repeated report: status remains waiting, no duplicate body or additional execution", flush=True)

        provider.queue(
            live.tool("terminal_send_input", {"action":"Answer the fixture", "process_id":task, "text":"yes", "submit":"enter"}),
            live.text("ANSWER SENT."))
        window.type("answer the fixture with yes")
        window.send(b"\r")
        live.until(lambda: window.shows("Allow once") and window.shows("Answer the fixture"), "input permission", window)
        window.send(b"\r")
        live.until(lambda: window.shows("ANSWER SENT."), "answer sent", window)
        live.until(lambda: path.read_text() == "SAVED-WAIT\nQuestion? yes\nFINAL:yes\n", "final log contents", window)
        assert path.read_text() == "SAVED-WAIT\nQuestion? yes\nFINAL:yes\n", path.read_text()
        print("PASS final disk log: answered question updates in place, no duplicate saved rows", flush=True)
    finally:
        window.close()
        provider.server.shutdown()
    print("Artifacts:", live.ROOT, flush=True)


if __name__ == "__main__":
    main()
