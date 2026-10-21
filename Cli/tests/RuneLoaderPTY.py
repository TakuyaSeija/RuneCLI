"""Rune started by naming it to the dynamic loader opens sessions like any Rune.

    python3 -I Cli/tests/RuneLoaderPTY.py [path/to/rune]

How a build made for a newer glibc than a system has is run there:
`ld-linux-x86-64.so.2 --library-path <newer glibc> ./rune`. The kernel then
runs the loader, and Rune 0.7.7 started its session host as the loader with
Rune's arguments — `-detached-host: cannot open shared object file`, exit 127,
no session (SeaDrive console, 2026-10-20). Here the window is started through
this system's own loader, with a `--library-path` naming the folder its glibc
is in (the loader's own answer to `--list`): the prompt must be answered
through a session host started the same way — the loader, the same options,
then Rune.

Linux only, and only a dynamically linked build: a static one has no loader.
"""
import json, os, re, shutil, subprocess, sys, tempfile, time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import RuneTerminal

BIN = sys.argv[1] if len(sys.argv) > 1 else str(Path(__file__).resolve().parents[2] / "target/debug" / ("rune.exe" if RuneTerminal.WINDOWS else "rune"))
if not RuneTerminal.LINUX:
    print("skip: a loader is named only on Linux")
    sys.exit(0)


def interpreter(path):
    """The loader an ELF program names (PT_INTERP), or None when static."""
    data = Path(path).read_bytes()
    if data[:4] != b"\x7fELF" or data[4] != 2:
        return None
    end = "<" if data[5] == 1 else ">"
    import struct
    phoff, = struct.unpack_from(end + "Q", data, 0x20)
    phentsize, phnum = struct.unpack_from(end + "HH", data, 0x36)
    for i in range(phnum):
        p_type, _f, p_offset, _v, _p, p_filesz = struct.unpack_from(end + "IIQQQQ", data, phoff + i * phentsize)
        if p_type == 3:
            return data[p_offset:p_offset + p_filesz].rstrip(b"\0").decode()
    return None


LOADER = interpreter(BIN)
if not LOADER:
    print("skip: a static build has no loader")
    sys.exit(0)
listing = subprocess.run([LOADER, "--list", BIN], capture_output=True, text=True).stdout
libc = re.search(r"libc\.so\.6 => (\S+)", listing)
LIBRARY_PATH = str(Path(libc.group(1)).parent) if libc else str(Path(LOADER).parent)

ROOT = Path(tempfile.mkdtemp(prefix="RuneLoader.", dir=str(Path(__file__).resolve().parents[2] / "target/test-tmp")))
HOME = ROOT / "Home"; HOME.mkdir()
PROJECT = ROOT / "Project"; PROJECT.mkdir()
# A copy, as a person runs a build they downloaded: its path is not the
# loader's, and nothing about it is on PATH.
COPY = ROOT / "rune"
shutil.copy2(BIN, COPY)
(HOME / "fixture.json").write_text(json.dumps({"responses": [{"chunks": [{"text": "Answered through the loader."}]} for _ in range(4)]}))
(HOME / "rune.toml").write_text('[providers]\ndefault = "fixture"\n[[providers.list]]\nname = "fixture"\nbase_url = "http://127.0.0.1:1"\napi = "openai"\nauth = { header = "Authorization", prefix = "Bearer " }\n[[providers.list.models]]\nid = "test"\nthinking = false\ncontext = 32768\n')

env = dict(os.environ, RUNE_HOME=str(HOME), TERM="xterm-256color", COLORTERM="truecolor")
window = RuneTerminal.Terminal([LOADER, "--library-path", LIBRARY_PATH, str(COPY), "-project", str(PROJECT),
                                "-script", str(HOME / "fixture.json")], env, 34, 120)
output = bytearray()


def drain():
    output.extend(window.read())


def until(predicate, description, timeout=40):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        drain()
        if predicate():
            return
        time.sleep(0.05)
    raise AssertionError(f"{description}\n--- screen tail ---\n{output[-3000:].decode('utf-8', 'replace')}")


def transcript():
    path = next(HOME.glob("projects/*/sessions/1/session.jsonl"), None)
    return path.read_text() if path else ""


def mapped_program(pid):
    """The inode of the file `pid` mapped as Rune — its `exe` is the loader."""
    for line in Path(f"/proc/{pid}/maps").read_text().splitlines():
        if line.rstrip().endswith(str(COPY)):
            return int(line.split()[4])
    return None


def argv_of(pid):
    return [part.decode() for part in Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0") if part]


try:
    until(lambda: b"test" in output, "first paint")
    window.write(b"through the loader\r")
    until(lambda: "Answered through the loader." in transcript(), "the prompt answered through a session host")
    hosts = RuneTerminal.hosts(HOME)
    assert hosts, "no session host"
    argv = argv_of(hosts[0])
    # The kernel names the loader by its own path (`/lib64/ld-linux…` is a
    # link): the same file, however spelt.
    assert os.path.samefile(argv[0], LOADER), f"the host was not started by the loader: {argv}"
    expected = ["--library-path", LIBRARY_PATH, str(COPY), "-detached-host"]
    assert argv[1:5] == expected, f"the host was not started as the window was: {argv}"
    log = next(HOME.glob("run/session_*.host.log"), None)
    assert not (log and "shared object" in log.read_text()), log.read_text()
    assert b"exited during startup" not in output, "the window reported a host that died"

    # A release installs a new file at the program's path: the window and
    # its host each become it in place — through the same loader, since the
    # kernel's program is the loader, not Rune.
    mark = len(output)
    replacement = ROOT / "rune.new"
    shutil.copy2(BIN, replacement)
    os.replace(replacement, COPY)
    installed = os.stat(COPY).st_ino

    def runs_installed(pid):
        return mapped_program(pid) == installed

    until(lambda: runs_installed(window.pid), "the window took the installed build over", timeout=60)
    until(lambda: any(runs_installed(h) for h in RuneTerminal.hosts(HOME)),
          "the session host took the installed build over", timeout=60)
    for host in RuneTerminal.hosts(HOME):
        if runs_installed(host):
            assert os.path.samefile(argv_of(host)[0], LOADER), argv_of(host)
    # The new build has drawn its window and says it was taken over live.
    until(lambda: b"updated to" in output[mark:], "the new build's window drawn", timeout=30)
    window.write(b"after the release\r")
    until(lambda: transcript().count("Answered through the loader.") >= 2, "a prompt answered after the release")
    assert window.alive(), "the window ended"
    print("loader: ok")
finally:
    if window.alive():
        window.write(b"\x03\x03")
        if not window.wait(10):
            window.kill()
    for host in RuneTerminal.hosts(HOME):
        RuneTerminal.kill(host)
    if not os.environ.get("RUNE_KEEP"):
        shutil.rmtree(ROOT, ignore_errors=True)
