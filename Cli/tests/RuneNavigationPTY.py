"""Input ownership and visual navigation on copied binaries and a local provider.
python3 -I -B Cli/tests/RuneNavigationPTY.py [path/to/rune]
"""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("live", Path(__file__).with_name("RuneLiveProviderPTY.py"))
live = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(live)
BINARY = live.ROOT / ("rune-navigation-test.exe" if live.WINDOWS else "rune-navigation-test")
shutil.copy2(live.BIN, BINARY)
live.BIN = str(BINARY)
os.environ["HOME"] = str(live.HOME)
WINDOWS = []
COUNT = 0
UP, DOWN, LEFT, RIGHT = b"\x1b[A", b"\x1b[B", b"\x1b[D", b"\x1b[C"
HOME, END, DELETE = b"\x1b[H", b"\x1b[F", b"\x1b[3~"
ALTLEFT, ALTRIGHT = b"\x1b[1;3D", b"\x1b[1;3C"


def ok(name):
    global COUNT
    COUNT += 1
    print(f"PASS {COUNT:03} {name}", flush=True)


def paint(window, seconds=0.2, grace=1.5):
    """Wait for what was just sent to be drawn: the screen above the status
    row changes, then holds still for `seconds`; nothing changing within
    `grace` means it changed nothing. A pause alone, fixed or `still for
    0.2 s`, read the screen from before the keys on a slow machine (macOS
    CI), where the first frame after them can take longer than that."""
    view = lambda: window.screen.text().rsplit("\n", 1)[0]
    window.drain()
    began = live.time.monotonic()
    last, still, changed = view(), began, False
    def drawn():
        nonlocal last, still, changed
        now = view()
        if now != last:
            last, still, changed = now, live.time.monotonic(), True
        if changed:
            return live.time.monotonic() - still >= seconds
        return live.time.monotonic() - began >= grace
    live.until(drawn, "frames never settled", window, grace + seconds + 10)


def composer():
    folder = live.session_dir(1)
    path = folder / "composer.json" if folder else None
    return json.loads(live.RuneTerminal.read_document(path)) if path and path.exists() else {}


def paste(window, text):
    window.send(b"\x1b[200~" + text.encode() + b"\x1b[201~")


def clear(window):
    window.send(HOME + DELETE * (len(composer().get("text", "")) + 1))
    live.until(lambda: composer().get("text", "") == "", "clear text", window)
    paint(window)


def command(window, text):
    clear(window)
    paste(window, text)
    window.send(b"\r")
    live.until(lambda: composer().get("text", "") == "", "command consumed", window)
    expected = {"/context": "what the model holds besides", "/undo": "UNDO", "/redo": "REDO", "/help": "esc close · or just carry on typing"}.get(text)
    if text == "/logs":
        live.until(lambda: selected_log(window) is not None, "Logs selector painted", window)
    elif expected:
        live.until(lambda: window.shows(expected), "screen opened: " + text, window)
    paint(window)


def click(window, text):
    row = live.row_with(window, text)
    assert row is not None, (text, window.screen.text())
    live.click(window, row + 1, live.column_of(window, row, text) + 1)
    paint(window)


def custom_focus(window):
    return any("Write your own" in row and "›" in row for row in window.screen.text().splitlines())


def selected_log(window):
    return next((r.strip() for r in window.screen.text().splitlines() if len(r) > 3 and r[2] == "›" and ("info" in r or "warn" in r or "error" in r)), None)


def run():
    provider = live.Provider()
    (live.HOME / "rune.toml").write_text(
        '[ui]\nmouse=true\nsidebar=false\n[host]\nidle_lifetime="1s"\n'
        '[questions]\ntimeout="5m"\n[providers]\ndefault="fake"\n[[providers.list]]\nname="fake"\n'
        f'base_url="http://127.0.0.1:{provider.port}/v1"\n'
        'auth={header="Authorization",prefix="Bearer "}\n'
        '[[providers.list.models]]\nid="fake-model"\nthinking=false\ncontext=65536\n')
    w = live.Window(["-project", str(live.PROJECT)], rows=32, cols=80)
    WINDOWS.append(w)
    live.until(lambda: w.shows("fake-model"), "first paint", w)
    live.ask(w, provider, "FIRST-HISTORY", live.text("FIRST-ACK"), expect="FIRST-ACK")
    live.settled(w)
    w.send(UP + b"X")
    live.until(lambda: composer().get("text") == "XFIRST-HISTORY", "first promoted history", w)
    ok("first promoted prompt is in live history")
    clear(w)
    for prompt in ["OLD-HISTORY", "SAME-HISTORY"]:
        live.ask(w, provider, prompt, live.text("ACK-" + prompt), expect="ACK-" + prompt)
        live.settled(w)
    paste(w, "SAME-HISTORY")
    live.until(lambda: composer().get("text") == "SAME-HISTORY", "matching draft", w)
    w.send(HOME + UP + UP)
    live.until(lambda: composer().get("text") == "OLD-HISTORY", "two explicit recalls", w)
    w.send(END + DOWN)
    live.until(lambda: composer().get("text") == "SAME-HISTORY", "latest history", w)
    w.send(DOWN + b"X")
    live.until(lambda: composer().get("text") == "SAME-HISTORYX", "matching draft restored", w)
    ok("draft equal to history is restored, not guessed away")
    clear(w)
    for value, keys, expected in [
        ("x" * 148, UP + UP + b"M", "M" + "x" * 148),
        ("h" * 74 + "\n" + "t" * 148, HOME + RIGHT * 74 + DOWN + b"M", "h" * 74 + "\n" + "t" * 74 + "M" + "t" * 74),
        ("x" * 148, b"\x1b[1;2A" + b"M", "x" * 74 + "M"),
    ]:
        paste(w, value)
        live.until(lambda: composer().get("text") == value, "wrap fixture", w)
        w.send(keys)
        live.until(lambda: composer().get("text") == expected, "wrapped edit", w)
        ok("full-width wrapped navigation/selection preserves the intended source")
        clear(w)
    # Enough real log rows to make the selector leave either viewport edge.
    for n in range(8):
        live.ask(w, provider, f"log-seed-{n}", live.text(f"LOG-ACK-{n}"), expect=f"LOG-ACK-{n}")
    for panel in ["/logs", "/context", "/undo", "/redo", "/help"]:
        command(w, panel)
        before = selected_log(w)
        paste(w, "/")
        paint(w)
        w.send(DOWN + DOWN)
        paint(w)
        # The palette may cover the selected log's row; compare its restored
        # identity after dismissal rather than treating covered cells as state.
        # Escape one layer: palette first, then this single screen.
        w.send(b"\x1b")
        live.until(lambda: composer().get("text", "") == "", "palette dismissed", w)
        paint(w)
        if panel == "/logs":
            assert selected_log(w) == before, "palette keys moved the log selector"
        w.send(b"\x1b")
        live.until(lambda: not w.shows("WHAT THE MODEL HOLDS") and not w.shows("LOGS\n"), "Escape closes " + panel, w)
        ok("palette owns arrows and Escape before " + panel)
    command(w, "/logs")
    for key in [HOME, END, UP * 15, DOWN * 10, b"\x1b[5~", b"\x1b[6~"]:
        w.send(key)
        paint(w)
        assert selected_log(w), w.screen.text()
        ok("Logs chevron remains visible through keyboard navigation")
    w.send(HOME)
    paint(w)
    first = w.screen.text()
    live.wheel(w, False, 8, 15)
    paint(w)
    assert selected_log(w) and w.screen.text() != first
    ok("Logs wheel moves chevron and viewport together")
    w.send(b"\r")
    live.until(lambda: w.shows("code "), "Enter opens the log's detail", w)
    w.send(b"\x1b")
    live.until(lambda: not w.shows("LOGS\n"), "Esc closes the open-detail screen", w)
    ok("Logs detail remains Enter-driven and one Escape closes its screen")
    # No waits between commands: delayed responses must not reopen the earlier screen.
    for sequence in [["/logs", "/context", "/help"], ["/context", "/logs", "/undo"],
                     ["/undo", "/redo", "/help"], ["/mcp", "/context", "/help"]]:
        for text in sequence:
            paste(w, text)
            w.send(b"\r")
        paint(w, 0.6)
        title = "esc close · or just carry on typing" if sequence[-1] == "/help" else "UNDO"
        assert w.shows(title), (sequence, w.screen.text())
        w.send(b"\x1b")
        paint(w, 0.5)
        assert w.shows("ask, or / for commands"), w.screen.text()
        assert not w.shows("LOGS\n") and not w.shows("WHAT THE MODEL HOLDS"), w.screen.text()
        ok("newest command replaces screens and stale responses: " + " -> ".join(sequence))
    for panel in ["/logs", "/context", "/help"]:
        command(w, panel)
        def rewind():
            if panel == "/logs":
                w.send(HOME)
            else:
                w.send(b"\x1b[5~" * 150)
            paint(w)
        for alt in [False, True]:
            rewind()
            for _ in range(3):
                live.wheel(w, False, 8, 15)
            paint(w)
            plain = w.screen.text()
            rewind()
            live.wheel(w, False, 8, 15, ctrl=not alt, alt=alt)
            paint(w)
            assert w.screen.text() == plain, (panel, alt, w.screen.text(), plain)
            ok(("Alt" if alt else "Ctrl") + "+wheel equals three ordinary notches in " + panel)
        w.send(b"\x1b")
        paint(w)
    w.send(b"\x07")
    live.until(lambda: w.shows("/ SETTINGS"), "Settings fast wheel", w)
    click(w, "Runtime")
    def chosen_setting():
        return next(r.strip() for r in w.screen.text().splitlines() if "❯" in r)
    for alt in [False, True]:
        w.send(HOME)
        paint(w)
        for _ in range(3):
            live.wheel(w, False, 12, 10)
        paint(w)
        plain = chosen_setting()
        w.send(HOME)
        paint(w)
        live.wheel(w, False, 12, 10, ctrl=not alt, alt=alt)
        paint(w)
        assert chosen_setting() == plain, (alt, chosen_setting(), plain)
        ok(("Alt" if alt else "Ctrl") + "+wheel accelerates Settings without entering tabs")
    live.press_escape(w, 2)
    paint(w)
    question = lambda title: {"title":title,"question":title+" body","type":"single",
        "options":[{"label":"Alpha","description":"First"},{"label":"Beta","description":"Second"}]}
    live.ask(w, provider, "QUESTION-NAVIGATION", live.tool("ask_user_questions", {
        "action":"Verify navigation", "questions":[question("First question"),question("Second question")]}),
        live.text("QUESTION-ACK"), expect="Write your own")
    # Each step waits for its own effect, never for "the screen held still":
    # keys sent together are drawn over several frames, and on a slow
    # machine (macOS CI, 2026-10-20) the first frame held still long enough
    # to be read before the paste and the arrows after it were drawn. Every
    # step starts from the custom field focused (waited for where it did not),
    # so leaving it is the effect.
    left = lambda what: live.until(lambda: not custom_focus(w), what, w)
    click(w, "Write your own")
    w.send(UP)
    left("empty custom Up")
    ok("empty custom Up leaves to preceding option")
    w.send(DOWN + DOWN)
    left("empty custom Down")
    ok("empty custom Down leaves to lower controls")
    click(w, "Write your own")
    live.until(lambda: custom_focus(w), "custom focused again", w)
    paste(w, "abc")
    live.until(lambda: w.shows("abc"), "custom answer typed", w)
    w.send(HOME + UP)
    left("nonempty custom Up at its start")
    w.send(DOWN)
    live.until(lambda: custom_focus(w), "custom focused from above", w)
    w.send(END + DOWN)
    left("nonempty custom Down at its end")
    ok("nonempty custom exits only at input boundaries")
    click(w, "Write your own")
    w.send(ALTRIGHT)
    live.until(lambda: w.shows("Second question body"), "AltRight question", w)
    w.send(ALTLEFT)
    live.until(lambda: w.shows("First question body") and w.shows("abc"), "AltLeft saved answer", w)
    ok("Alt arrows switch from custom and preserve question-local answer")
    click(w, "Write your own")
    w.send(HOME + DELETE * 3)
    paste(w, "q" * 140)
    paint(w)
    w.send(UP + UP + b"M")
    paint(w)
    # At top row Up moves to source start; another Up would leave the field.
    assert w.shows("M" + "q" * 69), w.screen.text()
    ok("Ask custom shares the corrected full-width soft-wrap geometry")
    w.send(b"\x07")
    live.until(lambda: w.shows("/ SETTINGS"), "settings over question", w)
    click(w, "Runtime")
    first = next(r for r in w.screen.text().splitlines() if "❯" in r)
    live.wheel(w, False, 12, 10)
    paint(w)
    second = next(r for r in w.screen.text().splitlines() if "❯" in r)
    assert first != second, w.screen.text()
    ok("Settings wheel works while Ask User waits")
    live.press_escape(w, 2)
    live.until(lambda: w.shows("Write your own"), "the question is back", w)
    w.send(b"\x14")
    paint(w)
    # Active overlay gets its navigation without touching the question answer.
    w.send(UP + DOWN + b"\x1b")
    paint(w)
    click(w, "Submit answers")
    live.until(lambda: w.shows("QUESTION-ACK"), "explicit answer submission", w)
    raw = [r for r in live.transcript() if r.get("tool_name") == "ask_user_questions"][-1]["content"]
    result = json.loads(raw[raw.index('{'):])
    assert result["answers"][0]["custom"] == "M" + "q" * 140, result
    ok("navigating other screens never changes or submits the custom answer")
    # A form can arrive while an already-open reader owns the screen. Hold the
    # provider at an explicit handshake, not a timing guess about HTTP latency.
    for panel in ["/logs", "/context", "/undo", "/redo"]:
        released = live.threading.Event()
        response = live.tool("ask_user_questions", {"action":"Check foreground ownership", "questions":[question("Ownership")]})
        def delayed(h, stream, event=released, reply=response):
            assert event.wait(20), "foreground screen was never opened"
            reply(h, stream)
        expected = "OWNERSHIP-ACK-" + panel
        provider.queue(delayed, live.text(expected))
        before = len(provider.main_calls())
        paste(w, "pending form over " + panel)
        w.send(b"\r")
        live.until(lambda: len(provider.main_calls()) > before, "provider handshake", w)
        command(w, panel)
        released.set()
        live.until(lambda: w.shows("Write your own"), "form arrives behind " + panel, w)
        w.send(DOWN + UP + b"\x1b[6~\x1b[5~")
        paint(w)
        if panel == "/logs":
            assert selected_log(w), w.screen.text()
        w.send(b"\r")
        paint(w)
        alpha = next(r for r in w.screen.text().splitlines() if "Alpha" in r)
        assert "●" not in alpha, (panel, "overlay Enter selected an answer", w.screen.text())
        w.send(b"\x1b")
        live.until(lambda: w.shows("Write your own"), "closing reader keeps the pending question", w)
        click(w, "Alpha")
        click(w, "Submit answers")
        live.until(lambda: w.shows(expected), "foreground answer completed", w)
        live.settled(w)
        ok(panel + " keeps navigation and Enter when Ask User arrives behind it")
    (live.ROOT / "final-screen.txt").write_text(w.screen.text())
    print(f"PASS ALL {COUNT} navigation checks", flush=True)
    provider.server.shutdown()


try:
    run()
finally:
    for window in reversed(WINDOWS):
        (live.ROOT / "last-screen.txt").write_text(window.screen.text())
        window.close()
    # The session hosts this run started run the copy too, and Windows
    # deletes no program that is running.
    for pid in live.RuneTerminal.holding(live.HOME, live.PROJECT):
        live.RuneTerminal.stop(pid)
    for _ in range(50):
        try:
            BINARY.unlink(missing_ok=True)
            break
        except PermissionError:
            live.time.sleep(0.1)
    print("ARTIFACTS", live.ROOT, flush=True)
