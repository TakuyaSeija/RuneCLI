"""A window whose session host goes away without a word finds the session's host
again — or starts one — instead of talking to nobody.

    python3 -I Cli/tests/RuneHostRecoveryPTY.py [path/to/rune]

The host is killed outright (as a crash would end it). The window must notice
its connection ended without `host_upgrading`, find no host, start one, and
answer the next prompt through it. Rune 0.1.5 left such a window on a dead
connection for good.
"""
import json, os, pty, fcntl, termios, struct, select, shutil, signal, subprocess, sys, tempfile, time
from pathlib import Path

ROOT = Path(tempfile.mkdtemp(prefix="RuneHostRecovery.", dir=str(Path(__file__).resolve().parents[2]/"target/test-tmp")))
HOME = ROOT / "Home"; HOME.mkdir()
PROJECT = ROOT / "Project"; PROJECT.mkdir()
BIN = sys.argv[1] if len(sys.argv) > 1 else str(Path(__file__).resolve().parents[2] / "target/debug/rune")
(HOME / "fixture.json").write_text(json.dumps({"responses": [{"chunks": [{"text": "Answered."}]} for _ in range(6)]}))
(HOME / "rune.toml").write_text('[providers]\ndefault = "fixture"\n[[providers.list]]\nname = "fixture"\nbase_url = "http://127.0.0.1:1"\napi = "openai"\nauth = { header = "Authorization", prefix = "Bearer " }\n[[providers.list.models]]\nid = "test"\nthinking = false\ncontext = 32768\n')


def until(predicate, description, timeout=40):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        drain()
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError(description)


master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 34, 120, 0, 0))
env = dict(os.environ, RUNE_HOME=str(HOME), TERM="xterm-256color", COLORTERM="truecolor")
window = subprocess.Popen([BIN, "-project", str(PROJECT), "-script", str(HOME / "fixture.json")],
                          stdin=slave, stdout=slave, stderr=slave, env=env)
os.close(slave)
output = bytearray()


def drain():
    while select.select([master], [], [], 0)[0]:
        try:
            output.extend(os.read(master, 65536))
        except OSError:
            return


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


def transcript():
    path = next(HOME.glob("projects/*/sessions/1/session.jsonl"), None)
    return path.read_text() if path else ""


try:
    until(lambda: b"test" in output, "first paint")
    os.write(master, b"first\r")
    until(lambda: transcript().count('"assistant"') >= 1, "the first answer")
    until(lambda: hosts(), "the session host")
    dead = hosts()[0]
    os.kill(dead, signal.SIGKILL)
    until(lambda: [h for h in hosts() if h != dead], "a session host again after the first one died")
    os.write(master, b"second\r")
    until(lambda: "second" in transcript() and transcript().count('"assistant"') >= 2,
          "the next prompt answered through the new host")
    assert window.poll() is None, "the window ended"
    print("host recovery: ok")
finally:
    if window.poll() is None:
        os.write(master, b"\x03\x03")
        try:
            window.wait(timeout=10)
        except subprocess.TimeoutExpired:
            window.kill()
    for host in hosts():
        os.kill(host, signal.SIGKILL)
    shutil.rmtree(ROOT, ignore_errors=True)
