"""Real PTY regression for context/mouse/composer/user-preview UI contracts.

    python3 -I Cli/tests/RuneMouseComposerPTY.py [path/to/rune]

Uses the existing terminal model, a local HTTP provider, and an isolated home;
no desktop settings, live sessions, or owner credentials are read or changed.
"""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import sys

spec = importlib.util.spec_from_file_location(
    "rune_live_pty", Path(__file__).with_name("RuneLiveProviderPTY.py")
)
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)


def row_with(window, text):
    row = live.row_with(window, text)
    assert row is not None, f"missing {text!r}\n{window.screen.text()}"
    return row


def composer_row(window, lines):
    """The screen row the composer's first line is drawn on once it shows
    exactly `lines`, one a row; `None` before. Waiting on a substring such as
    `› 1` was answered by the earlier `› 12X34` still on screen, and the click
    landed on that frame's geometry (macOS CI, 2026-10-19)."""
    rows = [row.strip("│ ").rstrip() for row in window.screen.text().split("\n")]
    for y in range(len(rows) - len(lines) + 1):
        if rows[y] == "› " + lines[0] and [r.strip() for r in rows[y + 1:y + len(lines)]] == lines[1:]:
            return y
    return None


def click_text(window, text):
    row = row_with(window, text)
    col = live.column_of(window, row, text)
    live.click(window, row + 1, col + 1)


def draft():
    path = live.HOME / "drafts.json"
    if not path.exists():
        return {}
    for project in json.loads(path.read_text()).get("projects", {}).values():
        for entry in project.get("entries", []):
            return entry["composer"]
    return {}


def paste(window, text):
    window.send(b"\x1b[200~" + text.encode() + b"\x1b[201~")


def resize(window, rows, cols):
    window.screen = live.Screen(rows, cols)
    window.resize(rows, cols)
    window.send(b"\x0c")


def main():
    provider = live.Provider()
    (live.HOME / "rune.toml").write_text(
        '[ui]\nmouse = true\nsidebar = true\n'
        '[host]\nidle_lifetime = "1s"\n'
        '[providers]\ndefault = "fake"\n'
        '[[providers.list]]\nname = "fake"\n'
        f'base_url = "http://127.0.0.1:{provider.port}/v1"\n'
        'auth = { header = "Authorization", prefix = "Bearer " }\n'
        '[[providers.list.models]]\nid = "fake-model"\nthinking = false\ncontext = 65536\n'
    )
    window = live.Window(["-project", str(live.PROJECT)], rows=46, cols=160)
    try:
        live.until(lambda: window.shows("fake-model"), "first paint", window)
        # All mouse events enter through the byte decoder and normal UI dispatch.
        window.send(b"\x07")  # Ctrl+G Settings
        live.until(lambda: window.shows("01 / MODELS"), "Settings overview", window)
        click_text(window, "01 / MODELS")
        live.until(lambda: window.shows("PROVIDERS") and not window.shows("01 / MODELS"),
                   "mouse opens Models card", window)
        click_text(window, "Overview")
        live.until(lambda: window.shows("01 / MODELS"), "mouse switches Overview tab", window)
        click_text(window, "Tools")
        live.until(lambda: window.shows("Command timeout"), "mouse switches Tools tab", window)
        click_text(window, "Command timeout")
        live.until(lambda: window.shows("Cancel") and window.shows("Apply"), "mouse opens editor", window)
        click_text(window, "Cancel")
        live.until(lambda: not window.shows("Cancel"), "mouse cancels editor", window)
        live.press_escape(window, 2)
        live.until(lambda: window.shows("ask, or / for commands"), "Settings closes", window)
        print("ok Settings card/tab/editor/Cancel clicks")

        paste(window, "1234")
        live.until(lambda: draft().get("text") == "1234" and window.shows("› 1234"),
                   "single-line draft", window)
        row = row_with(window, "› 1234")
        live.click(window, row + 1, 7)
        live.until(lambda: window.screen.y == row and window.screen.x == 6,
                   "click between 2 and 3", window)
        window.send(b"X")
        live.until(lambda: draft().get("text") == "12X34", "insert at clicked caret", window)
        window.send(b"\x05\x15")  # End, kill to line start
        paste(window, "1\n2\n3\n")
        live.until(lambda: draft().get("text") == "1\n2\n3\n"
                   and composer_row(window, ["1", "2", "3", ""]) is not None,
                   "multiline draft", window)
        row = composer_row(window, ["1", "2", "3", ""])
        live.click(window, row + 1, 80)
        live.until(lambda: window.screen.y == row and window.screen.x == 5,
                   "blank on row one goes to row end", window)
        window.send(b"X")
        live.until(lambda: draft().get("text") == "1X\n2\n3\n", "insert at first-line end", window)
        # Empty trailing line remains a real click target.
        live.click(window, row + 4, 5)
        live.until(lambda: window.screen.y == row + 3 and window.screen.x == 4,
                   "click trailing empty line", window)
        print("ok caret between characters, blank line-end, trailing empty line")

        # Delete this eight-byte draft from its source start; no prompt is sent.
        window.send(b"\x1b[H")
        for _ in range(8):
            window.send(b"\x1b[3~")
        live.until(lambda: draft().get("text", "") == "" and window.shows("ask, or / for commands"),
                   "clear multiline draft", window)
        long = "\n".join(f"input-{n:02}" for n in range(20))
        paste(window, long)
        live.until(lambda: draft().get("text") == long and window.shows("input-19"),
                   "long draft", window)
        visible = [line for line in window.screen.text().splitlines() if "input-" in line]
        assert len(visible) == 11, window.screen.text()
        resize(window, 12, 60)
        live.until(lambda: window.shows("input-19"), "short-terminal input reflow", window)
        visible = [line for line in window.screen.text().splitlines() if "input-" in line]
        assert 1 <= len(visible) <= 7, window.screen.text()
        resize(window, 46, 160)
        live.until(lambda: window.shows("input-19"), "large-terminal input reflow", window)
        provider.queue(live.text("Answer to folded prompt."))
        window.send(b"\r")
        live.until(lambda: window.shows("Answer to folded prompt.") and window.shows("16 lines cut"),
                   "sent user prompt folds at four lines", window)
        assert not window.shows("input-19"), window.screen.text()
        stored = next(row for row in live.transcript() if row.get("role") == "user")
        assert stored["content"] == long, stored
        calls = provider.main_calls()
        assert long in "\n".join(live.messages_of(calls[-1])), calls[-1]
        click_text(window, "16 lines cut")
        live.until(lambda: window.shows("input-19") and not window.shows("lines cut"),
                   "mouse expands whole user message", window)
        click_text(window, "you")
        live.until(lambda: window.shows("16 lines cut") and not window.shows("input-19"),
                   "mouse refolds user message", window)
        print("ok input up to 11 rows, short-terminal clamp, four-line user preview, whole prompt stored/sent")

        window.type("/context")
        window.send(b"\r")
        live.until(lambda: window.shows("SYSTEM PROMPT"), "context panel", window)
        # Ignore the sidebar seam; only the context source's left columns count.
        rows = window.screen.text().splitlines()
        source = [line[:90] for line in rows if line.startswith("    ")]
        assert source and all(not line.startswith("    │") for line in source), window.screen.text()
        assert any("You are" in line or "You keep" in line for line in source), window.screen.text()
        window.send(b"\x1b")
        live.until(lambda: window.shows("Answer to folded prompt."), "context closes", window)
        print("ok context source without left gutter")
        window.type("/session")
        window.send(b"\r")
        live.until(lambda: window.shows("Resume session"), "session picker opens", window)
        # This fixture has one selected session; clicking its metadata resumes.
        click_text(window, "#1")
        live.until(lambda: not window.shows("Resume session") and window.shows("Answer to folded prompt."),
                   "mouse resumes selected session", window)
        print("ok session picker mouse resumes selected card")
        print("PASS mouse/composer/context/user preview PTY")
    except BaseException:
        (live.ROOT / "FailedScreen.txt").write_text(window.screen.text())
        print("FAILED fixture:", live.ROOT)
        raise
    finally:
        window.close()


if __name__ == "__main__":
    passed = False
    try:
        main()
        passed = True
    finally:
        # Only processes carrying this test's isolated home/project are touched.
        for pid in live.RuneTerminal.holding(live.HOME, live.PROJECT):
            live.RuneTerminal.stop(pid)
        if passed:
            shutil.rmtree(live.ROOT, ignore_errors=True)
