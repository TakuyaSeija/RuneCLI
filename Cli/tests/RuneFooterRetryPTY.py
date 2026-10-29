"""Footer ownership in a real isolated terminal against a local HTTP provider.
python3 -I -B Cli/tests/RuneFooterRetryPTY.py /path/to/rune
No live account or installed binary is used.
"""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys
import time

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location("live", Path(__file__).with_name("RuneLiveProviderPTY.py"))
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)
binary = live.ROOT / ("rune-footer.exe" if live.WINDOWS else "rune-footer")
shutil.copy2(live.BIN, binary)
live.BIN = str(binary)
os.environ["HOME"] = str(live.HOME)
os.environ.pop("RUNE_PROCESS_TREE", None)


def hold(window, seconds, predicate=None):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        window.drain()
        if predicate is not None:
            assert predicate(), window.screen.text()
        time.sleep(.03)


def paste(window, text):
    window.send(b"\x1b[200~" + text.encode() + b"\x1b[201~")
    window.send(b"\r")


def main():
    provider = live.Provider()
    (live.HOME / "rune.toml").write_text(
        '[ui]\nmouse=true\nsidebar=false\n'
        '[host]\nidle_lifetime="1s"\n'
        '[classifier]\nenabled=false\n'
        '[retry]\ninitial_delay="12s"\nmax_delay="1m"\nmax_attempts=3\n'
        '[providers]\ndefault="fake"\n'
        '[[providers.list]]\nname="fake"\n'
        f'base_url="http://127.0.0.1:{provider.port}/v1"\n'
        'auth={header="Authorization",prefix="Bearer "}\n'
        '[[providers.list.models]]\nid="fake-model"\nthinking=false\ncontext=65536\n'
    )
    window = live.Window(["-project", str(live.PROJECT)], rows=30, cols=110)
    try:
        live.until(lambda: window.shows("fake-model"), "initial paint", window)
        provider.queue(live.status(503, {}, {"error": {"message": "offline footer fixture"}}))
        paste(window, "Exercise retry cancellation")
        live.until(lambda: window.shows("retrying in"), "retry countdown", window)
        before = len(provider.main_calls())
        window.send(b"\x1b")
        live.until(lambda: window.shows("press esc again to stop"), "Esc hint over retry", window)
        hold(window, 2.2, lambda: window.shows("press esc again to stop") and not window.shows("retrying in"))
        live.until(lambda: window.shows("retrying in") and not window.shows("press esc again to stop"), "hint expires at stop deadline", window, timeout=2)
        assert len(provider.main_calls()) == before
        window.send(b"\x1b")
        live.until(lambda: window.shows("press esc again to stop"), "expired Esc rearms", window)
        hold(window, .15)
        window.send(b"\x1b")
        live.until(lambda: window.shows("stopped"), "second Esc cancels retry", window)
        live.settled(window)
        assert len(provider.main_calls()) == before, "cancel restarted the model request"
        print("PASS Esc visible across retry ticks, expires at 3s, rearms and cancels", flush=True)

        paste(window, "/verbose")
        live.until(lambda: window.shows("Verbose mode on"), "first ordinary status", window)
        start = time.monotonic()
        paste(window, "/verbose")
        hold(window, 2.2, lambda: window.shows("Verbose mode on") and not window.shows("Verbose mode off"))
        live.until(lambda: window.shows("Verbose mode off"), "waiting status replaces after 3s", window, timeout=2)
        elapsed = time.monotonic() - start
        assert 2.5 <= elapsed <= 4.8, elapsed
        hold(window, 4, lambda: window.shows("Verbose mode off"))
        live.until(lambda: not window.shows("Verbose mode off"), "unopposed status expires at 8s", window, timeout=5)
        print("PASS ordinary messages retain 3s when contested and 8s when unopposed", flush=True)

        provider.queue(live.status(503, {}, {"error": {"message": "retry continues behind confirmation"}}), live.text("RETRY_DEADLINE_UNCHANGED"))
        start = time.monotonic()
        paste(window, "Exercise retry continuation")
        live.until(lambda: window.shows("retrying in"), "second retry countdown", window)
        window.send(b"\x1b")
        live.until(lambda: window.shows("press esc again to stop"), "confirmation before continuation", window)
        hold(window, 1.5)
        window.send(b"x")
        live.until(lambda: not window.shows("press esc again to stop"), "ordinary key disarms confirmation", window)
        live.until(lambda: window.shows("RETRY_DEADLINE_UNCHANGED"), "retry deadline unaffected by display", window, timeout=16)
        elapsed = time.monotonic() - start
        assert 11 <= elapsed <= 16, elapsed
        live.settled(window)
        print(f"PASS confirmation does not reset the real retry deadline ({elapsed:.2f}s)", flush=True)
        (live.ROOT / "provider-calls.json").write_text(json.dumps(provider.calls, indent=2))
    finally:
        window.close()
        provider.server.shutdown()
    print("Artifacts:", live.ROOT, flush=True)


if __name__ == "__main__":
    main()
