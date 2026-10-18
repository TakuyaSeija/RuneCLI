"""Run every PTY fixture against one build, as CI does on each OS.

    python3 -I Cli/tests/RunPTY.py [path/to/rune]

Each fixture runs alone, in its own process, with a deadline; its output is
printed when it fails. The Konsole fixtures drive a real Konsole window over
DBus on the owner's desktop and are not run here.
"""
import subprocess, sys, time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import RuneTerminal

BIN = sys.argv[1] if len(sys.argv) > 1 else str(
    HERE.parents[1] / "target" / "debug" / ("rune.exe" if RuneTerminal.WINDOWS else "rune"))
DESKTOP_ONLY = ("RuneKonsole",)
DEADLINE = 1200

failed = []
for fixture in sorted(HERE.glob("Rune*.py")):
    if fixture.stem == "RuneTerminal" or fixture.stem.startswith(DESKTOP_ONLY):
        continue
    began = time.monotonic()
    try:
        run = subprocess.run([sys.executable, "-I", str(fixture), BIN], capture_output=True,
                             text=True, encoding="utf-8", errors="replace", timeout=DEADLINE)
        code, out = run.returncode, run.stdout + run.stderr
    except subprocess.TimeoutExpired as e:
        said = [part.decode("utf-8", "replace") if isinstance(part, bytes) else part or ""
                for part in (e.stdout, e.stderr)]
        code, out = "timeout", "".join(said)
    took = time.monotonic() - began
    skips = [line for line in out.splitlines() if line.startswith("skip")]
    print(f"{'PASS' if code == 0 else 'FAIL'} {fixture.stem} ({took:.0f}s)", flush=True)
    for line in skips:
        print(f"     {line}", flush=True)
    if code != 0:
        failed.append(fixture.stem)
        print(f"----- {fixture.stem} (exit {code}) -----\n{out[-12000:]}\n-----", flush=True)

print(f"{'FAILED: ' + ', '.join(failed) if failed else 'all PTY fixtures passed'}")
sys.exit(1 if failed else 0)
