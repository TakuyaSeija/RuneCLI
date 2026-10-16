"""Two windows on one session stay one picture, and neither takes the other down.

    python3 -I Cli/tests/RuneTwoWindowsPTY.py [path/to/rune]

Window A creates session 1 (scripted provider), window B resumes it.
  0. A draws the message that created the session as its own question — that
     first message is stored by A and announced later by the session's host.
  1. What A sends and the answer to it appear in B as they happen.
  2. A window that opens afterwards (C) shows the whole conversation.
  3. Ctrl+O in A (it writes `ui.sidebar` to rune.toml) leaves every window alive
     and the setting flips each time — the press that once ended all processes.
"""
import json, os, re, pty, fcntl, termios, struct, select, shutil, signal, subprocess, sys, tempfile, time
from pathlib import Path

ROOT = Path(tempfile.mkdtemp(prefix="RuneTwoWindows.", dir=str(Path(__file__).resolve().parents[2]/"target/test-tmp")))
HOME = ROOT / "Home"; HOME.mkdir()
PROJECT = ROOT / "Project"; PROJECT.mkdir()
BIN = sys.argv[1] if len(sys.argv) > 1 else str(Path(__file__).resolve().parents[2] / "target/debug/rune")
(HOME / "fixture.json").write_text(json.dumps({"responses": [{"chunks": [{"text": f"Reply number {n}."}]} for n in range(1, 7)]}))
(HOME / "rune.toml").write_text('[providers]\ndefault = "fixture"\n[[providers.list]]\nname = "fixture"\nbase_url = "http://127.0.0.1:1"\napi = "openai"\nauth = { header = "Authorization", prefix = "Bearer " }\n[[providers.list.models]]\nid = "test"\nthinking = false\ncontext = 32768\n')


class Window:
    def __init__(self, args):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 34, 140, 0, 0))
        env = dict(os.environ, RUNE_HOME=str(HOME), TERM="xterm-256color", COLORTERM="truecolor")
        self.proc = subprocess.Popen([BIN, *args], stdin=slave, stdout=slave, stderr=slave, env=env)
        os.close(slave)
        self.out = bytearray()

    def drain(self):
        while select.select([self.master], [], [], 0)[0]:
            try:
                self.out.extend(os.read(self.master, 65536))
            except OSError:
                return

    def send(self, data):
        os.write(self.master, data)

    def alive(self):
        return self.proc.poll() is None

    def shows(self, text):
        return text.encode() in self.out

    def shows_question(self, text):
        """A user block: the `you ·` rule, then the text, as drawn (colour taken out)."""
        plain = re.sub(rb"\x1b\[[0-9;?]*[ -/]*[@-~]", b"", self.out).decode("utf-8", "replace")
        rule = plain.find("you \u00b7")
        return rule >= 0 and text in plain[rule:]


def until(predicate, description, windows, timeout=40):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        for w in windows:
            w.drain()
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError(description)


def sidebar_value():
    for line in (HOME / "rune.toml").read_text().splitlines():
        if line.strip().startswith("sidebar"):
            return line.split("=")[1].split("#")[0].strip()
    return None


def transcript():
    path = next(HOME.glob("projects/*/sessions/1/session.jsonl"), None)
    return path.read_text() if path else ""


def hosts():
    found = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            cmd = (entry / "cmdline").read_bytes()
            environ = (entry / "environ").read_bytes()
        except OSError:
            continue
        if b"-detached-host" in cmd and f"RUNE_HOME={HOME}".encode() in environ:
            found.append(int(entry.name))
    return found


windows = []
try:
    a = Window(["-project", str(PROJECT), "-script", str(HOME / "fixture.json")]); windows.append(a)
    until(lambda: a.shows("test"), "A: first paint", windows)
    a.send(b"first question\r")
    until(lambda: transcript().count('"assistant"') >= 1, "A: first answer stored", windows)
    until(lambda: a.shows_question("first question") and a.shows("Reply number 1."),
          "A: draws the message that created the session, and its answer", windows)
    b = Window(["-project", str(PROJECT), "-resume", "1"]); windows.append(b)
    until(lambda: b.shows("Reply number 1."), "B: shows the answer given before it opened", windows)

    a.send(b"second question\r")
    until(lambda: b.shows("second question") and b.shows("Reply number 2."),
          "B: shows what A sent and the answer to it, live", windows)

    c = Window(["-project", str(PROJECT), "-resume", "1"]); windows.append(c)
    until(lambda: c.shows("first question") and c.shows("Reply number 2."),
          "C: a window opened later shows the whole conversation", windows)

    results = []
    for press in (1, 2):
        before = sidebar_value()
        a.send(b"\x0f")  # Ctrl+O
        time.sleep(2.5)  # several configuration polls
        for w in windows:
            w.drain()
        results.append((before, sidebar_value(), [w.alive() for w in windows]))
    for before, after, alive in results:
        assert all(alive), f"a window ended after Ctrl+O: {alive}"
        assert before != after, f"Ctrl+O did not flip ui.sidebar ({before} -> {after})"
    print("two windows: ok")
finally:
    for w in windows:
        if w.alive():
            try:
                w.send(b"\x03\x03")
                w.proc.wait(timeout=8)
            except Exception:
                w.proc.kill()
    for h in hosts():
        try:
            os.kill(h, signal.SIGKILL)
        except OSError:
            pass
    shutil.rmtree(ROOT, ignore_errors=True)
