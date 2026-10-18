"""Regression for Appearance/Runtime ownership, custom-answer mouse and live diffs.

python3 -I Cli/tests/RuneAuditSettingsPTY.py [path/to/rune]
Uses the existing local provider and terminal model with an isolated RUNE_HOME.
"""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal

spec = importlib.util.spec_from_file_location("live", Path(__file__).with_name("RuneLiveProviderPTY.py"))
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)


def click_text(window, text):
    row = live.row_with(window, text)
    assert row is not None, (text, window.screen.text())
    col = live.column_of(window, row, text)
    live.click(window, row + 1, col + 1)


def sidebar(window):
    row = live.row_with(window, "SESSIONS")
    if row is None:
        return ""
    first = live.column_of(window, row, "SESSIONS")
    return "\n".join("".join(c for c in line[first:] if c != "\0") for line in window.screen.grid)


def main():
    provider = live.Provider()
    (live.HOME / "rune.toml").write_text(
        '[ui]\nmouse = true\nsidebar = true\n[host]\nidle_lifetime = "1s"\n'
        '[providers]\ndefault = "fake"\n[[providers.list]]\nname = "fake"\n'
        f'base_url = "http://127.0.0.1:{provider.port}/v1"\n'
        'auth = { header = "Authorization", prefix = "Bearer " }\n'
        '[[providers.list.models]]\nid = "fake-model"\nthinking = false\ncontext = 65536\n'
    )
    windows = []
    a = live.Window(["-project", str(live.PROJECT)], rows=46, cols=180)
    windows.append(a)
    try:
        live.until(lambda: a.shows("fake-model"), "first paint", a)
        a.send(b"\x07")
        live.until(lambda: a.shows("01 / MODELS"), "Settings", a)
        click_text(a, "Appearance")
        live.until(lambda: a.shows("Color theme"), "Appearance", a)
        assert "Preview Start" not in a.screen.text() and "LRU cache" not in a.screen.text()
        click_text(a, "Runtime")
        live.until(lambda: a.shows("Credit cooldown"), "Runtime", a)
        for wrong in ["Command deadline", "Command timeout", "Permission timeout", "Question timeout"]:
            assert wrong not in a.screen.text(), a.screen.text()
        click_text(a, "Tools")
        live.until(lambda: a.shows("Command timeout"), "Tools", a)
        click_text(a, "Command timeout")
        live.until(lambda: a.shows("Apply") and a.shows("Cancel"), "timeout editor", a)
        row = live.row_with(a, "› 120s")
        assert row is not None, a.screen.text()
        live.click(a, row + 1, 8)  # after the first source character
        live.until(lambda: a.screen.y == row and a.screen.x == 7, "Settings mouse caret", a)
        click_text(a, "Cancel")
        a.send(b"\x1b\x1b")
        live.until(lambda: a.shows("ask, or / for commands"), "return to conversation", a)
        print("ok Appearance removed preview, Runtime no tool duplicates, Settings text caret")

        form = {"action":"Ask for an answer", "questions":[{
            "title":"Caret audit", "question":"Choose or write", "type":"single",
            "options":[{"label":"Choice A", "description":"Use a predefined answer"}]
        }]}
        live.ask(a, provider, "Ask me", live.tool("ask_user_questions", form), live.text("Questions finished."), expect="Write your own")
        click_text(a, "Write your own")
        a.send(b"\x1b[200~abcdef\x1b[201~")
        live.until(lambda: a.shows("abcdef"), "custom answer text", a)
        row = live.row_with(a, "abcdef")
        live.click(a, row + 1, 11)
        live.until(lambda: a.screen.y == row and a.screen.x == 10, "custom-answer mouse caret", a)
        a.send(b"X")
        live.until(lambda: a.shows("abXcdef"), "typing at clicked caret", a)
        click_text(a, "Submit answers")
        live.until(lambda: a.shows("Questions finished."), "custom answer submitted", a)
        result = next(r for r in live.transcript() if r.get("tool_name") == "ask_user_questions")
        assert "abXcdef" in result["content"], result
        print("ok Questions mouse caret and submitted text")

        (live.PROJECT / "notes.txt").write_text("one\ntwo\nthree\n")
        live.ask(a, provider, "Change notes", live.tool("patch_file", {"action":"Edit notes", "path":"notes.txt", "old_str":"one", "new_str":"ONE"}), live.text("Notes edited."), expect="Allow once")
        click_text(a, "Allow once")
        live.until(lambda: a.shows("Notes edited."), "patch completes after approval", a)
        live.until(lambda: "notes.txt" in sidebar(a) and "+1" in sidebar(a), "initial sidebar diff", a)
        b = live.Window(["-project", str(live.PROJECT), "-resume", "1"], rows=46, cols=180)
        windows.append(b)
        live.until(lambda: "notes.txt" in sidebar(b) and "+1" in sidebar(b), "joiner sidebar", b)
        before = len(provider.main_calls())
        (live.PROJECT / "notes.txt").write_text("ONE\nTWO\nthree\n")
        live.until(lambda: "+2" in sidebar(a), "watcher first window diff", a)
        live.until(lambda: "+2" in sidebar(b), "watcher second window diff", b)
        (live.PROJECT / "notes.txt").write_text("one\ntwo\nthree\n")
        live.until(lambda: "notes.txt" in sidebar(a) and "+2" not in sidebar(a), "external restore A", a)
        live.until(lambda: "notes.txt" in sidebar(b) and "+2" not in sidebar(b), "external restore B", b)
        assert len(provider.main_calls()) == before, "watcher cannot trigger model inference"
        print("ok content-based external file diff parity across two windows without /undo")
        print("PASS audit Settings/Questions/sidebar PTY")
    except BaseException:
        (live.ROOT / "FailedScreen.txt").write_text(a.screen.text())
        print("FAILED fixture:", live.ROOT)
        raise
    finally:
        for w in windows:
            w.close()


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
