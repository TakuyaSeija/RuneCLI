"""Mouse selection/copy end-to-end: isolated copied build and local provider.

python3 -I -B Cli/tests/RuneSelectionPTY.py [path/to/rune]
Clipboard writes are observed from OSC 52, never read from the owner's clipboard.
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

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("live", Path(__file__).with_name("RuneLiveProviderPTY.py"))
live = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(live)
BINARY = live.ROOT / ("rune-copy-test.exe" if live.WINDOWS else "rune-copy-test")
shutil.copy2(live.BIN, BINARY)
live.BIN = str(BINARY)
os.environ["HOME"] = str(live.HOME)
COUNT = 0


def ok(description):
    global COUNT
    COUNT += 1
    print(f"PASS {COUNT:02} {description}", flush=True)


def paste(window, text):
    window.send(b"\x1b[200~" + text.encode() + b"\x1b[201~")


def locate(window, text, last=False):
    rows = [y for y, row in enumerate(window.screen.text().split("\n")) if text in row]
    assert rows, (text, window.screen.text())
    row = rows[-1] if last else rows[0]
    col = live.column_of(window, row, text)
    assert col is not None
    return row + 1, col + 1


def click_text(window, text):
    live.click(window, *locate(window, text))


def copies(window, start):
    return [base64.b64decode(value).decode() for value in re.findall(rb"\x1b\]52;c;([A-Za-z0-9+/=]+)\x07", bytes(window.raw[start:]))]


def quiet_frame(window, after, quiet=0.4, timeout=40):
    """Wait until output after a point exists and has paused for `quiet` s."""
    size, since = None, time.monotonic()
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        window.drain()
        if len(window.raw) != size:
            size, since = len(window.raw), time.monotonic()
        elif after() and time.monotonic() - since >= quiet:
            return
        time.sleep(0.03)
    raise AssertionError("released frame\n--- screen ---\n" + window.screen.text())


def has_reverse_sgr(data):
    return re.search(rb"\x1b\[(?:0;)?7(?:;[\d;]+)?m", data) is not None


def drag(window, first, last, expected):
    start = len(window.raw)
    y, x = first
    end_y, end_x = last
    window.send(f"\x1b[<0;{x};{y}M\x1b[<32;{end_x};{end_y}M".encode())
    # Wait for the actual painted selection, not just the event sent to stdin.
    live.until(lambda: has_reverse_sgr(bytes(window.raw[start:])), "drag highlight", window)
    window.send(f"\x1b[<0;{end_x};{end_y}m".encode())
    live.until(lambda: copies(window, start), "OSC 52 clipboard payload", window)
    assert copies(window, start) == [expected], (expected, copies(window, start), window.screen.text())
    # Clipboard is emitted before the release repaint. Read the completed frame
    # after that payload; Ctrl+L is deliberately owned locally by Settings.
    def released():
        data = bytes(window.raw[start:])
        match = list(re.finditer(rb"\x1b\]52;c;[A-Za-z0-9+/=]+\x07", data))[-1]
        return data[match.end():]
    if live.WINDOWS:
        # ConPTY answers a frame's status request itself and never passes it
        # on, so no ESC [5n marks the frame's end here: it is complete once
        # output after the payload has paused.
        quiet_frame(window, released)
    else:
        live.until(lambda: b"\x1b[?2026l\x1b[5n" in released(), "released frame", window)
    last_frame = released().split(b"\x1b[?2026h")[-1]
    assert not has_reverse_sgr(last_frame), "highlight survived copy release"
    ok(f"clipboard exact and highlight clears: {expected[:38]!r}")


def pair(window, first, last):
    a = locate(window, first, last=True)
    b = locate(window, last, last=True)
    return a, (b[0], b[1] + len(last) - 1)


def clear(window):
    window.send(b"\x1b[H")
    window.send(b"\x0b" * 12)


def main():
    provider = live.Provider()
    (live.HOME / "rune.toml").write_text(
        '[ui]\nmouse = true\nsidebar = true\n'
        '[host]\nidle_lifetime = "1s"\n'
        '[classifier]\nenabled = false\n'
        '[providers]\ndefault = "fake"\n'
        '[[providers.list]]\nname = "fake"\n'
        f'base_url = "http://127.0.0.1:{provider.port}/v1"\n'
        'auth = { header = "Authorization", prefix = "Bearer " }\n'
        '[[providers.list.models]]\nid = "fake-model"\nthinking = false\ncontext = 65536\n'
    )
    window = live.Window(["-project", str(live.PROJECT)], rows=46, cols=160)
    try:
        live.until(lambda: window.shows("ask, or / for commands"), "first paint", window)
        paste(window, "COPYINPUTabcdef")
        live.until(lambda: window.shows("› COPYINPUTabcdef"), "input", window)
        first = locate(window, "COPYINPUTabcdef")
        caret = (window.screen.y, window.screen.x)
        drag(window, (first[0], first[1] + 9), (first[0], first[1] + 12), "abc")
        assert (window.screen.y, window.screen.x) == caret
        window.send(b"X")
        live.until(lambda: window.shows("COPYINPUTabcdefX"), "typing after drag appends", window)
        live.click(window, first[0], first[1] + 9)
        window.send(b"Y")
        live.until(lambda: window.shows("COPYINPUTYabcdefX"), "click still places caret", window)
        ok("composer drag preserves draft/caret; click still moves caret")
        clear(window)
        live.until(lambda: window.shows("ask, or / for commands"), "clear input", window)
        provider.queue(live.text("COPYLINE-one\nCOPYLINE-two\n\n```text\n  CODELINE-one │ literal\n  CODELINE-two\n```"))
        paste(window, "copy response fixture")
        window.send(b"\r")
        live.until(lambda: window.shows("CODELINE-two"), "answer", window)
        live.settled(window)
        drag(window, *pair(window, "COPYLINE-one", "COPYLINE-two"), "COPYLINE-one\nCOPYLINE-two")
        drag(window, *pair(window, "  CODELINE-one", "  CODELINE-two"), "  CODELINE-one │ literal\n  CODELINE-two")
        # Boxed terminal output and borderless reader output, through real tools.
        window.type("/mode auto")
        window.send(b"\r")
        (live.PROJECT / "copy.txt").write_text("READLINE-one\nREADLINE-two\n")
        provider.queue(live.tool("terminal_use", {"action":"Copy output fixture", "command":"printf 'TOOLLINE-one\\nTOOLLINE-two\\n'"}), live.text("terminal copy ready"))
        paste(window, "run copy tool")
        window.send(b"\r")
        live.until(lambda: window.shows("TOOLLINE-two") and window.shows("terminal copy ready"), "terminal box", window)
        live.settled(window)
        drag(window, *pair(window, "TOOLLINE-one", "TOOLLINE-two"), "TOOLLINE-one\nTOOLLINE-two")
        provider.queue(live.tool("inspect_file", {"action":"Read copy fixture", "path":"copy.txt"}), live.text("reader copy ready"))
        paste(window, "read copy tool")
        window.send(b"\r")
        live.until(lambda: window.shows("READLINE-two") and window.shows("reader copy ready"), "reader", window)
        live.settled(window)
        first = locate(window, "READLINE-one")
        last = locate(window, "READLINE-two")
        drag(window, first, (last[0], last[1] + len("READLINE-two") - 1), "READLINE-one\n     2| READLINE-two")
        window.send(b"\x07")
        live.until(lambda: window.shows("01 / MODELS"), "settings", window)
        click_text(window, "01 / MODELS")
        live.until(lambda: window.shows("PROVIDERS"), "Models", window)
        # Search field is focused by a click rather than by keyboard navigation.
        click_text(window, "Type to filter models")
        paste(window, "MODELINPUTabcdef")
        live.until(lambda: window.shows("MODELINPUTabcdef"), "Models text", window)
        first = locate(window, "MODELINPUTabcdef")
        drag(window, first, (first[0], first[1] + 5), "MODEL")
        window.send(b"X")
        live.until(lambda: window.shows("MODELINPUTabcdefX"), "Models draft preserved", window)
        click_text(window, "Tools")
        live.until(lambda: window.shows("Command timeout"), "Tools Settings", window)
        click_text(window, "Command timeout")
        live.until(lambda: window.shows("Cancel") and window.shows("Apply"), "Settings editor", window)
        clear(window)
        paste(window, "123456s")
        live.until(lambda: window.shows("› 123456s"), "Settings value", window)
        first = locate(window, "123456s")
        drag(window, first, (first[0], first[1] + 3), "123")
        window.send(b"7")
        live.until(lambda: window.shows("123456s7"), "Settings edit preserved", window)
        click_text(window, "Cancel")
        live.press_escape(window, 2)
        live.until(lambda: window.shows("reader copy ready"), "Settings closed", window)
        # Read-only slash-command panel beside the same sidebar.
        window.type("/help")
        window.send(b"\r")
        live.until(lambda: window.shows("COMMANDS"), "help panel", window)
        first = locate(window, "/new")
        last = locate(window, "/set key=value")
        start_text = window.screen.text().splitlines()[first[0] - 1].split("│")[0]
        end_text = window.screen.text().splitlines()[last[0] - 1].split("│")[0]
        expected = start_text[first[1] - 1:].rstrip() + "\n" + end_text[:last[1] - 1 + len("/set key=value")].rstrip()
        drag(window, first, (last[0], last[1] + len("/set key=value") - 1), expected)
        window.send(b"\x1b")
        live.until(lambda: window.shows("reader copy ready"), "help closes", window)
        window.type("/session")
        window.send(b"\r")
        live.until(lambda: window.shows("Resume session"), "picker", window)
        paste(window, "PICKERabcdef")
        live.until(lambda: window.shows("PICKERabcdef"), "picker search", window)
        first = locate(window, "PICKERabcdef")
        drag(window, first, (first[0], first[1] + 6), "PICKER")
        window.send(b"X")
        live.until(lambda: window.shows("PICKERabcdefX"), "picker caret preserved", window)
        clear(window)
        window.send(b"\x12")
        live.until(lambda: window.shows("New title:"), "rename", window)
        clear(window)
        paste(window, "RENAMEabcdef")
        live.until(lambda: window.shows("RENAMEabcdef"), "rename input", window)
        first = locate(window, "RENAMEabcdef")
        drag(window, first, (first[0], first[1] + 6), "RENAME")
        window.send(b"X")
        live.until(lambda: window.shows("RENAMEabcdefX"), "rename caret preserved", window)
        ok("Settings, Models, picker search/rename keep source and click behavior")
        print(f"PASS ALL {COUNT} mouse copy PTY checks", flush=True)
    except BaseException:
        (live.ROOT / "FailedScreen.txt").write_text(window.screen.text())
        print("FAILED fixture:", live.ROOT, flush=True)
        raise
    finally:
        window.close()


if __name__ == "__main__":
    passed = False
    try:
        main()
        passed = True
    finally:
        for pid in live.RuneTerminal.holding(live.HOME, live.PROJECT):
            live.RuneTerminal.stop(pid)
        if passed:
            shutil.rmtree(live.ROOT, ignore_errors=True)
