"""A window whose session host goes away without a word finds the session's host
again — or starts one — instead of talking to nobody.

    python3 -I Cli/tests/RuneHostRecoveryPTY.py [path/to/rune]

The host is killed outright (as a crash would end it). The window must notice
its connection ended without `host_upgrading`, find no host, start one, and
answer the next prompt through it. Rune 0.1.5 left such a window on a dead
connection for good.
"""
import json, os, shutil, signal, subprocess, sys, tempfile, time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import RuneTerminal

ROOT = Path(tempfile.mkdtemp(prefix="RuneHostRecovery.", dir=str(Path(__file__).resolve().parents[2]/"target/test-tmp")))
HOME = ROOT / "Home"; HOME.mkdir()
PROJECT = ROOT / "Project"; PROJECT.mkdir()
BIN = sys.argv[1] if len(sys.argv) > 1 else str(Path(__file__).resolve().parents[2] / "target/debug" / ("rune.exe" if RuneTerminal.WINDOWS else "rune"))
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


env = dict(os.environ, RUNE_HOME=str(HOME), TERM="xterm-256color", COLORTERM="truecolor")
window = RuneTerminal.Terminal([BIN, "-project", str(PROJECT), "-script", str(HOME / "fixture.json")], env, 34, 120)
output = bytearray()


def drain():
    output.extend(window.read())


def hosts():
    return RuneTerminal.hosts(HOME)


def transcript():
    path = next(HOME.glob("projects/*/sessions/1/session.jsonl"), None)
    return path.read_text() if path else ""


try:
    until(lambda: b"test" in output, "first paint")
    window.write(b"first\r")
    until(lambda: transcript().count('"assistant"') >= 1, "the first answer")
    until(lambda: hosts(), "the session host")
    dead = hosts()[0]
    RuneTerminal.kill(dead)
    until(lambda: [h for h in hosts() if h != dead], "a session host again after the first one died")
    window.write(b"second\r")
    until(lambda: "second" in transcript() and transcript().count('"assistant"') >= 2,
          "the next prompt answered through the new host")
    assert window.alive(), "the window ended"
    print("host recovery: ok")
finally:
    if window.alive():
        window.write(b"\x03\x03")
        if not window.wait(10):
            window.kill()
    for host in hosts():
        RuneTerminal.kill(host)
    shutil.rmtree(ROOT, ignore_errors=True)
