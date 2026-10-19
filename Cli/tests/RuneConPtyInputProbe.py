"""TEMPORARY diagnosis: does a Windows pseudo console hand a program in VT
input mode exactly the bytes written to it?

    python3 -I -B Cli/tests/RuneConPtyInputProbe.py [path/to/rune]

No Rune: a child reads its console records with the input mode Rune sets
(`console.rs` `raw`) and the same reads (`stdio.rs`), while this writes the
editing fixture's `clear()` pattern, idle and under CPU load. Every record is
kept beside the result. Elsewhere it says skip.
"""
import multiprocessing
import os
from pathlib import Path
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import RuneTerminal

CHILD = r'''
import ctypes, sys
from ctypes import wintypes as w
k32 = ctypes.WinDLL("kernel32", use_last_error=True)
class KEY(ctypes.Structure):
    _fields_ = [("down", w.BOOL), ("repeat", w.WORD), ("vk", w.WORD), ("scan", w.WORD),
                ("char", w.WCHAR), ("state", w.DWORD)]
class EVENT(ctypes.Union):
    _fields_ = [("key", KEY), ("pad", ctypes.c_byte * 16)]
class RECORD(ctypes.Structure):
    _fields_ = [("type", w.WORD), ("event", EVENT)]
handle = k32.GetStdHandle(-10)
mode = w.DWORD()
k32.GetConsoleMode(handle, ctypes.byref(mode))
# Rune's raw mode: no line/echo/processed/quick edit, VT input, window input,
# extended flags, the mouse the program's.
mode = (mode.value & ~(0x2 | 0x4 | 0x1 | 0x40 | 0x10)) | 0x200 | 0x8 | 0x80 | 0x10
k32.SetConsoleMode(handle, mode)
out = open(sys.argv[1], "w", encoding="utf-8")
typed = open(sys.argv[2], "wb")
records = (RECORD * 64)()
sys.stdout.write("READY\r\n"); sys.stdout.flush()
while True:
    k32.WaitForSingleObject(handle, 0xFFFFFFFF)
    waiting = w.DWORD()
    if not k32.GetNumberOfConsoleInputEvents(handle, ctypes.byref(waiting)) or not waiting.value:
        continue
    read = w.DWORD()
    k32.ReadConsoleInputW(handle, records, min(waiting.value, 64), ctypes.byref(read))
    for r in records[:read.value]:
        if r.type == 1:
            k = r.event.key
            out.write(f"K {'d' if k.down else 'u'} c={ord(k.char) if k.char else 0:04x} vk={k.vk:02x} r={k.repeat} s={k.state:x}\n")
            if k.down and k.char:
                typed.write((k.char * max(k.repeat, 1)).encode("utf-8", "surrogatepass"))
                if k.char == "Q":
                    out.close(); typed.close(); sys.exit(0)
        else:
            out.write(f"E{r.type}\n")
    out.write(f"-- {read.value}\n")
    out.flush(); typed.flush()
'''

DELETE = b"\x1b[3~"


def burn(until):
    while time.monotonic() < until:
        pass


def round_trip(folder, label, load):
    child = folder / "child.py"
    child.write_text(CHILD)
    records, typed = folder / f"records-{label}.txt", folder / f"typed-{label}.bin"
    term = RuneTerminal.Terminal([sys.executable, "-I", str(child), str(records), str(typed)],
                                 dict(os.environ), 30, 100)
    seen = bytearray()
    end = time.monotonic() + 20
    while b"READY" not in seen and time.monotonic() < end:
        seen.extend(term.read())
        time.sleep(0.02)
    assert b"READY" in seen, seen
    burners = []
    if load:
        until = time.monotonic() + 60
        burners = [multiprocessing.Process(target=burn, args=(until,)) for _ in range(os.cpu_count() * 2)]
        for b in burners:
            b.start()
    sent = bytearray()
    for _ in range(10):
        for _ in range(30):
            chunk = DELETE * 20
            term.write(chunk)
            sent.extend(chunk)
            term.read()
        chunk = b"\x1b[200~one two\x1b[201~\x1b[H" + b"\x1b[C" * 7 + b"\x17"
        term.write(chunk)
        sent.extend(chunk)
    term.write(b"Q")
    sent.extend(b"Q")
    end = time.monotonic() + 60
    while term.alive() and time.monotonic() < end:
        term.read()
        time.sleep(0.05)
    for b in burners:
        b.terminate()
    got = typed.read_bytes() if typed.exists() else b""
    if got == bytes(sent):
        print(f"PASS conpty delivers the bytes written ({label}, {len(sent)} bytes)", flush=True)
        return True
    at = next((i for i, (a, b) in enumerate(zip(got, sent)) if a != b), min(len(got), len(sent)))
    print(f"FAIL conpty {label}: {len(got)} of {len(sent)} bytes, first difference at {at}: "
          f"got {got[max(0, at - 12):at + 12]!r} sent {bytes(sent[max(0, at - 12):at + 12])!r}", flush=True)
    return False


def main():
    if not RuneTerminal.WINDOWS:
        print("skip: a Windows pseudo console only")
        return 0
    test_tmp = HERE.parents[1] / "target" / "test-tmp"
    test_tmp.mkdir(parents=True, exist_ok=True)
    # Kept beside the fixtures' evidence (the CI artifact's paths).
    folder = Path(tempfile.mkdtemp(prefix="RuneLiveProvider.probe-", dir=test_tmp))
    results = [round_trip(folder, "idle", False), round_trip(folder, "load", True)]
    (folder / "result.txt").write_text(repr(results))
    return 0 if all(results) else 1


if __name__ == "__main__":
    sys.exit(main())
