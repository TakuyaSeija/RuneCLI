"""Model connection setup asks for what the account needs, never calls vendor APIs:
a preset with its own address asks for the key alone and its Enter connects;
one whose address is the account's asks for that first (owner, 2026-10-22).

python3 -I Cli/tests/RuneModelConnectionsPTY.py [path/to/rune]
Isolated home, existing terminal fixture, local catalogue, no model inference.
"""
import importlib.util
from pathlib import Path
import shutil
import tomllib

spec = importlib.util.spec_from_file_location("live", Path(__file__).with_name("RuneLiveProviderPTY.py"))
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)


def click_text(window, text):
    row = live.row_with(window, text)
    assert row is not None, (text, window.screen.text())
    live.click(window, row + 1, live.column_of(window, row, text) + 1)


def select(window, text):
    for _ in range(80):
        window.drain()
        lines = window.screen.text().splitlines()
        target = next((i for i, line in enumerate(lines) if text in line), None)
        mark = next((i for i, line in enumerate(lines) if "❯" in line), None)
        if target is not None and mark == target:
            return
        before = window.screen.text()
        window.send(b"\x1b[B" if target is None or mark is None or mark < target else b"\x1b[A")
        live.until(lambda: window.screen.text() != before, "selection move", window)
        window.drain()
    raise AssertionError((text, window.screen.text()))


def edit(window, label, text, title):
    click_text(window, label)
    live.until(lambda: window.shows(title) and window.shows("Apply"), title, window)
    window.send(b"\x15")
    window.type(text)
    window.send(b"\r")
    live.until(lambda: window.shows("Connect →") and not window.shows("Apply"), "field retained", window)


def main():
    provider = live.Provider()
    (live.HOME / "rune.toml").write_text(
        '[ui]\nmouse = true\nsidebar = false\n[host]\nidle_lifetime = "1s"\n'
        '[providers]\ndefault = "fake"\n[[providers.list]]\nname = "fake"\n'
        f'base_url = "http://127.0.0.1:{provider.port}/v1"\n'
        'auth = { header = "Authorization", prefix = "Bearer " }\n'
        '[[providers.list.models]]\nid = "fake-model"\nthinking = false\n'
    )
    window = live.Window(["-project", str(live.PROJECT)], rows=48, cols=170)
    try:
        live.until(lambda: window.shows("fake-model"), "first paint", window)
        window.send(b"\x07")
        live.until(lambda: window.shows("01 / MODELS"), "Settings", window)
        click_text(window, "Connections")
        live.until(lambda: window.shows("Add connection"), "Connections", window)
        click_text(window, "Add connection")
        live.until(lambda: window.shows("MODEL PROVIDERS"), "model providers", window)
        select(window, "moonshot")
        window.send(b"\r")
        # The key is the one thing asked for; the preset's address is shown.
        live.until(lambda: window.shows("API key / moonshot"), "key asked first", window)
        window.send(b"\x1b")
        live.until(lambda: window.shows("Connect →") and window.shows("https://api.moonshot.ai"), "access form", window)
        before = tomllib.loads((live.HOME / "rune.toml").read_text())
        assert not any(p["name"] == "local-moonshot" for p in before["providers"]["list"])
        edit(window, "Connection name", "local-moonshot", "Connection name")
        # A test points the preset at the loopback stand-in; giving the
        # address leads straight on to the key, whose Enter connects.
        click_text(window, "API base URL")
        live.until(lambda: window.shows("Base URL / moonshot") and window.shows("Apply"), "address editor", window)
        window.send(b"\x15")
        window.type(f"http://127.0.0.1:{provider.port}/v1")
        window.send(b"\r")
        live.until(lambda: window.shows("API key / moonshot"), "key asked after the address", window)
        window.type("fixture-key-not-for-a-vendor")
        assert not provider.calls, "field editing must never run inference"
        window.send(b"\r")
        live.until(lambda: "local-moonshot" in (live.HOME / "rune.toml").read_text(), "saved on the key's Enter", window)
        saved = (live.HOME / "rune.toml").read_text()
        account = next(p for p in tomllib.loads(saved)["providers"]["list"] if p["name"] == "local-moonshot")
        assert account["api"] == "moonshot"
        assert account["base_url"] == f"http://127.0.0.1:{provider.port}/v1"
        assert "fixture-key-not-for-a-vendor" not in saved
        assert "fixture-key-not-for-a-vendor" not in window.screen.text()

        # Custom has no address of its own: it is asked for first, and its
        # key may stay empty.
        live.until(lambda: window.shows("connected and saved"), "connected", window)
        window.send(b"\x1b")
        live.until(lambda: window.shows("Add connection"), "Connections again", window)
        click_text(window, "Add connection")
        live.until(lambda: window.shows("MODEL PROVIDERS"), "model providers again", window)
        select(window, "compatible")
        window.send(b"\r")
        live.until(lambda: window.shows("Base URL / compatible"), "address asked first", window)
        window.type(f"http://127.0.0.1:{provider.port}/v1")
        window.send(b"\r")
        live.until(lambda: window.shows("API key / compatible"), "key after the address", window)
        window.send(b"\r")
        live.until(lambda: "\"compatible\"" in (live.HOME / "rune.toml").read_text(), "custom saved", window)
        custom = next(p for p in tomllib.loads((live.HOME / "rune.toml").read_text())["providers"]["list"]
                      if p["name"] == "compatible")
        assert custom["api"] == "compatible" and "secret" not in custom
        assert not provider.calls, "Connect checks catalogue, never model generation"
        print("PASS key-only preset connects on the key's Enter; account address asked first; TOML choices, masked vault key, no inference")
    except BaseException:
        (live.ROOT / "FailedScreen.txt").write_text(window.screen.text())
        print("FAILED fixture:", live.ROOT)
        raise
    finally:
        window.close()


if __name__ == "__main__":
    passed = False
    try:
        main()
        passed = True
    finally:
        for pid in live.RuneTerminal.holding(live.HOME, live.PROJECT):
            live.RuneTerminal.stop(pid)
        if passed:
            shutil.rmtree(live.ROOT, ignore_errors=True)
