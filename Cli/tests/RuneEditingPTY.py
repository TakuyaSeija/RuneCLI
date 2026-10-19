"""Canonical editing end-to-end: copied binary, local provider/MCP, isolated home.

python3 -I -B Cli/tests/RuneEditingPTY.py [path/to/rune]
No owner settings, desktop, credentials, or external model service are used.
"""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("live", Path(__file__).with_name("RuneLiveProviderPTY.py"))
live = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(live)
BINARY = live.ROOT / ("rune-editing-test.exe" if live.WINDOWS else "rune-editing-test")
shutil.copy2(live.BIN, BINARY)
live.BIN = str(BINARY)
os.environ["HOME"] = str(live.HOME)
ENV = dict(os.environ, HOME=str(live.HOME), RUNE_HOME=str(live.HOME))
WINDOWS = []
COUNT = 0

LEFT = b"\x1b[D"
RIGHT = b"\x1b[C"
HOME = b"\x1b[H"
END = b"\x1b[F"
DELETE = b"\x1b[3~"
CTRLLEFT = b"\x1b[1;5D"
CTRLRIGHT = b"\x1b[1;5C"
SHIFTLEFT = b"\x1b[1;2D"
SHIFTRIGHT = b"\x1b[1;2C"
CSLEFT = b"\x1b[1;6D"
CSRIGHT = b"\x1b[1;6C"

CASES = [
    ("CtrlK middle", "abcTAIL", 3, b"\x0b", "abc"),
    ("CtrlK EOF", "abc", 3, b"\x0b", "abc"),
    ("CtrlU prefix", "abcTAIL", 3, b"\x15", "TAIL"),
    ("CtrlLeft", "one two", 7, CTRLLEFT + b"X", "one Xtwo"),
    ("CtrlRight", "one two", 0, CTRLRIGHT + b"X", "oneX two"),
    ("CtrlW", "one two", 7, b"\x17", "one "),
    ("AltD", "one two", 3, b"\x1bd", "one"),
    ("CtrlY", "one two", 7, b"\x17\x19", "one two"),
    ("undo", "one two", 7, b"\x17\x1f", "one two"),
    ("redo", "one two", 7, b"\x17\x1f\x1br", "one "),
    ("AltY", "one two", 7, b"\x17" + LEFT + END + b"\x15\x19\x1by", "two"),
    ("select glyph left", "abcd", 3, SHIFTLEFT + b"X", "abXd"),
    ("select glyph right", "abcd", 1, SHIFTRIGHT + DELETE, "acd"),
    ("select word left", "one two", 7, CSLEFT + b"X", "one X"),
    ("select word right", "one two", 0, CSRIGHT + b"\x7f", " two"),
    ("transpose glyph", "abcd", 2, b"\x1bx", "acbd"),
    ("transpose words", "one two", 7, b"\x1bw", "two one"),
    ("uppercase", "one two", 4, b"\x1bu", "one TWO"),
    ("lowercase", "ONE two", 0, b"\x1bl", "one two"),
    ("capitalize", "hELLo two", 0, b"\x1bv", "Hello two"),
    ("line start", "abc", 3, b"\x01X", "Xabc"),
    ("line end", "abc", 0, b"\x05X", "abcX"),
    ("buffer start", "abc", 3, HOME + b"X", "Xabc"),
    ("buffer end", "abc", 0, END + b"X", "abcX"),
    ("paste spaces", "abc", 1, b"\x1b[200~ X \x1b[201~", "a X bc"),
    ("accent backspace", "e\u0301x", 2, b"\x7f", "x"),
    ("ZWJ delete", "👩\u200d💻x", 0, DELETE, "x"),
    ("skin tone delete", "👍🏽x", 0, DELETE, "x"),
    ("flag delete", "🇮🇩x", 0, DELETE, "x"),
]
MULTI = [
    ("CtrlK logical middle", "first\nabcTAIL\nlast", 9, b"\x0b", "first\nabc\nlast"),
    ("CtrlK newline", "abc\nnext", 3, b"\x0b", "abcnext"),
    ("sticky column", "abcd\nx\nabcd", 3, b"\x1b[B\x1b[BX", "abcd\nx\nabcXd"),
    ("line start not buffer", "first\nsecond", 12, b"\x01X", "first\nXsecond"),
    ("line end not buffer", "first\nsecond", 1, b"\x05X", "firstX\nsecond"),
    ("select row down", "abc\ndef", 1, b"\x1b[1;2BX", "aXef"),
    ("select row up", "abc\ndef", 5, b"\x1b[1;2AX", "aXef"),
]


def ok(name):
    global COUNT
    COUNT += 1
    print(f"PASS {COUNT:03} {name}", flush=True)


def draft():
    path = live.HOME / "drafts.json"
    if not path.exists():
        return {}
    for project in json.loads(path.read_text()).get("projects", {}).values():
        for entry in project.get("entries", []):
            return entry["composer"]
    return {}


def pending():
    folder = live.session_dir(1)
    if not folder:
        return draft()
    path = folder / "composer.json"
    return json.loads(path.read_text()) if path.exists() else {}


def paste(window, text):
    window.send(b"\x1b[200~" + text.encode() + b"\x1b[201~")


def click_text(window, text):
    row = live.row_with(window, text)
    assert row is not None, (text, window.screen.text())
    col = live.column_of(window, row, text)
    live.click(window, row + 1, col + 1)


def position(window, text, at):
    # Source offsets in CASES count code points; Right walks complete visible
    # glyphs, so locate by one real renderer click where a combining suffix is
    # present. Our only non-ASCII middle case is the end of e + combining acute.
    window.send(HOME)
    head = text[:at]
    count = len(head)
    if head == "e\u0301":
        count = 1
    window.send(RIGHT * count)


def clear(window):
    window.send(END + b"\x1b[1;6H")  # unsupported chord must do nothing
    # Home is the whole buffer start, not Ctrl+A's logical-line start.
    window.send(HOME + DELETE * 600)


def case_keys(window, case):
    name, initial, at, keys, expected = case
    clear(window)
    paste(window, initial)
    position(window, initial, at)
    window.send(keys)
    return name, expected


def field_value(window, prefix):
    for row in window.screen.text().splitlines():
        if prefix in row:
            rest = row.split(prefix, 1)[1]
            return rest.rsplit("│", 1)[0].rstrip()
    return None


def wait_field(window, expected, prefix="› "):
    # A harmless marker disambiguates empty results and ignores unrelated chat
    # text. It is not an undo operation and cannot hide a wrong actual result.
    window.send(END + b"~")
    live.until(lambda: field_value(window, prefix) == expected + "~", f"field expected {expected!r}", window, 15)
    return expected


def drag_text(window, text, start, end):
    row=live.row_with(window,text)
    assert row is not None,(text,window.screen.text())
    col=live.column_of(window,row,text)
    for button,offset,final in [(0,start,"M"),(32,end,"M"),(0,end,"m")]:
        window.send(f"\x1b[<{button};{col+offset+1};{row+1}{final}".encode())


def run():
    provider = live.Provider()
    config = (
        '[ui]\nmouse=true\nsidebar=false\n[host]\nidle_lifetime="1s"\n'
        '[questions]\ntimeout="5m"\n'
        '[providers]\ndefault="fake"\n[[providers.list]]\nname="fake"\n'
        f'base_url="http://127.0.0.1:{provider.port}/v1"\n'
        'auth={header="Authorization",prefix="Bearer "}\n'
        '[[providers.list.models]]\nid="fake-model"\nthinking=false\ncontext=65536\n'
    )
    (live.HOME / "rune.toml").write_text(config)
    window = live.Window(["-project", str(live.PROJECT)], rows=46, cols=160)
    WINDOWS.append(window)
    try:
        live.until(lambda: window.shows("fake-model"), "first paint", window)
        for case in CASES + MULTI:
            name, expected = case_keys(window, case)
            # Append marker, then check disk, not only rendered row strings.
            window.send(END + b"~")
            live.until(lambda: draft().get("text") == expected + "~", name, window, 15)
            ok("composer " + name)
        clear(window)
        paste(window, "abc")
        window.send(END + b"\x18\x15X")
        live.until(lambda: draft().get("text") == "abcX", "unsupported undo prefix cannot erase", window)
        ok("legacy undo prefix cannot fall through into destructive CtrlU")
        window.send(b"\x1b_Gi=31;OK\x1b\\X")
        live.until(lambda: draft().get("text") == "abcXX", "terminal report not typed", window)
        ok("terminal reply never becomes user text")
        clear(window);paste(window,"abcdef")
        live.until(lambda:window.shows("abcdef"),"drag composer text",window)
        drag_text(window,"abcdef",2,4);window.send(b"X")
        live.until(lambda:draft().get("text")=="abXef","drag replaces composer source",window)
        window.send(b"\x1f")
        live.until(lambda:draft().get("text")=="abcdef","drag edit undo",window)
        ok("composer mouse drag edits source and undo restores it")
        clear(window);paste(window,"composer-local");window.send(b"\x15")
        live.until(lambda:draft().get("text","")=="","composer own kill-ring prepared",window)
        window.send(b"\x07")
        live.until(lambda: window.shows("01 / MODELS"), "Settings", window)
        click_text(window, "Generation")
        live.until(lambda: window.shows("Stop sequences"), "Generation", window)
        click_text(window, "Stop sequences")
        live.until(lambda: window.shows("Apply") and window.shows("Cancel"), "Settings text editor", window)
        for case in CASES:
            name, expected = case_keys(window, case)
            wait_field(window, expected)
            ok("Settings text " + name)
        clear(window);paste(window,"abcdef")
        live.until(lambda:field_value(window,"› ")=="abcdef","drag Settings text",window)
        drag_text(window,"abcdef",2,4);window.send(b"X")
        wait_field(window,"abXef")
        ok("Settings mouse drag edits source")
        click_text(window, "Cancel")
        click_text(window,"Connections")
        live.until(lambda:window.shows("fake") and window.shows("Provider"),"fixture connection",window)
        click_text(window,"fake")
        live.until(lambda:window.shows("Add API key"),"fixture account page",window)
        click_text(window,"Add API key")
        live.until(lambda:window.shows("Apply") and window.shows("Cancel"),"key name editor",window)
        paste(window,"Editing fixture")
        window.send(b"\r")
        live.until(lambda:window.shows("API key / fake"),"masked editor",window)
        paste(window,"abcTAIL");position(window,"abcTAIL",3);window.send(b"\x0b")
        live.until(lambda:field_value(window,"› ")=="•••","masked forward kill",window)
        window.send(b"\x1f")
        live.until(lambda:field_value(window,"› ")=="•••••••","masked undo",window)
        window.send(b"\x1br\x19")
        live.until(lambda:field_value(window,"› ")=="•••••••","masked redo and yank",window)
        assert not window.shows("abcTAIL"),window.screen.text()
        click_text(window,"Cancel")
        window.send(b"\x1b\x1b\x1b")
        live.until(lambda: window.shows("ask, or / for commands"), "Settings closed", window)
        window.send(b"\x19X")
        live.until(lambda:draft().get("text")=="composer-localX","secret kill ring never enters composer",window)
        ok("secret field edits stay masked and kill ring never crosses fields")
        clear(window)
        # One real question per case: verifies actual submitted custom content.
        for n, case in enumerate(CASES + MULTI):
            provider.queue(live.tool("ask_user_questions", {
                "action": "Editing regression",
                "questions": [{"title": f"Case {n}", "question": "Write the edited value.", "type": "single",
                               "options": [{"label": "Skip", "description": "Not used by this fixture"}]}]
            }), live.text(f"Editing answer {n} accepted."))
            clear(window)
            paste(window, f"editing case {n}")
            window.send(b"\r")
            live.until(lambda: window.shows("Write your own"), "question", window)
            click_text(window, "Write your own")
            name, expected = case_keys(window, case)
            click_text(window, "Submit answers")
            live.until(lambda: window.shows(f"Editing answer {n} accepted."), "answer completed", window)
            results = [r for r in live.transcript() if r.get("tool_name") == "ask_user_questions"]
            result = json.loads(results[-1]["content"])
            assert result["answers"][0]["custom"] == expected, (name, result)
            ok("Questions stored answer " + name)
        # Host ACK keeps a local undo; foreign input invalidates it instead of
        # reverting a peer. Both the stored composer and the other screen agree.
        clear(window)
        peer=live.Window(["-project",str(live.PROJECT),"-resume","1"],rows=46,cols=160)
        WINDOWS.append(peer)
        live.until(lambda:peer.shows("Editing answer"),"peer joins",peer)
        paste(window,"abcTAIL")
        live.until(lambda:pending().get("text")=="abcTAIL","shared composer",window)
        live.until(lambda:peer.shows("abcTAIL"),"peer receives composer",peer)
        position(window,"abcTAIL",3);window.send(b"\x0b")
        live.until(lambda:pending().get("text")=="abc","kill persisted",window)
        window.send(b"\x1f")
        live.until(lambda:pending().get("text")=="abcTAIL","undo after host ACK",window)
        live.until(lambda:peer.shows("abcTAIL"),"peer receives undo",peer)
        peer.send(END+b"Z")
        live.until(lambda:pending().get("text")=="abcTAILZ","foreign change stored",peer)
        live.until(lambda:window.shows("abcTAILZ"),"first window receives foreign input",window)
        window.send(b"\x1f"+END+b"X")
        live.until(lambda:pending().get("text")=="abcTAILZX","undo cannot overwrite peer text",window)
        live.until(lambda:peer.shows("abcTAILZX"),"peer sees exact input",peer)
        ok("two-window ACK preserves local undo; peer replacement invalidates stale history")
        peer.close();WINDOWS.remove(peer)
        clear(window)
        paste(window, "dasdas\nfdsfds")
        position(window, "dasdas\nfdsfds", 3)
        window.send(b"\x1b[A")
        window.send(b"\x1b[A")
        live.until(lambda: pending().get("text") == "editing case 35", "Up at 0 recalls previous history prompt", window)
        # In recalled prompt (cursor at 0), move right into text:
        window.send(b"\x1b[C\x1b[C")
        # Press Up: must move cursor to 0, NOT switch history:
        window.send(b"\x1b[A")
        assert pending().get("text") == "editing case 35", "Up while inside recalled prompt must not jump history"
        # Press Up again at 0: now it recalls case 34:
        window.send(b"\x1b[A")
        live.until(lambda: pending().get("text") == "editing case 34", "Up at 0 recalls earlier history", window)
        # Move right into case 34:
        window.send(b"\x1b[C\x1b[C")
        # Press Down: must move cursor to end, NOT switch history:
        window.send(b"\x1b[B")
        assert pending().get("text") == "editing case 34", "Down while inside recalled prompt must not jump history"
        # Press Down again at end: now advances history to case 35:
        window.send(b"\x1b[B")
        live.until(lambda: pending().get("text") == "editing case 35", "Down at end advances history", window)
        # Press Down again at end: restores multiline draft:
        window.send(b"\x1b[B")
        live.until(lambda: pending().get("text") == "dasdas\nfdsfds", "Down restores multiline draft", window)
        position(window, "dasdas\nfdsfds", 10)
        window.send(b"\x1b[BX")
        live.until(lambda: pending().get("text") == "dasdas\nfdsfdsX", "Down at bottom row moves to end", window)
        ok("multiline Up moves to 0 then recalls history; Down moves to end then advances history")
        clear(window)
        window.send(b"\x1b[A")
        live.until(lambda: pending().get("text") == "editing case 35", "Up on empty composer recalls history", window)
        window.send(b"\x1b[A")
        live.until(lambda: pending().get("text") == "editing case 34", "Up at 0 recalls earlier history", window)
        window.send(b"\x1b[BX")
        live.until(lambda: pending().get("text") == "editing case 34X", "Down moves cursor to end of recalled prompt", window)
        ok("empty Up and cursor-0 Up recall history; Down moves to end")
        clear(window)
        # Isolated local MCP server follows the real JSON-RPC input broker.
        fixture = live.PROJECT / "McpFixture.py"
        fixture_source=(Path(__file__).resolve().parents[2]/"Engine/tests/McpFixture.py").read_text()
        fields={"composite":{"type":"string"},**{f"case_{n:02}":{"type":"string"} for n in range(len(CASES))}}
        old='"properties":{"name":{"type":"string", "minLength":2}}, "required":["name"]'
        assert old in fixture_source
        fixture_source=fixture_source.replace(old,'"properties":'+json.dumps(fields)+', "required":'+json.dumps(list(fields)))
        fixture_source=fixture_source.replace('payload["user_name"] = request["params"]["inputResponses"]["details"].get("content", {}).get("name")','payload["user_fields"] = request["params"]["inputResponses"]["details"].get("content", {})')
        fixture.write_text(fixture_source)
        # The interpreter itself, not a virtual environment's (CI runs the
        # fixtures from one): a server reads only what its confinement grants,
        # and a venv's own folder is a `read_roots` choice of the person's.
        python = getattr(sys, "_base_executable", None) or sys.executable
        args = [str(BINARY), "-project", str(live.PROJECT), "-mcp", "add", "editing", python, str(fixture)]
        added = subprocess.run(args, env=ENV, capture_output=True, text=True, timeout=20)
        assert added.returncode == 0, added.stderr + added.stdout
        clear(window)
        paste(window, "/mcp connect editing")
        window.send(b"\r")
        live.until(lambda: window.shows("ready"), "MCP connects", window)
        window.send(b"\x1b")
        provider.queue(live.tool("call_mcp_tool", {"action": "Test input", "server_id": "editing", "tool": "echo", "arguments": {"action": "ask", "items": []}}), live.text("MCP edited answer accepted."))
        clear(window)
        paste(window, "Ask the local MCP server")
        window.send(b"\r")
        live.until(lambda: window.shows("Allow once") or window.shows("Enter the name to use."), "MCP permission/form", window)
        if window.shows("Allow once"):
            click_text(window, "Allow once")
        live.until(lambda: window.shows("Enter the name to use."), "MCP input", window)
        # Current MCP input is rendered without a prefix: inspect submission
        # content after each operation in unit matrix; E2E proves one composite
        # edit really crosses the server boundary intact.
        paste(window, "abcTAIL")
        position(window, "abcTAIL", 3)
        window.send(b"\x0b\x1f\x1br\x19")
        # Next field via Tab/next, then every canonical case on the real form.
        window.send(b"\r")
        expected_mcp={"composite":"abcTAIL"}
        for n,case in enumerate(CASES):
            name,expected=case_keys(window,case)
            expected_mcp[f"case_{n:02}"]=expected
            window.send(b"\r")
        click_text(window, "Submit")
        live.until(lambda: window.shows("MCP edited answer accepted."), "MCP result", window)
        result = [r for r in live.transcript() if r.get("tool_name") == "call_mcp_tool"][-1]["content"]
        actual_mcp=json.loads(result)["structuredContent"]["user_fields"]
        assert actual_mcp==expected_mcp,(actual_mcp,expected_mcp)
        ok("MCP real server receives composite kill/undo/redo/yank")
        for n,case in enumerate(CASES):
            assert actual_mcp[f"case_{n:02}"]==case[4],(case[0],actual_mcp)
            ok("MCP stored field "+case[0])
        clear(window)
        paste(window, "/session")
        window.send(b"\r")
        live.until(lambda: window.shows("Resume session"), "picker", window)
        for case in CASES:
            name, expected = case_keys(window, case)
            wait_field(window, expected, "⌕ ")
            ok("picker search " + name)
        clear(window);paste(window,"abcdef")
        live.until(lambda:field_value(window,"⌕ ")=="abcdef","picker drag search",window)
        drag_text(window,"abcdef",2,4);window.send(b"X")
        wait_field(window,"abXef","⌕ ")
        ok("picker search mouse drag edits source")
        clear(window)
        window.send(b"\x12")
        live.until(lambda: window.shows("New title:"), "rename", window)
        for case in CASES:
            name, expected = case_keys(window, case)
            wait_field(window, expected, "New title: ")
            ok("picker rename " + name)
        clear(window);paste(window,"abcdef")
        live.until(lambda:field_value(window,"New title: ")=="abcdef","picker drag rename",window)
        drag_text(window,"abcdef",2,4);window.send(b"X")
        wait_field(window,"abXef","New title: ")
        ok("picker rename mouse drag edits source without saving")
        window.send(b"\x1b\x1b")
        print(f"PASS ALL {COUNT} end-to-end editing checks", flush=True)
    except BaseException:
        (live.ROOT / "FailedScreen.txt").write_text(window.screen.text())
        print("FAILED editing PTY artifacts:", live.ROOT, flush=True)
        raise
    finally:
        provider.server.shutdown()


if __name__ == "__main__":
    passed = False
    try:
        run()
        passed = True
    finally:
        for window in WINDOWS:
            window.close()
        for pid in live.RuneTerminal.holding(live.HOME, live.PROJECT):
            live.RuneTerminal.stop(pid)
        if passed:
            shutil.rmtree(live.ROOT, ignore_errors=True)
