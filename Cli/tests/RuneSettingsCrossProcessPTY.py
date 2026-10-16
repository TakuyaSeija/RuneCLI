"""A global setting saved by one process shows in every other window without a
relaunch — a window still on its draft and a window with a session, Settings
open while it changes or opened after, saved by `-config set` or by another
window's own Settings (owner, 2026-10-14: a draft window kept the old value
until it was relaunched).

python3 -I Cli/tests/RuneSettingsCrossProcessPTY.py [path/to/rune]
Two real windows on one isolated RUNE_HOME and a local stand-in provider.
"""
import importlib.util
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time

spec = importlib.util.spec_from_file_location("live", Path(__file__).with_name("RuneLiveProviderPTY.py"))
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)

KEY = "sanitizer.timeout"
# A window shows what another process saved within this long: the shared
# configuration follows the file every 250 ms, the window asks every ~100 ms.
FOLLOWS_WITHIN = 5


def click_text(window, text):
    row = live.row_with(window, text)
    assert row is not None, (text, window.screen.text())
    live.click(window, row + 1, live.column_of(window, row, text) + 1)


def shown(window):
    window.drain()
    for line in window.screen.text().split("\n"):
        if KEY in line:
            return line.strip()
    return ""


def open_generation(window):
    window.send(b"\x07")
    live.until(lambda: window.shows("01 / MODELS"), "Settings", window)
    click_text(window, "Generation")
    live.until(lambda: window.shows(KEY), "Generation page", window)


def close_settings(window):
    window.send(b"\x1b\x1b")
    live.until(lambda: window.shows("for commands"), "back to the conversation", window)


def config_set(value):
    env = dict(os.environ, RUNE_HOME=str(live.HOME))
    out = subprocess.run([live.BIN, "-config", "set", KEY, value], env=env, capture_output=True, text=True)
    assert out.returncode == 0, out


def expect(window, value, what):
    live.until(lambda: value in shown(window), f"{what}: expected {value}, saw {shown(window)!r}",
               window, timeout=FOLLOWS_WITHIN)


def save_in_settings(window, value):
    """Through the window's own Settings: a sensitive field asks twice."""
    click_text(window, KEY)
    live.until(lambda: window.shows("Apply"), "field editor", window)
    window.send(b"\x15")
    window.type(value)
    for _ in range(4):
        window.send(b"\r")
        time.sleep(0.8)
        window.drain()
        if not window.shows("Apply") and value in shown(window):
            return
    raise AssertionError("the window's own save did not apply\n" + window.screen.text())


def main():
    provider = live.Provider()
    (live.HOME / "rune.toml").write_text(
        '[ui]\nmouse = true\nsidebar = false\n[host]\nidle_lifetime = "1s"\n'
        '[providers]\ndefault = "fake"\n[[providers.list]]\nname = "fake"\n'
        f'base_url = "http://127.0.0.1:{provider.port}/v1"\n'
        'auth = { header = "Authorization", prefix = "Bearer " }\n'
        '[[providers.list.models]]\nid = "fake-model"\nthinking = false\ncontext = 65536\n'
    )
    other = live.ROOT / "Other"
    other.mkdir()
    a = live.Window(["-project", str(live.PROJECT)], rows=70, cols=180)
    b = live.Window(["-project", str(other)], rows=70, cols=180)
    try:
        live.until(lambda: a.shows("fake-model"), "first window", a)
        live.until(lambda: b.shows("fake-model"), "second window", b)

        config_set("77s")
        time.sleep(1)
        open_generation(a)
        expect(a, "77s", "draft window, Settings opened after the change")
        config_set("88s")
        expect(a, "88s", "draft window, Settings open while it changes")
        close_settings(a)
        print("ok a draft window follows -config set")

        live.ask(a, provider, "hello", live.text("Hi there."), expect="Hi there.")
        open_generation(a)
        config_set("99s")
        expect(a, "99s", "session window, Settings open while it changes")
        close_settings(a)
        config_set("66s")
        time.sleep(1)
        open_generation(a)
        expect(a, "66s", "session window, Settings opened after the change")
        print("ok a session window follows -config set")

        open_generation(b)
        save_in_settings(b, "55s")
        expect(a, "55s", "session window, saved in a draft window's Settings")
        assert "55s" in (live.HOME / "rune.toml").read_text()
        save_in_settings(a, "44s")
        expect(b, "44s", "draft window, saved in a session window's Settings")
        close_settings(b)
        open_generation(b)
        expect(b, "44s", "draft window, Settings reopened")
        print("ok each window's own Settings reaches the other, both ways")

        close_settings(a)
        close_settings(b)
        config_set("33s")
        time.sleep(1)
        open_generation(a)
        open_generation(b)
        expect(a, "33s", "session window after both closed Settings")
        expect(b, "33s", "draft window after both closed Settings")
        print("PASS settings follow across processes")
    except BaseException:
        (live.ROOT / "FailedA.txt").write_text(a.screen.text())
        (live.ROOT / "FailedB.txt").write_text(b.screen.text())
        print("FAILED fixture:", live.ROOT)
        raise
    finally:
        a.close()
        b.close()


if __name__ == "__main__":
    passed = False
    try:
        main()
        passed = True
    finally:
        for proc in Path("/proc").glob("[0-9]*/cmdline"):
            try:
                if str(live.HOME).encode() in proc.read_bytes():
                    os.kill(int(proc.parent.name), signal.SIGTERM)
            except (OSError, ProcessLookupError):
                pass
        if passed:
            shutil.rmtree(live.ROOT, ignore_errors=True)
