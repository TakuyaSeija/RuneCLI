"""Real windows against a real HTTP provider — a local stand-in that answers the
way an OpenAI-compatible server does — for what a model call, a tool step and
the screen promise:

    python3 -I Cli/tests/RuneLiveProviderPTY.py [path/to/rune]

  1. A draft holds only what was typed: no conversation, no context, and
     drafts.json is laid out for a person to read.
  2. A 503 that asks for thirty minutes (Retry-After: 1800) is repeated on
     Rune's own curve: the answer is on the screen about 3 s later.
  3. A stream that goes quiet is abandoned after `network.stream_idle_timeout`
     and repeated, its partial answer dropped; thinking or keep-alives that
     keep arriving are never cut, however long they run.
  4. A patch whose old text is not in the file is drawn as its step — what it
     tried, no "failed", no tags — and the model is told the whole notice.
  5. /undo lists the files the session changed; Alt+S shows what undoing one
     puts back; Enter starts a countdown and a second Enter puts the file back
     the way it was before the session, tells the model, and leaves the row at
     +0 −0 — nothing drawn in the conversation; /redo then puts back what the
     undo replaced though the file was edited since, and is used up; while the
     model works it refuses.
  6. AGENTS.md edited by someone else between turns is told to the model once,
     as a <context_update>, and not drawn in the conversation.
  7. A background command that ends while the model is still answering is
     shown at once and read by the model at its next request — once.
  8. A command's output is its lines, not the rows of the box it ran in: the
     model gets a long line whole, and with the side panel closed the opened
     step spans the screen.
  9. Through the host protocol (the GUI's door): a stall limit changed while a
     request waits reaches that request.
 10. /undo puts back one file of a turn that changed two.
 11. A changed limit is told as a <status_update>, not drawn; a new skill as a
     <context_update>; reading it writes capabilities.json laid out.
 12. The stall limit changed in the settings screen reaches a waiting request.
 13. Reading up the conversation: the centred jump hint, Ctrl+↓ and a click on
     it, a Ctrl/Alt wheel notch 3× as far, /help and the side panel leaving
     the reader where they were.
 14. A command's temporary file lands in the session's scratch, and the
     scratch goes when the session closes (window gone, host ended).
 16. The session picker (`-session`) takes a release over live, and so does
     a window opened from it, with what was typed in it.
 17. A release installed over windows on drafts: the empty one moves at once,
     the one with words and a file moves once the hands rest, carrying them
     (and keys typed as the new build starts), and sending them is answered.

The window is read through a small terminal model (`Screen`), not by searching
the bytes it wrote: a row is sent from its first changed cell, so a sentence is
on the screen without ever being in the stream whole. The window's questions
are answered as a terminal answers them (`ESC [ 5 n`, the background colour).
"""
import codecs, collections, http.server, json, os, re, select, shutil, signal
import socket, subprocess, sys, tempfile, threading, time, unicodedata
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import RuneTerminal
from RuneTerminal import WINDOWS, LINUX

TEST_TMP = Path(__file__).resolve().parents[2] / "target" / "test-tmp"
TEST_TMP.mkdir(parents=True, exist_ok=True)
ROOT = Path(tempfile.mkdtemp(prefix="RuneLiveProvider.", dir=TEST_TMP))
HOME = ROOT / "Home"; HOME.mkdir()
PROJECT = ROOT / "Project"; PROJECT.mkdir()
BIN = sys.argv[1] if len(sys.argv) > 1 else str(Path(__file__).resolve().parents[2] / "target/debug" / ("rune.exe" if WINDOWS else "rune"))
assert os.path.realpath(HOME) != os.path.realpath(os.path.expanduser("~/.rune"))


# ─── The provider ────────────────────────────────────────────────────────────

def delta(**fields):
    return {"id": "r", "object": "chat.completion.chunk", "model": "fake-model",
            "choices": [{"index": 0, "delta": fields, "finish_reason": None}]}


def finish(reason):
    return {"id": "r", "object": "chat.completion.chunk", "model": "fake-model",
            "choices": [{"index": 0, "delta": {}, "finish_reason": reason}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16}}


def open_stream(h):
    h.send_response(200)
    h.send_header("Content-Type", "text/event-stream")
    h.send_header("Cache-Control", "no-cache")
    h.end_headers()


def say(h, chunk):
    h.wfile.write(b"data: " + json.dumps(chunk).encode() + b"\n\n")
    h.wfile.flush()


def close_stream(h):
    h.wfile.write(b"data: [DONE]\n\n")
    h.wfile.flush()


def text(answer):
    def run(h, stream):
        if not stream:
            body = json.dumps({"choices": [{"index": 0, "finish_reason": "stop",
                                            "message": {"role": "assistant", "content": answer}}]}).encode()
            h.send_response(200)
            h.send_header("Content-Type", "application/json")
            h.send_header("Content-Length", str(len(body)))
            h.end_headers()
            h.wfile.write(body)
            return
        open_stream(h)
        say(h, delta(role="assistant", content=answer))
        say(h, finish("stop"))
        close_stream(h)
    return run


def tool(name, args):
    def run(h, stream):
        call = {"index": 0, "id": f"call_{name}_{time.monotonic_ns()}", "type": "function",
                "function": {"name": name, "arguments": json.dumps(args)}}
        if not stream:
            message = {"role": "assistant", "content": None,
                       "tool_calls": [{k: v for k, v in call.items() if k != "index"}]}
            body = json.dumps({"choices": [{"index": 0, "finish_reason": "tool_calls", "message": message}]}).encode()
            h.send_response(200)
            h.send_header("Content-Type", "application/json")
            h.send_header("Content-Length", str(len(body)))
            h.end_headers()
            h.wfile.write(body)
            return
        open_stream(h)
        say(h, delta(role="assistant", tool_calls=[call]))
        say(h, finish("tool_calls"))
        close_stream(h)
    return run


def status(code, headers, payload):
    def run(h, stream):
        body = json.dumps(payload).encode()
        h.send_response(code)
        for key, value in headers.items():
            h.send_header(key, value)
        h.send_header("Content-Type", "application/json")
        h.send_header("Content-Length", str(len(body)))
        h.end_headers()
        h.wfile.write(body)
    return run


def stall(first):
    """One chunk, then silence — not a byte — until the client lets go."""
    def run(h, stream):
        open_stream(h)
        say(h, delta(role="assistant", content=first))
        end = time.monotonic() + 120
        while time.monotonic() < end:
            ready, _, _ = select.select([h.connection], [], [], 0.2)
            if ready:
                try:
                    if not h.connection.recv(1, socket.MSG_PEEK):
                        return
                except OSError:
                    return
    return run


def thinking(seconds, every, answer):
    def run(h, stream):
        open_stream(h)
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            say(h, delta(role="assistant", reasoning_content="still weighing it. "))
            time.sleep(every)
        say(h, delta(content=answer))
        say(h, finish("stop"))
        close_stream(h)
    return run


def keepalive(seconds, every, answer):
    def run(h, stream):
        open_stream(h)
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            h.wfile.write(b": keep-alive\n\n")
            h.wfile.flush()
            time.sleep(every)
        say(h, delta(role="assistant", content=answer))
        say(h, finish("stop"))
        close_stream(h)
    return run


class Provider:
    """Answers model calls from `plan`, one behaviour per call; a title job is
    answered on the side. Every call is kept, with when it came."""

    def __init__(self):
        self.plan = collections.deque()
        self.calls = []
        self.lock = threading.Lock()
        provider = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                body = json.dumps({"object": "list", "data": [{"id": "fake-model", "object": "model"}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0))
                body = json.loads(self.rfile.read(length) or b"{}")
                names = [t.get("function", {}).get("name") for t in body.get("tools") or []]
                kind = "title" if names == ["set_title"] else "main"
                with provider.lock:
                    provider.calls.append({"at": time.monotonic(), "kind": kind, "body": body})
                    if kind == "title":
                        answer = tool("set_title", {"title": "Live provider test"})
                    elif provider.plan:
                        answer = provider.plan.popleft()
                    else:
                        answer = text("(nothing planned)")
                try:
                    answer(self, bool(body.get("stream")))
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def main_calls(self):
        with self.lock:
            return [c for c in self.calls if c["kind"] == "main"]

    def queue(self, *answers):
        with self.lock:
            self.plan.extend(answers)


def messages_of(call):
    """What a request put in front of the model after the system prompt (which
    explains every tag, so it names them all), as `role: text` lines."""
    out = []
    for m in call["body"].get("messages", []):
        if m.get("role") == "system":
            continue
        content = m.get("content")
        if isinstance(content, list):
            content = "".join(part.get("text", "") for part in content if isinstance(part, dict))
        out.append(f"{m.get('role')}: {content or ''}")
    return out


# ─── The terminal ────────────────────────────────────────────────────────────

def cells(ch):
    if unicodedata.combining(ch) or unicodedata.category(ch) in ("Mn", "Me", "Cf"):
        return 0
    return 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1


class Screen:
    """What a terminal shows after the bytes so far: cursor moves, erases,
    margins and scrolls, wide characters, and each cell's foreground colour
    (`ink`) — enough to tell a red bullet from a grey one."""

    def __init__(self, rows, cols):
        self.rows, self.cols = rows, cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.inks = [[None] * cols for _ in range(rows)]
        self.fg = None
        self.y = self.x = 0
        self.top, self.bottom = 0, rows
        self.pending = ""
        self.decode = codecs.getincrementaldecoder("utf-8")(errors="replace")

    def feed(self, data):
        s = self.pending + self.decode.decode(data)
        self.pending = ""
        i = 0
        while i < len(s):
            c = s[i]
            if c == "\x1b":
                end = self.sequence_end(s, i)
                if end is None:
                    self.pending = s[i:]
                    return
                self.escape(s[i:end])
                i = end
                continue
            if c == "\r":
                self.x = 0
            elif c == "\n":
                self.line_feed()
            elif c == "\b":
                self.x = max(0, self.x - 1)
            elif ord(c) >= 0x20:
                self.put(c)
            i += 1

    @staticmethod
    def sequence_end(s, i):
        if i + 1 >= len(s):
            return None
        kind = s[i + 1]
        if kind == "[":
            j = i + 2
            while j < len(s) and not ("\x40" <= s[j] <= "\x7e"):
                j += 1
            return j + 1 if j < len(s) else None
        if kind == "]":
            j = i + 2
            while j < len(s):
                if s[j] == "\x07":
                    return j + 1
                if s[j] == "\x1b" and j + 1 < len(s) and s[j + 1] == "\\":
                    return j + 2
                j += 1
            return None
        return i + 2

    def escape(self, seq):
        if not seq.startswith("\x1b["):
            return
        body, final = seq[2:-1], seq[-1]
        private = body.startswith("?")
        nums = [int(p) if p.isdigit() else 0 for p in body.lstrip("?").split(";")] if body else []
        first = nums[0] if nums else 0
        if private:
            return
        if final == "m":
            self.sgr(body)
            return
        if final in "Hf":
            self.y = min(self.rows - 1, max(first, 1) - 1)
            self.x = min(self.cols - 1, max(nums[1] if len(nums) > 1 else 1, 1) - 1)
        elif final == "G":
            self.x = min(self.cols - 1, max(first, 1) - 1)
        elif final == "K":
            row = self.grid[self.y]
            span = range(self.x, self.cols) if first == 0 else range(0, self.x + 1) if first == 1 else range(self.cols)
            for c in span:
                row[c] = " "
                self.inks[self.y][c] = None
        elif final == "J":
            if first == 2 or first == 3:
                self.grid = [[" "] * self.cols for _ in range(self.rows)]
                self.inks = [[None] * self.cols for _ in range(self.rows)]
            elif first == 0:
                for c in range(self.x, self.cols):
                    self.grid[self.y][c] = " "
                    self.inks[self.y][c] = None
                for r in range(self.y + 1, self.rows):
                    self.grid[r] = [" "] * self.cols
                    self.inks[r] = [None] * self.cols
        elif final == "r":
            if len(nums) >= 2 and nums[0]:
                self.top, self.bottom = nums[0] - 1, nums[1]
            else:
                self.top, self.bottom = 0, self.rows
            self.y = self.x = 0
        elif final in "ST":
            by = max(first, 1)
            for grid, blank in ((self.grid, " "), (self.inks, None)):
                band = grid[self.top:self.bottom]
                for _ in range(min(by, len(band))):
                    if final == "S":
                        band.pop(0)
                        band.append([blank] * self.cols)
                    else:
                        band.pop()
                        band.insert(0, [blank] * self.cols)
                grid[self.top:self.bottom] = band
        elif final == "A":
            self.y = max(0, self.y - max(first, 1))
        elif final == "B":
            self.y = min(self.rows - 1, self.y + max(first, 1))
        elif final == "C":
            self.x = min(self.cols - 1, self.x + max(first, 1))
        elif final == "D":
            self.x = max(0, self.x - max(first, 1))

    def sgr(self, body):
        """The foreground colour from a Select Graphic Rendition."""
        nums = [int(p) if p.isdigit() else 0 for p in body.split(";")] if body else [0]
        i = 0
        while i < len(nums):
            n = nums[i]
            if n in (0, 39):
                self.fg = None
            elif n == 38 and i + 4 < len(nums) and nums[i + 1] == 2:
                self.fg = tuple(nums[i + 2:i + 5])
                i += 4
            elif n == 38 and i + 2 < len(nums) and nums[i + 1] == 5:
                self.fg = ("256", nums[i + 2])
                i += 2
            elif 30 <= n <= 37 or 90 <= n <= 97:
                self.fg = ("basic", n)
            i += 1

    def ink(self, y, x):
        return self.inks[y][x]

    def line_feed(self):
        if self.y + 1 >= self.bottom:
            for grid, blank in ((self.grid, " "), (self.inks, None)):
                band = grid[self.top:self.bottom]
                band.pop(0)
                band.append([blank] * self.cols)
                grid[self.top:self.bottom] = band
        else:
            self.y += 1

    def put(self, ch):
        w = cells(ch)
        if w == 0:
            return
        if self.x + w > self.cols:
            return
        row = self.grid[self.y]
        for c in (self.x, self.x + w - 1):
            if row[c] == "\0" and c > 0:
                row[c - 1] = " "
            if c + 1 < self.cols and row[c + 1] == "\0" and row[c] != "\0":
                row[c + 1] = " "
        row[self.x] = ch
        self.inks[self.y][self.x] = self.fg
        if w == 2:
            row[self.x + 1] = "\0"
        self.x += w

    def text(self):
        return "\n".join("".join(ch for ch in row if ch != "\0").rstrip() for row in self.grid)


class Window:
    def __init__(self, args, rows=46, cols=160, bin=None):
        env = dict(os.environ, RUNE_HOME=str(HOME), TERM="xterm-256color", COLORTERM="truecolor")
        self.term = RuneTerminal.Terminal([bin or BIN, *args], env, rows, cols)
        self.pid = self.term.pid
        self.screen = Screen(rows, cols)
        self.tail = b""

    def drain(self):
        while True:
            data = self.term.read()
            if not data:
                return
            seen = self.tail + data
            # Answered as a terminal answers: a status request once read, the
            # background colour when asked.
            asked = seen.count(b"\x1b[5n") - self.tail.count(b"\x1b[5n")
            if asked > 0:
                self.term.write(b"\x1b[0n" * asked)
            if b"\x1b]11;?" in seen and b"\x1b]11;?" not in self.tail:
                self.term.write(b"\x1b]11;rgb:1010/1010/1010\x1b\\")
            self.tail = seen[-8:]
            self.screen.feed(data)

    def send(self, data):
        self.term.write(data)

    def alive(self):
        return self.term.alive()

    def resize(self, rows, cols):
        self.term.resize(rows, cols)

    def type(self, text):
        for ch in text:
            self.send(ch.encode())
            time.sleep(0.004)

    def shows(self, needle):
        return needle in self.screen.text()

    def close(self):
        if self.term.alive():
            self.send(b"\x03\x03")
            if not self.term.wait(5):
                self.term.kill()


def until(predicate, description, window=None, timeout=40):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if window:
            window.drain()
        if predicate():
            return
        time.sleep(0.03)
    raise AssertionError(description + ("\n--- screen ---\n" + window.screen.text() if window else ""))


def session_dir(n):
    return next(HOME.glob(f"projects/*/sessions/{n}"), None)


def transcript(n=1):
    folder = session_dir(n)
    path = folder / "session.jsonl" if folder else None
    if not path or not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def ask(window, provider, prompt, *answers, expect, timeout=40):
    """Queue the model's answers, send `prompt`, wait for `expect` on screen.
    Returns the model calls the prompt caused."""
    before = len(provider.main_calls())
    provider.queue(*answers)
    window.type(prompt)
    window.send(b"\r")
    until(lambda: window.shows(expect), f"{expect!r} never shown", window, timeout)
    return provider.main_calls()[before:]


# The window's spinner while a piece of work runs (a turn, a restore, …).
SPINNER = set("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")


def settled(window, quiet=0.4, timeout=40):
    """Wait until the window shows no work running — no spinner on its last
    rows — for `quiet` seconds. A turn's answer is drawn before the turn has
    let the session go, and `/undo` is refused until it has (the panel says
    "Enter again once the turn is done", as a person would)."""
    still = None
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        window.drain()
        rows = window.screen.text().rstrip("\n").split("\n")[-3:]
        if any(ch in SPINNER for row in rows for ch in row):
            still = None
        elif still is None:
            still = time.monotonic()
        elif time.monotonic() - still >= quiet:
            return
        time.sleep(0.03)
    raise AssertionError("work never settled\n--- screen ---\n" + window.screen.text())


def row_with(window, *needles):
    """The screen row holding every needle, or None."""
    for y, row in enumerate(window.screen.text().split("\n")):
        if all(n in row for n in needles):
            return y
    return None


def column_of(window, y, needle):
    """The cell column `needle` starts at on row `y` (wide characters counted
    as the terminal counts them)."""
    row = "".join(ch if ch != "\0" else "\uffff" for ch in window.screen.grid[y])
    at = row.replace("\uffff", "").find(needle)
    if at < 0:
        return None
    seen = col = 0
    for col, ch in enumerate(window.screen.grid[y]):
        if ch == "\0":
            continue
        if seen == at:
            return col
        seen += 1
    return None


def bullet_ink(window, y):
    """The colour of a step's bullet: on its title row's line or the action
    line just above it."""
    for row in range(y, max(-1, y - 3), -1):
        for x, ch in enumerate(window.screen.grid[row]):
            if ch == "●":
                return window.screen.ink(row, x)
    return None


def inks_of(window, y):
    """A row's text with each visible cell's colour, for a failure message."""
    if y is None:
        return "no such row\n" + window.screen.text()
    return " ".join(f"{ch}{window.screen.ink(y, x)}" for x, ch in enumerate(window.screen.grid[y])
                    if ch.strip() and ch != "\0")


def reddish(ink):
    return (isinstance(ink, tuple) and len(ink) == 3 and all(isinstance(c, int) for c in ink)
            and ink[0] > 140 and ink[0] > ink[1] + 40 and ink[0] > ink[2] + 40)


def wheel(window, up, row, col, ctrl=False, alt=False):
    button = (64 if up else 65) + (16 if ctrl else 0) + (8 if alt else 0)
    window.send(f"\x1b[<{button};{col};{row}M".encode())


def click(window, row, col):
    window.send(f"\x1b[<0;{col};{row}M".encode())
    window.send(f"\x1b[<0;{col};{row}m".encode())


def line_rows(window):
    """Where each `Line NNN` of the long answer is on screen: number → row."""
    out = {}
    for y, row in enumerate(window.screen.text().split("\n")):
        for m in re.finditer(r"Line (\d{3})", row):
            out[int(m.group(1))] = y
    return out


def set_stall_limit(window, value):
    """Through the settings screen, as a person does it: Ctrl+G, the section
    bar to Runtime, down to `Stall limit`, Enter, the value, Enter."""
    window.send(b"\x07")
    until(lambda: window.shows("/ SETTINGS"), "settings opened", window, 10)
    row = row_with(window, "Runtime")
    assert row is not None, window.screen.text()
    click(window, row + 1, column_of(window, row, "Runtime") + 1)
    until(lambda: window.shows("Overview / Runtime") and window.shows("Stall limit"), "Runtime settings", window, 10)
    row = row_with(window, "Stall limit")
    click(window, row + 1, column_of(window, row, "Stall limit") + 1)
    until(lambda: window.shows("Overview / Runtime / Stall limit"), "the stall editor", window, 10)
    window.send(b"\x15")
    window.type(value)
    window.send(b"\r")
    def seconds(text):
        units = {"ms": 0.001, "s": 1, "m": 60, "h": 3600}
        return sum(float(n) * units[u] for n, u in re.findall(r"(\d+(?:\.\d+)?)(ms|s|m|h)", text))

    def saved():
        m = re.search(r'(?m)^stream_idle_timeout\s*=\s*"([^"]*)"', (HOME / "rune.toml").read_text())
        return m and seconds(m.group(1)) == seconds(value)
    until(saved, f"stall limit {value} saved", window, 10)
    for _ in range(4):
        if not window.shows("/ SETTINGS"):
            break
        window.send(b"\x1b")
        time.sleep(0.4)
        window.drain()
    until(lambda: not window.shows("/ SETTINGS"), "back to the conversation", window, 10)


# ─── The cases ───────────────────────────────────────────────────────────────

def main():
    provider = Provider()
    (HOME / "rune.toml").write_text(
        "[providers]\ndefault = \"fake\"\n"
        "[[providers.list]]\nname = \"fake\"\n"
        f"base_url = \"http://127.0.0.1:{provider.port}/v1\"\n"
        "auth = { header = \"Authorization\", prefix = \"Bearer \" }\n"
        "[[providers.list.models]]\nid = \"fake-model\"\nthinking = false\ncontext = 65536\n"
        "[network]\nstream_idle_timeout = \"2s\"\n"
        "[host]\nidle_lifetime = \"3s\"\n"
    )
    (PROJECT / "AGENTS.md").write_text("Rule one: be brief.\n")
    window = Window(["-project", str(PROJECT)])
    passed = []
    skipped = []
    try:
        until(lambda: window.shows("fake-model"), "first paint", window)

        # 1. A draft is only what was typed.
        window.type("hello draft")
        drafts = HOME / "drafts.json"
        until(lambda: drafts.exists() and "hello draft" in drafts.read_text(), "draft saved", window)
        laid_out = drafts.read_text()
        assert "\n  " in laid_out and laid_out.endswith("\n"), laid_out
        assert not list(HOME.glob("projects/*/sessions/*/session.jsonl")), "a draft wrote a conversation"
        assert "Rule one" not in laid_out and "memory_recall" not in laid_out, "context in a draft"
        window.send(b"\x15")
        until(lambda: "hello draft" not in drafts.read_text(), "draft cleared", window)
        passed.append("draft holds only what was typed; drafts.json laid out")

        # 2. Retry-After: 1800 is not what is waited out.
        calls = ask(window, provider, "first question",
                    status(503, {"Retry-After": "1800"}, {"error": {"message": "overloaded, come back later"}}),
                    text("Answer after retry."), expect="Answer after retry.")
        assert len(calls) == 2, calls
        waited = calls[1]["at"] - calls[0]["at"]
        assert 2.5 <= waited <= 8, f"waited {waited:.1f}s"
        passed.append(f"503 + Retry-After 1800 repeated after {waited:.1f}s")

        # 3a. A stream gone quiet is abandoned after the stall limit (2 s here).
        calls = ask(window, provider, "second question",
                    stall("Partial words that must go"), text("Answer after stall."),
                    expect="Answer after stall.")
        assert len(calls) == 2, calls
        gap = calls[1]["at"] - calls[0]["at"]
        assert 4.0 <= gap <= 10, f"stall + wait took {gap:.1f}s"
        until(lambda: not window.shows("Partial words"), "the partial answer stayed", window, 5)
        said = [r for r in transcript() if r.get("role") == "assistant"]
        assert not any("Partial words" in (r.get("content") or "") for r in said), said
        passed.append(f"silent stream abandoned and repeated after {gap:.1f}s, partial dropped")

        # 3b. Thinking that keeps arriving is never cut, however long.
        calls = ask(window, provider, "third question",
                    thinking(6, 0.5, "Answer after long thinking."), expect="Answer after long thinking.")
        assert len(calls) == 1, f"{len(calls)} calls: thinking was cut"
        # 3c. So are keep-alive comments.
        calls = ask(window, provider, "fourth question",
                    keepalive(5, 0.5, "Answer after keep-alives."), expect="Answer after keep-alives.")
        assert len(calls) == 1, f"{len(calls)} calls: keep-alives were cut"
        passed.append("6 s of thinking and 5 s of keep-alives under a 2 s limit: one call each")

        # 4. A failed patch: what it tried, red, no "failed", no tags.
        window.type("/mode auto")
        window.send(b"\r")
        time.sleep(0.5)
        ask(window, provider, "create notes",
            tool("create_file", {"action": "write the notes", "path": "notes.txt", "text": "one\n"}),
            text("Created notes."), expect="Created notes.")
        assert (PROJECT / "notes.txt").read_text() == "one\n"
        calls = ask(window, provider, "patch it",
                    tool("patch_file", {"action": "change it", "path": "notes.txt",
                                        "old_str": "zero", "new_str": "two"}),
                    text("That did not match."), expect="That did not match.")
        shown = window.screen.text()
        step = next((line for line in shown.splitlines() if "notes.txt" in line and "Edit" in line), "")
        assert "−1 +1" in step, shown
        assert "failed" not in shown.lower().replace("failed_", ""), shown
        assert "tool_notice" not in shown, shown
        told = "\n".join(messages_of(calls[-1]))
        assert "<tool_notice>" in told and "Exact match not found" in told, told
        assert (PROJECT / "notes.txt").read_text() == "one\n"
        # Red is the only sign of the failure; the reason waits for the step
        # to be opened, and comes without tags.
        failed = row_with(window, "notes.txt", "−1 +1")
        assert reddish(bullet_ink(window, failed)), inks_of(window, failed - 1)
        assert "Exact match not found" not in shown, "the reason shown on a closed step"
        window.send(b"\x12")  # ctrl+r: open every step
        until(lambda: window.shows("Exact match not found"), "the reason in the opened step", window, 10)
        assert "tool_notice" not in window.screen.text()
        window.send(b"\x12")
        until(lambda: not window.shows("Exact match not found"), "the step closed again", window, 10)
        # A create that never ran: +0, red, no word, no tag.
        ask(window, provider, "create it wrong",
            tool("create_file", {"action": "write it", "path": "bad.txt"}),
            text("The create was refused."), expect="The create was refused.")
        refused = row_with(window, "bad.txt", "+0")
        assert refused is not None, window.screen.text()
        assert reddish(bullet_ink(window, refused)), inks_of(window, refused - 1)
        assert "failed" not in window.screen.text().lower().replace("failed_", "")
        assert not (PROJECT / "bad.txt").exists()
        passed.append("failed patch 'notes.txt −1 +1' and refused create 'bad.txt +0': red, untagged, "
                      "reason only when opened; model told the notice")

        # 5. /undo: diff, countdown, restore, told to the model, nothing drawn.
        ask(window, provider, "patch for real",
            tool("patch_file", {"action": "change it", "path": "notes.txt", "old_str": "one", "new_str": "two"}),
            text("Patched."), expect="Patched.")
        assert (PROJECT / "notes.txt").read_text() == "two\n"
        window.type("/undo")
        window.send(b"\r")
        # The session created notes.txt: all of it is new, and undo removes it.
        until(lambda: window.shows("UNDO") and row_with(window, "notes.txt", "+1", "[undo]") is not None,
              "undo panel", window)
        window.send(b"\x1bs")  # alt+s: the diff
        until(lambda: window.shows("alt+s back to the list") and window.shows("+ two"), "the file's diff", window)
        window.send(b"\x1b")
        until(lambda: window.shows("alt+s show diff"), "back to the list", window)
        window.send(b"\r")
        until(lambda: window.shows("Press Enter within"), "the countdown", window)
        time.sleep(3.5)  # ran out: Enter starts it again rather than restoring
        until(lambda: not window.shows("Press Enter within"), "the countdown ended", window)
        window.send(b"\r")
        until(lambda: window.shows("Press Enter within"), "a fresh countdown", window)
        assert (PROJECT / "notes.txt").read_text() == "two\n", "an Enter after the countdown restored"
        window.send(b"\r")
        until(lambda: not (PROJECT / "notes.txt").exists(), "back to before the session: no file", window)
        until(lambda: row_with(window, "notes.txt", "+0 −0") is not None, "the row stays at zero", window)
        window.send(b"\x1b")
        until(lambda: not window.shows("alt+s show diff"), "undo closed", window)
        assert not window.shows("Files_Restored"), window.screen.text()
        # /redo: the file edited after the undo; redo still puts back what the
        # undo replaced, and is used up.
        (PROJECT / "notes.txt").write_text("edited after the undo\n")
        window.type("/redo")
        window.send(b"\r")
        until(lambda: row_with(window, "notes.txt", "[redo]") is not None, "redo lists notes.txt", window)
        window.send(b"\x1bs")
        until(lambda: window.shows("− edited after the undo") and window.shows("+ two"), "the redo diff", window)
        window.send(b"\x1b")
        until(lambda: window.shows("alt+s show diff"), "back to the redo list", window)
        window.send(b"\r")
        until(lambda: window.shows("to redo notes.txt"), "redo countdown", window)
        window.send(b"\r")
        until(lambda: (PROJECT / "notes.txt").read_text() == "two\n", "redone", window)
        until(lambda: window.shows("Nothing to redo: undo a file first"), "redo used up", window)
        window.send(b"\x1b")
        until(lambda: not window.shows("alt+s show diff"), "redo closed", window)
        calls = ask(window, provider, "what now", text("I will read notes again."),
                    expect="I will read notes again.")
        told = "\n".join(messages_of(calls[-1]))
        assert "restored notes.txt to how it was before this session" in told, told
        assert "redid their undo of notes.txt" in told, told
        # Refused while the model works.
        provider.queue(thinking(5, 0.5, "Done weighing."))
        window.type("weigh it")
        window.send(b"\r")
        calls_before = len(provider.main_calls())
        until(lambda: len(provider.main_calls()) > calls_before - 1 and window.shows("Thinking"),
              "turn started", window)
        window.type("/undo")
        window.send(b"\r")
        until(lambda: window.shows("UNDO"), "undo panel while busy", window)
        window.send(b"\r")
        window.send(b"\r")
        until(lambda: window.shows("Not done: the session is working"), "restore refused while busy", window, 10)
        assert (PROJECT / "notes.txt").read_text() == "two\n"
        window.send(b"\x1b")
        until(lambda: window.shows("Done weighing."), "busy turn ended", window)
        passed.append("/undo diff, countdown (an Enter after it restarts it), restore to before the session, "
                      "told, not drawn; /redo over a later edit, used up, told; refused while busy")

        # 6. AGENTS.md changed elsewhere: one <context_update>, shown.
        (PROJECT / "AGENTS.md").write_text("Rule one: be brief.\nRule two: be thorough.\n")
        calls = ask(window, provider, "next", text("Will re-read the rules."), expect="Will re-read the rules.")
        told = messages_of(calls[-1])
        updates = [m for m in told if m.startswith("user: <context_update>")]
        assert len(updates) == 1 and "AGENTS.md changed since you read it" in updates[0], told
        time.sleep(1)
        assert not window.shows("Context_Update"), window.screen.text()
        calls = ask(window, provider, "again", text("Fine."), expect="Fine.")
        assert sum(m.startswith("user: <context_update>") for m in messages_of(calls[-1])) == 1, "told twice"
        passed.append("AGENTS.md change told once as <context_update>, not drawn")

        # 7. A background task that ends mid-answer: shown at once, read next call.
        before = len(provider.main_calls())
        provider.queue(
            tool("terminal_use", {"action": "run it in the background", "command": "sleep 2; echo bg-done",
                                  "run_in_background": True}),
            thinking(7, 0.5, "Still answering."),
            text("Saw it finish."),
        )
        window.type("start the job")
        window.send(b"\r")
        until(lambda: window.shows("System_Notification"), "notification shown", window, 20)
        assert not window.shows("Still answering."), "shown only after the answer"
        until(lambda: window.shows("Saw it finish."), "the model read it without a new prompt", window, 30)
        calls = provider.main_calls()[before:]
        assert len(calls) == 3, len(calls)
        notified = [m for m in messages_of(calls[2]) if m.startswith("user: <system_notification>")]
        assert len(notified) == 1, messages_of(calls[2])
        calls = ask(window, provider, "and now", text("Noted."), expect="Noted.")
        assert sum(m.startswith("user: <system_notification>") for m in messages_of(calls[-1])) == 1, "told twice"
        passed.append("background end shown mid-answer, read at the next call, once")

        # 8. A command's output is its lines, not the rows of the box it ran in:
        #    with the side panel closed, the opened step is as wide as the
        #    screen, and the model got the line whole.
        calls = ask(window, provider, "print a long line",
                    tool("terminal_use", {"action": "print a long line",
                                          "command": "printf 'x%.0s' $(seq 1 300); echo"}),
                    text("Printed."), expect="Printed.")
        told = "\n".join(messages_of(calls[-1]))
        assert "x" * 300 in told, "the model got the line cut into rows"
        if window.shows("TokOut"):
            window.send(b"\x0f")  # ctrl+o: the side panel closes
            until(lambda: not window.shows("TokOut"), "side panel closed", window)
        window.send(b"\x12")  # ctrl+r: every step opens
        widest = lambda: max((len(m.group(0)) for m in re.finditer(r"x{20,}", window.screen.text())), default=0)
        until(lambda: widest() > 120, "the opened step stays as narrow as the box it ran in", window, 10)
        passed.append(f"long output kept whole for the model; opened step {widest()} columns wide, panel off")
        window.send(b"\x12")  # ctrl+r: the steps close again

        # 10. /undo puts back one file a turn changed, and only that one.
        (PROJECT / "a.txt").write_text("alpha\n")
        (PROJECT / "b.txt").write_text("beta\n")
        ask(window, provider, "patch both",
            tool("patch_file", {"action": "a", "path": "a.txt", "old_str": "alpha", "new_str": "ALPHA"}),
            tool("patch_file", {"action": "b", "path": "b.txt", "old_str": "beta", "new_str": "BETA"}),
            text("Both patched."), expect="Both patched.")
        settled(window)
        window.type("/undo")
        window.send(b"\r")
        until(lambda: window.shows("UNDO") and window.shows("a.txt") and window.shows("b.txt"),
              "undo panel with both files", window)
        target = row_with(window, "a.txt", "[undo]")
        chosen = row_with(window, "›", "[undo]")
        for _ in range(abs(target - chosen)):
            window.send(b"\x1b[B" if target > chosen else b"\x1b[A")
        until(lambda: row_with(window, "›", "a.txt") is not None, "a.txt chosen", window)
        window.send(b"\r")
        window.send(b"\r")
        back = lambda: [(PROJECT / "a.txt").read_text() == "alpha\n", (PROJECT / "b.txt").read_text() == "beta\n"]
        until(lambda: any(back()), "one file put back", window)
        time.sleep(1)
        assert back() == [True, False], back()
        if window.shows("UNDO"):
            window.send(b"\x1b")
        passed.append("/undo put back one file of a two-file turn, the other untouched")

        # 11. A changed limit is told as a <status_update> and shown; a new
        #     skill as a <context_update>; reading it writes capabilities.json
        #     laid out for a person.
        config = (HOME / "rune.toml").read_text()
        if re.search(r"(?m)^command_timeout\s*=", config):
            config = re.sub(r"(?m)^command_timeout\s*=.*$", 'command_timeout = "77s"', config)
        elif re.search(r"(?m)^\[terminal\]\s*$", config):
            config = re.sub(r"(?m)^\[terminal\]\s*$", '[terminal]\ncommand_timeout = "77s"', config, count=1)
        else:
            config += '\n[terminal]\ncommand_timeout = "77s"\n'
        (HOME / "rune.toml").write_text(config)
        time.sleep(2)
        skill = PROJECT / ".rune/skills/brief"
        skill.mkdir(parents=True)
        (skill / "SKILL.md").write_text("---\nname: brief\ndescription: Answer in one line.\n---\nAnswer in one line.\n")
        calls = ask(window, provider, "anything new?",
                    tool("inspect_file", {"action": "read the skill", "path": ".rune/skills/brief/SKILL.md"}),
                    text("Read the brief skill."), expect="Read the brief skill.")
        told = messages_of(calls[0])
        status_rows = [m for m in told if m.startswith("user: <status_update>")]
        assert status_rows and "command timeout: 77s" in status_rows[-1], told
        context_rows = [m for m in told if m.startswith("user: <context_update>")]
        assert context_rows and "skill" in context_rows[-1].lower(), told
        assert not window.shows("Status_Update"), window.screen.text()
        caps = session_dir(1) / "capabilities.json"
        until(lambda: caps.exists(), "capabilities.json written", window, 10)
        laid = caps.read_text()
        assert "\n  " in laid and laid.endswith("\n"), laid
        passed.append("limit change told as <status_update>, not drawn; new skill as <context_update>; "
                      "capabilities.json laid out")

        # 12. The stall limit changed in the settings screen reaches a request
        #     already waiting.
        set_stall_limit(window, "5m")
        before = len(provider.main_calls())
        provider.queue(stall("Waiting..."), text("Answer after the panel's limit."))
        window.type("wait here")
        window.send(b"\r")
        until(lambda: len(provider.main_calls()) > before, "the request went out", window, 20)
        time.sleep(3)  # past the old 2 s, under 5 m: still waiting
        assert len(provider.main_calls()) == before + 1, "repeated under a 5 m limit"
        changed = time.monotonic()
        set_stall_limit(window, "1s")
        until(lambda: window.shows("Answer after the panel's limit."), "the waiting request took it", window, 20)
        took = provider.main_calls()[before + 1]["at"] - changed
        assert took <= 8, f"{took:.1f}s"
        passed.append(f"stall limit set in Settings mid-request: repeated {took:.1f}s after the change")

        # 13. Reading up the conversation: the jump hint, Ctrl+↓, a click on
        #     the hint, Ctrl+wheel, an overlay and the side panel.
        hint = "Press Ctrl+↓ Jump to latest"
        lines = "\n".join(f"- Line {i:03}" for i in range(1, 151))
        ask(window, provider, "list lines", text(lines + "\n\nEnd of lines."), expect="End of lines.")
        assert not window.shows(hint), "the hint at the live end"
        over = (10, 30)
        for _ in range(6):
            wheel(window, True, *over)
            time.sleep(0.15)
        until(lambda: window.shows(hint), "the hint once scrolled up", window, 5)
        y = row_with(window, hint)
        left = column_of(window, y, hint)
        right = window.screen.cols - left - len(hint)
        assert abs(left - right) <= 2, f"not centred: {left} | {right}"
        assert y >= window.screen.rows // 2, f"hint on row {y}"
        window.send(b"\x1b[1;5B")  # ctrl+↓
        until(lambda: not window.shows(hint) and window.shows("End of lines."), "ctrl+↓ jumped", window, 5)
        for _ in range(6):
            wheel(window, True, *over)
            time.sleep(0.15)
        until(lambda: window.shows(hint), "the hint again", window, 5)
        y = row_with(window, hint)
        click(window, y + 1, column_of(window, y, hint) + 8)
        until(lambda: not window.shows(hint) and window.shows("End of lines."), "a click on the hint jumped", window, 5)

        # 2B: a notch is two rows; a Ctrl or Alt notch goes three times as far
        #     (owner, 2026-10-09: 2 and 6, no acceleration).
        for _ in range(12):
            wheel(window, True, *over)
            time.sleep(0.15)
        time.sleep(0.5)
        window.drain()

        def moved(**held):
            before = line_rows(window)
            wheel(window, True, *over, **held)
            time.sleep(0.6)
            window.drain()
            after = line_rows(window)
            shared = [n for n in before if n in after]
            assert shared, "no line on screen before and after"
            return after[shared[0]] - before[shared[0]]

        plain = moved()
        fast = moved(ctrl=True)
        alt = moved(alt=True)
        assert plain == 2 and fast == 6 and alt == fast, (plain, fast, alt)

        # 2C: an overlay read and closed leaves the conversation where it was.
        reading = line_rows(window)
        window.type("/help")
        window.send(b"\r")
        until(lambda: window.shows("COMMANDS"), "help open", window, 5)
        for _ in range(4):
            wheel(window, True, *over)
            time.sleep(0.15)
        window.send(b"\x1b")
        until(lambda: not window.shows("COMMANDS"), "help closed", window, 5)
        time.sleep(0.3)
        window.drain()
        assert line_rows(window) == reading, (reading, line_rows(window))

        # 2C: the side panel opening and closing keeps the paragraph read.
        top = min(reading)
        window.send(b"\x0f")  # ctrl+o: on
        until(lambda: window.shows("TokOut"), "side panel open", window, 5)
        time.sleep(0.3)
        window.drain()
        assert top in line_rows(window), f"Line {top:03} left the screen when the panel opened"
        window.send(b"\x0f")  # off
        until(lambda: not window.shows("TokOut"), "side panel closed", window, 5)
        time.sleep(0.3)
        window.drain()
        assert top in line_rows(window), f"Line {top:03} left the screen when the panel closed"
        window.send(b"\x1b[1;5B")
        passed.append(f"jump hint centred, Ctrl+↓ and a click jump; notch {plain} rows, Ctrl/Alt {fast}; "
                      "help and the side panel leave the reader in place")

        # 14 (first half). A command's temporary file lands in the session's
        # own scratch.
        ask(window, provider, "make a temp file",
            tool("terminal_use", {"action": "write a temp file", "command": "echo kept > \"$TMPDIR/probe.txt\""}),
            text("Temp file written."), expect="Temp file written.")
        scratch = next(HOME.glob("projects/*/scratch/1"), None)
        assert scratch and (scratch / "probe.txt").read_text() == "kept\n", scratch
    finally:
        window.close()

    # 14. The window gone and its host ended (idle 3 s here), the session is
    #     closed: its scratch folder goes, and nothing beside it.
    sibling = scratch.parent / "999999"
    sibling.mkdir()
    (sibling / "keep").write_text("another session's")
    until(lambda: not scratch.exists(), "the closed session's scratch is still there", None, 30)
    assert (sibling / "keep").read_text() == "another session's"
    passed.append("window closed, host ended: the session's scratch went, a sibling's stayed")

    # 9. The host protocol: a limit changed while a request waits reaches it.
    host = subprocess.Popen([BIN, "-host", "-db", str(HOME / "rune.db")], stdin=subprocess.PIPE,
                            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                            env=dict(os.environ, RUNE_HOME=str(HOME)), text=True, bufsize=1)
    events = collections.deque()

    def read():
        for line in host.stdout:
            try:
                events.append(json.loads(line))
            except ValueError:
                pass
    threading.Thread(target=read, daemon=True).start()

    def send(command):
        host.stdin.write(json.dumps(command) + "\n")
        host.stdin.flush()

    def wait_for(test, description, timeout=30):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            for e in list(events):
                if test(e):
                    return e
            time.sleep(0.02)
        raise AssertionError(description + ": " + json.dumps(list(events)[-10:]))

    try:
        other = ROOT / "Other"; other.mkdir()
        send({"cmd": "session.open", "id": 1, "project": str(other), "fresh": True})
        opened = wait_for(lambda e: e.get("t") == "ack" and e.get("id") == 1, "open")
        session = opened["data"]["session"] if "session" in opened.get("data", {}) else opened["data"]["id"]
        send({"cmd": "settings.set", "id": 2, "session": session,
              "key": "config.network.stream_idle_timeout", "value": "5m"})
        wait_for(lambda e: e.get("t") == "ack" and e.get("id") == 2, "limit raised")
        before = len(provider.main_calls())
        provider.queue(stall("Waiting..."), text("Answer after the new limit."))
        send({"cmd": "prompt", "id": 3, "session": session, "text": "wait for it"})
        until(lambda: len(provider.main_calls()) > before, "the request went out", None, 20)
        time.sleep(2)
        changed = time.monotonic()
        send({"cmd": "settings.set", "id": 4, "session": session,
              "key": "config.network.stream_idle_timeout", "value": "1s"})
        wait_for(lambda e: e.get("t") == "text" and "Answer after the new limit." in e.get("delta", ""),
                 "the waiting request took the new limit", 20)
        calls = provider.main_calls()[before:]
        assert len(calls) == 2, len(calls)
        took = calls[1]["at"] - changed
        assert took <= 6, f"the new limit reached the waiting request only after {took:.1f}s"
        passed.append(f"stall limit lowered mid-request: repeated {took:.1f}s after the change")
    finally:
        host.terminate()
        try:
            host.wait(5)
        except subprocess.TimeoutExpired:
            host.kill()

    # 16-17 replace the running build in place, which Windows cannot do
    # (`platform::REPLACES_IN_PLACE`: a newer build is taken at the next start).
    if RuneTerminal.REPLACES_IN_PLACE:
        # 16. The session picker takes a release over live: the build it runs from
        #     replaced on disk, the same process `exec`s the new one and the list
        #     is back.
        installed = ROOT / "bin" / "rune"
        installed.parent.mkdir()
        shutil.copy2(BIN, installed)
        picker = Window(["-session", "-project", str(PROJECT)], bin=str(installed))
        try:
            until(lambda: picker.shows("Live provider test"), "the session list", picker, 20)
            before = RuneTerminal.running_image(picker.pid)
            staged = installed.with_name("rune.new")
            shutil.copy2(BIN, staged)
            os.rename(staged, installed)
            new = os.stat(installed).st_ino
            assert new != before
            until(lambda: RuneTerminal.running_image(picker.pid) == new,
                  "the picker never took the release over", picker, 20)
            picker.screen = Screen(picker.screen.rows, picker.screen.cols)
            until(lambda: picker.shows("Live provider test"), "the list after the take-over", picker, 20)
            assert picker.alive(), "the picker exited"
            # 16b. A window opened from the picker keeps what is typed while it
            #      opens, and takes the next release over with it: the keys the
            #      picker was given are not counted as the window's (0.3.8 kept
            #      such a window on the old build for good, and lost the keys
            #      typed between the choice and the window).
            for key in (b"\x1b[B", b"\x1b[A"):
                picker.send(key)
                time.sleep(0.2)
            picker.send(b"\r")
            # Typed at once, while the session's host is still starting: these
            # keys belong to the window that opens (0.3.8 lost them).
            picker.type("typed after picking")
            stored = lambda: any("typed after picking" in c.read_text()
                                 for c in HOME.glob("projects/*/sessions/*/composer.json"))
            until(stored, "the words typed during the opening reached the session's composer", picker, 30)
            time.sleep(3.5)  # hands at rest
            before = RuneTerminal.running_image(picker.pid)
            staged = installed.with_name("rune.new")
            shutil.copy2(BIN, staged)
            os.rename(staged, installed)
            new = os.stat(installed).st_ino
            assert new != before
            until(lambda: RuneTerminal.running_image(picker.pid) == new,
                  "the window opened from the picker never took the release over", picker, 30)
            # The new build may have drawn its first frame already: a fresh
            # screen model sees it only when the whole screen is drawn again.
            picker.screen = Screen(picker.screen.rows, picker.screen.cols)
            picker.send(b"\x0c")  # ctrl+l
            until(lambda: picker.shows("typed after picking"), "the words carried over", picker, 20)
            assert picker.alive(), "the window exited"
            passed.append("session picker took a release installed under it over: same pid, new build, list back; "
                          "a window opened from it took the next one over with its words")
        finally:
            picker.close()

        # 17. A release installed while windows sit on drafts (owner, 2026-10-09):
        #     what is typed does not hold a window on the old build. An empty
        #     draft takes the release over at once; one with words and a file
        #     attached takes it over too, once the hands rest (3 s) — never while
        #     they type — carrying its words, its attachment and its draft entry.
        #     A key typed while the new build starts is not lost, and the draft
        #     sent after it is answered.
        installed = ROOT / "bin2" / "rune"
        installed.parent.mkdir()
        shutil.copy2(BIN, installed)
        typed_dir = ROOT / "DraftTyped"; typed_dir.mkdir()
        empty_dir = ROOT / "DraftEmpty"; empty_dir.mkdir()
        note = typed_dir / "release-notes.txt"
        note.write_text("notes the draft carries\n")
        typed = Window(["-project", str(typed_dir)], bin=str(installed))
        empty = Window(["-project", str(empty_dir)], bin=str(installed))
        try:
            for w in (typed, empty):
                until(lambda: w.shows("fake-model"), "draft window paints", w, 20)
            typed.send(b"\x1b[200~" + str(note).encode() + b"\x1b[201~")
            until(lambda: typed.shows("release-notes.txt"), "the file attached", typed)
            typed.type("words typed before the release")
            drafts = HOME / "drafts.json"
            until(lambda: "words typed before the release" in drafts.read_text(), "draft saved", typed)
            staged = installed.with_name("rune.new")
            shutil.copy2(BIN, staged)
            os.rename(staged, installed)
            new = os.stat(installed).st_ino
            exe = lambda w: RuneTerminal.running_image(w.pid)
            old = exe(typed)
            # Waiting for the empty window may exceed the typed window's 3 s
            # hand-rest interval (preflight/copy under load). Keep its hands active
            # during that wait: otherwise a correct idle handover makes the next
            # assertion falsely report that it moved *while* being typed into.
            last_touch = [time.monotonic()]
            typed.send(b"\x05")

            def empty_has_taken_over():
                typed.drain()
                now = time.monotonic()
                if now - last_touch[0] >= 0.5:
                    typed.send(b"\x05")
                    last_touch[0] = now
                return exe(empty) == new

            until(empty_has_taken_over, "the empty draft never took the release over", empty, 20)

            # Hands still typing: no take-over for as long as the keys come.
            for ch in " and more":
                typed.type(ch)
                resting = time.monotonic()
                time.sleep(0.7)
                assert exe(typed) == old, "the window handed over while being typed into"
            # The moment the new build is in place, type: the keys wait in the
            # terminal for it, and it reads them.
            deadline = time.monotonic() + 20
            while exe(typed) != new and time.monotonic() < deadline:
                time.sleep(0.005)
            rested = time.monotonic() - resting
            assert exe(typed) == new, "the window with words never took the release over"
            assert rested >= 2.5, f"handed over {rested:.1f}s after the last key"
            typed.type("+late")
            typed.screen = Screen(typed.screen.rows, typed.screen.cols)
            until(lambda: typed.shows("words typed before the release and more+late"),
                  "the words, and the keys typed as the new build started", typed, 20)
            until(lambda: typed.shows("release-notes.txt"), "the attachment after the take-over", typed)
            until(lambda: "words typed before the release and more+late" in drafts.read_text(),
                  "the draft entry after the take-over", typed)

            before = len(provider.main_calls())
            provider.queue(text("Answer to the words typed before."))
            typed.send(b"\r")
            until(lambda: typed.shows("Answer to the words typed before."),
                  "the draft sent after a release was never answered", typed, 40)
            told = "\n".join(messages_of(provider.main_calls()[before]))
            assert "words typed before the release and more+late" in told, told
            assert "notes the draft carries" in told, told
            passed.append(f"release over drafts: empty one moved at once; typed one waited for the "
                          f"hands to rest ({rested:.1f}s), moved with its words, attachment and late keys, "
                          f"and was answered")
        finally:
            typed.close()
            empty.close()

    else:
        skipped.append("16-17 release taken over live: Windows replaces no running program")

    for line in passed:
        print("ok  ", line)
    for line in skipped:
        print("skip", line)
    print(f"PASS: {len(passed)} cases")


if __name__ == "__main__":
    passed_all = False
    try:
        main()
        passed_all = True
    finally:
        # Hosts this test started run detached; they end with their home.
        for pid in RuneTerminal.holding(HOME, PROJECT):
            RuneTerminal.stop(pid)
        # A failed run keeps its home: the session's own files are the
        # evidence of what the windows and the host did.
        if passed_all:
            shutil.rmtree(ROOT, ignore_errors=True)
        else:
            print(f"kept for inspection: {ROOT}", file=sys.stderr)
