"""Inside tmux, Rune sends no more colours than the terminal watching can show.

    python3 -I Cli/tests/RuneTmuxColoursPTY.py [path/to/rune]

tmux translates every colour for each terminal attached to it, by what that
terminal says it is. One attached as `xterm` (eight colours; `docker exec -it`
without `-e TERM`) turned Rune's amber 256-colour inks into plain red — the
SeaDrive console, 2026-10-21 — while Rune, reading tmux's own `TERM`
(`tmux-256color`), went on sending 256 colours. Rune now asks tmux what is
attached: watched as `xterm` it picks the nearest of sixteen itself (amber
stays a yellow, `93`); watched as `xterm-256color` it keeps its 256 colours.

Needs tmux; skipped where it is not installed (and on Windows).
"""
import os, re, shutil, subprocess, sys, tempfile, time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import RuneTerminal

BIN = sys.argv[1] if len(sys.argv) > 1 else str(Path(__file__).resolve().parents[2] / "target/debug" / ("rune.exe" if RuneTerminal.WINDOWS else "rune"))
TMUX = shutil.which("tmux")
if RuneTerminal.WINDOWS or not TMUX:
    print("skip: tmux is not installed")
    sys.exit(0)

ROOT = Path(tempfile.mkdtemp(prefix="RuneTmux.", dir=str(Path(__file__).resolve().parents[2] / "target/test-tmp")))
HOME = ROOT / "Home"; HOME.mkdir()
PROJECT = ROOT / "Project"; PROJECT.mkdir()
(HOME / "rune.toml").write_text('[ui]\ntheme = "amber"\n')
SOCKET = f"rune-colours-{os.getpid()}"


def tmux(*args, **kw):
    return subprocess.run([TMUX, "-L", SOCKET, "-f", os.devnull, *args], capture_output=True, text=True, **kw)


def colours_shown_as(client_term):
    """The SGR codes Rune drew its title with, watched by a terminal of `client_term`."""
    # The window starts only once the client is attached: colour is decided
    # as it starts.
    tmux("new-session", "-d", "-x", "120", "-y", "30",
         f"sleep 2; RUNE_HOME={HOME} exec {BIN} -project {PROJECT}")
    client = RuneTerminal.Terminal([TMUX, "-L", SOCKET, "-f", os.devnull, "attach"],
                                   dict(os.environ, TERM=client_term), 30, 120)
    try:
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            client.read()
            pane = tmux("capture-pane", "-e", "-p").stdout
            title = re.search(r"((?:\x1b\[[0-9;]*m)+)Rune CLI", pane)
            if title:
                return title.group(1)
            time.sleep(0.2)
        raise AssertionError(f"Rune never drew its title under a {client_term} client:\n{pane}")
    finally:
        tmux("kill-server")
        if client.alive():
            client.kill()


try:
    narrow = colours_shown_as("xterm")
    assert "38;5;" not in narrow and "38;2;" not in narrow, f"256/true colour sent to an 8-colour client: {narrow!r}"
    assert re.search(r"\[(?:1;)?93m", narrow), f"amber is not a yellow of sixteen: {narrow!r}"
    wide = colours_shown_as("xterm-256color")
    assert "38;5;" in wide, f"a 256-colour client lost its colours: {wide!r}"
    print("tmux colours: ok")
finally:
    tmux("kill-server")
    shutil.rmtree(ROOT, ignore_errors=True)
