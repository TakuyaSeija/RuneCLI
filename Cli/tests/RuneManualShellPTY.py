"""Manual shell and clickable references in a real isolated terminal, no live account.
python3 -I -B Cli/tests/RuneManualShellPTY.py [path/to/rune]
"""
import base64
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import sys
import time
import threading

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("live", Path(__file__).with_name("RuneLiveProviderPTY.py"))
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)
binary = live.ROOT / ("rune-manual-test.exe" if live.WINDOWS else "rune-manual-test")
shutil.copy2(live.BIN, binary)
live.BIN = str(binary)
os.environ["HOME"] = str(live.HOME)
os.environ.pop("RUNE_PROCESS_TREE", None)


def paste(window, text):
    window.send(b"\x1b[200~" + text.encode() + b"\x1b[201~")


def locate(window, text):
    row = live.row_with(window, text)
    assert row is not None, (text, window.screen.text())
    col = live.column_of(window, row, text)
    assert col is not None
    return row + 1, col + 1


def click(window, text):
    live.click(window, *locate(window, text))


def command_row(window, text):
    for row, line in enumerate(window.screen.text().splitlines()):
        if text in line and re.search(r"│ (?:\$|PS>|>) ", line):
            return row
    return None


def command_box(window, text):
    lines = window.screen.text().splitlines()
    row = command_row(window, text)
    if row is None:
        return []
    bottom = next((i for i in range(row + 1, len(lines)) if "╰" in lines[i]), len(lines) - 1)
    return lines[row:bottom + 1]


def main():
    provider = live.Provider()
    (live.HOME / "rune.toml").write_text(
        '[ui]\nmouse = true\nsidebar = false\n'
        '[host]\nidle_lifetime = "1s"\n'
        '[classifier]\nenabled = false\n'
        '[providers]\ndefault = "fake"\n'
        '[[providers.list]]\nname = "fake"\n'
        f'base_url = "http://127.0.0.1:{provider.port}/v1"\n'
        'auth = { header = "Authorization", prefix = "Bearer " }\n'
        '[[providers.list.models]]\nid = "fake-model"\nthinking = false\ncontext = 65536\n'
    )
    opener_log = live.ROOT / "browser-open.json"
    if live.LINUX:
        opener_dir = live.ROOT / "desktop-opener"
        opener_dir.mkdir()
        opener = opener_dir / "xdg-open"
        opener.write_text("#!/usr/bin/env python3\nimport json,sys\nfrom pathlib import Path\nPath(" + repr(str(opener_log)) + ").write_text(json.dumps(sys.argv[1:]))\n")
        opener.chmod(0o755)
        os.environ["PATH"] = str(opener_dir) + os.pathsep + os.environ.get("PATH", "")
    window = live.Window(["-project", str(live.PROJECT)], rows=38, cols=120)
    try:
        live.until(lambda: window.shows("fake-model"), "initial paint", window)
        marker = live.PROJECT / "manual-marker.txt"
        command = f"! echo MANUAL-PTY-OUTPUT\necho once >> '{marker}'"
        paste(window, command)
        live.until(lambda: window.shows("Shell · full access once"), "manual label before Enter", window)
        assert not marker.exists() and not provider.main_calls(), "paste ran without Enter"
        provider.queue(live.text("Observed manual result. Read [official docs](https://example.com/a?x=1&y=2) and [source file](src/main.rs)."))
        window.send(b"\r")
        live.until(lambda: window.shows("Observed manual result."), "AI responds after manual completion", window)
        live.settled(window)
        assert marker.read_text().strip() == "once"
        calls = provider.main_calls()
        assert len(calls) == 1
        context = json.dumps(calls[0]["body"], ensure_ascii=False)
        assert "MANUAL-PTY-OUTPUT" in context and "manual_shell" in context
        assert window.shows("Bash") or window.shows("PowerShell") or window.shows("Cmd") or window.shows("sh")
        row = command_row(window, "echo MANUAL-PTY-OUTPUT")
        assert row is not None
        box = command_box(window, "echo MANUAL-PTY-OUTPUT")
        assert "echo MANUAL-PTY-OUTPUT ↵ echo once" in box[0], window.screen.text()
        assert "├" in box[1], "closed command preview exceeds one row"
        assert "…" in box[0], "a hidden command suffix must be explicit"
        assert any("lines cut" in line for line in box), window.screen.text()
        click(window, "lines cut")
        live.until(lambda: len(command_box(window, "echo MANUAL-PTY-OUTPUT")) > 1 and "echo once" in command_box(window, "echo MANUAL-PTY-OUTPUT")[1], "Open All restores multiline command", window)
        row = command_row(window, "echo MANUAL-PTY-OUTPUT")
        assert "echo once" in window.screen.text().splitlines()[row + 1], window.screen.text()
        assert "echo once" not in window.screen.text().splitlines()[row], "multiline preview was flattened"
        live.click(window, row + 1, live.column_of(window, row, "echo MANUAL-PTY-OUTPUT") + 1)
        live.until(lambda: len(command_box(window, "echo MANUAL-PTY-OUTPUT")) > 1 and "├" in command_box(window, "echo MANUAL-PTY-OUTPUT")[1], "command folds back to one row", window)
        row = live.row_with(window, "Observed manual result.")
        col = live.column_of(window, row, "Observed manual result.")
        assert window.screen.ink(row, col) == (255, 255, 255), window.screen.ink(row, col)
        print("PASS manual multiline: paste waits for Enter, full-once command settles once, AI sees output, response is white", flush=True)

        click(window, "official docs")
        live.until(lambda: window.shows("REFERENCE") and window.shows("https://example.com/a?x=1&y=2"), "link review panel", window)
        assert window.shows("Open in browser")
        if live.LINUX:
            assert not opener_log.exists(), "label click opened the browser before review"
            click(window, "Open in browser")
            live.until(lambda: opener_log.exists(), "explicit desktop opening", window)
            assert json.loads(opener_log.read_text()) == ["https://example.com/a?x=1&y=2"]
        start = len(window.raw)
        click(window, "Copy complete destination")
        live.until(lambda: re.search(rb"\x1b\]52;c;([A-Za-z0-9+/=]+)\x07", bytes(window.raw[start:])), "destination clipboard", window)
        encoded = re.search(rb"\x1b\]52;c;([A-Za-z0-9+/=]+)\x07", bytes(window.raw[start:])).group(1)
        assert base64.b64decode(encoded).decode() == "https://example.com/a?x=1&y=2"
        live.press_escape(window)
        live.until(lambda: window.shows("source file") and not window.shows("REFERENCE"), "return to transcript after URL review", window)
        click(window, "source file")
        live.until(lambda: window.shows("REFERENCE") and window.shows("src/main.rs"), "file path review", window)
        assert not window.shows("Open in browser") and window.shows("copy only")
        live.press_escape(window)
        live.until(lambda: not window.shows("REFERENCE"), "return from path review", window)
        print("PASS links: review first, explicit web Open passes one exact URL to isolated desktop stub, Copy exact, file copy-only", flush=True)

        provider.queue(live.text("Ordinary mixed prompt received."))
        before = len(live.transcript())
        paste(window, "hello fix this ! echo MUST-NOT-RUN")
        window.send(b"\r")
        live.until(lambda: window.shows("Ordinary mixed prompt received."), "mixed prompt response", window)
        live.settled(window)
        new = live.transcript()[before:]
        assert not any(row.get("context", {}).get("kind") == "manual_shell" for row in new)
        assert any(row.get("content") == "hello fix this ! echo MUST-NOT-RUN" for row in new)
        print("PASS mixed prose ! remains a normal AI prompt, not a directly executed command", flush=True)

        window.close()
        window = live.Window(["-project", str(live.PROJECT), "-resume", "1"], rows=38, cols=120)
        live.until(lambda: window.shows("MANUAL-PTY-OUTPUT"), "recorded manual replay", window)
        assert marker.read_text().strip() == "once"
        print("PASS resume: recorded manual command/output reappear without executing the script again", flush=True)

        release_arguments = threading.Event()
        # The preview names the shell that will run the command: bash or sh
        # (`$`) on Linux and macOS, PowerShell (`PS>`) as Windows' automatic shell.
        prompt = "PS>" if live.WINDOWS else "$"
        command = "echo STREAM-PTY-OUTPUT\necho HIDDEN-SECOND"
        args = json.dumps({"action": "Streaming terminal preview", "command": command})
        split = args.index("HIDDEN-SECOND")

        def streaming_terminal(handler, streaming):
            assert streaming
            live.open_stream(handler)
            live.say(handler, live.delta(role="assistant", tool_calls=[{
                "index": 0, "id": "stream-terminal", "type": "function",
                "function": {"name": "terminal_use", "arguments": args[:split]},
            }]))
            if not release_arguments.wait(30):
                return
            live.say(handler, live.delta(tool_calls=[{
                "index": 0, "function": {"arguments": args[split:]},
            }]))
            live.say(handler, live.finish("tool_calls"))
            live.close_stream(handler)

        provider.queue(streaming_terminal, live.text("Streaming preview verified."))
        paste(window, "Check the streaming terminal preview.")
        window.send(b"\r")
        try:
            live.until(lambda: window.shows(f"{prompt} echo STREAM-PTY-OUTPUT"), "correct prompt before arguments finish", window)
            screen = window.screen.text()
            row = command_row(window, "echo STREAM-PTY-OUTPUT")
            assert " ↵ echo " in screen.splitlines()[row], screen
            assert "lines cut" not in "\n".join(command_box(window, "echo STREAM-PTY-OUTPUT")), screen
            others = screen.replace(f"{prompt} echo STREAM-PTY-OUTPUT", "")
            assert "$ echo STREAM-PTY-OUTPUT" not in others and "> echo STREAM-PTY-OUTPUT" not in others, screen
            assert "HIDDEN-SECOND" not in screen, screen
        finally:
            release_arguments.set()
        live.until(lambda: window.shows("Allow once") and window.shows("HIDDEN-SECOND"), "permission still asks before execution", window)
        window.send(b"\r")
        live.until(lambda: window.shows("Streaming preview verified."), "streamed command result", window)
        live.settled(window)
        screen = window.screen.text()
        row = command_row(window, "echo STREAM-PTY-OUTPUT")
        assert "├" in screen.splitlines()[row + 1], screen
        assert f"{prompt} echo STREAM-PTY-OUTPUT ↵ echo HIDDEN-SECOND" in screen.splitlines()[row], screen
        assert "lines cut" not in "\n".join(command_box(window, "echo STREAM-PTY-OUTPUT")), screen
        line = screen.splitlines()[row]
        first = live.column_of(window, row, "echo STREAM-PTY-OUTPUT") + 1
        last = live.column_of(window, row, "HIDDEN-SECOND") + len("HIDDEN-SECOND")
        start = len(window.raw)
        window.send(f"\x1b[<0;{first};{row + 1}M\x1b[<32;{last};{row + 1}M\x1b[<0;{last};{row + 1}m".encode())
        live.until(lambda: re.search(rb"\x1b\]52;c;([A-Za-z0-9+/=]+)\x07", bytes(window.raw[start:])), "projected command clipboard", window)
        encoded = re.search(rb"\x1b\]52;c;([A-Za-z0-9+/=]+)\x07", bytes(window.raw[start:])).group(1)
        assert base64.b64decode(encoded).decode() == command
        print(f"PASS streaming: the shell's {prompt} from the first argument delta, newline-marked one-row preview, original multiline clipboard, gate unchanged", flush=True)
    finally:
        window.close()
        provider.server.shutdown()
    print("Artifacts:", live.ROOT, flush=True)


if __name__ == "__main__":
    main()
