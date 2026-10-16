"""Real windows for what the owner reported on 2026-10-15 (0.5.8):

    python3 -I Cli/tests/RuneDraftModelPTY.py [path/to/rune]

  1. `rune` started in a terminal standing in a folder that was deleted and
     made again says which folder and the `cd` that enters the new one — not a
     bare "No such file or directory (os error 2)".
  2. A model chosen in a draft stays chosen: from the Models page (arrows +
     Enter) and from `/model <id>`. It said "Settings saved. Active: B" and
     the screen went back to A, every time.
  3. A 9router route starts Off (owner, 2026-10-15): no effort beside its
     name, `reasoning_effort: none` in the request — on a route the router
     cannot stop thinking too; `/thinking on` is the route default (none sent),
     `/thinking low` sends `low`, `/thinking max` (a level three levels left
     out) sends `max`, `/thinking default` sends none again.

Runs on the provider stand-in of `RuneLiveProviderPTY.py`, in a home of its own.
"""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

spec = importlib.util.spec_from_file_location("live", Path(__file__).with_name("RuneLiveProviderPTY.py"))
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)

ROUTE = "ag/gemini-3.8-flash-high"
CAPS = {"reasoning": True, "thinkingFormat": "gemini-level", "thinkingCanDisable": False,
        "thinkingEffortSupported": False, "tools": True}


def header(window):
    window.drain()
    rows = [line for line in window.screen.text().splitlines() if "╭─ [" in line]
    return rows[-1] if rows else ""


def holds(window, needle, seconds, description):
    """`needle` is in the composer's title for the whole of `seconds`."""
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        title = header(window)
        assert needle in title, f"{description}: {title!r}\n--- screen ---\n{window.screen.text()}"
        time.sleep(0.1)


def drafted_model():
    drafts = json.loads((live.HOME / "drafts.json").read_text())
    for project in drafts["projects"].values():
        for entry in project["entries"]:
            return entry["composer"]["settings"]["model"]
    return None


def main():
    passed = []
    provider = live.Provider()
    rows = [{"id": "model-alpha", "object": "model"}, {"id": "model-bravo", "object": "model"},
            {"id": ROUTE, "object": "model", "capabilities": CAPS}]

    def do_get(self):
        body = json.dumps({"object": "list", "data": rows}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    provider.server.RequestHandlerClass.do_GET = do_get
    caps = ", ".join(f"{k} = {json.dumps(v)}" for k, v in CAPS.items())
    (live.HOME / "rune.toml").write_text(
        "[providers]\ndefault = \"fake\"\n"
        "[[providers.list]]\nname = \"fake\"\n"
        f"base_url = \"http://127.0.0.1:{provider.port}/v1\"\n"
        "auth = { header = \"Authorization\", prefix = \"Bearer \" }\n"
        "[[providers.list.models]]\nid = \"model-alpha\"\nthinking = false\ncontext = 65536\n"
        "[[providers.list.models]]\nid = \"model-bravo\"\nthinking = false\ncontext = 65536\n"
        "[[providers.list]]\nname = \"router\"\napi = \"9router\"\n"
        "dialect = \"open_ai\"\nthinking = \"effort\"\n"
        f"base_url = \"http://127.0.0.1:{provider.port}/v1\"\n"
        "auth = { header = \"Authorization\", prefix = \"Bearer \" }\n"
        f"[[providers.list.models]]\nid = \"{ROUTE}\"\nthinking = true\ncontext = 65536\n"
        f"metadata = {{ capabilities = {{ {caps} }} }}\n"
        "[host]\nidle_lifetime = \"3s\"\n"
    )

    # 1. A terminal standing in a deleted folder.
    gone = live.ROOT / "Testing"
    gone.mkdir()
    shell = subprocess.run(
        ["/bin/sh", "-c", f'cd "{gone}" && rm -rf "{gone}" && mkdir "{gone}" && exec "$0" -p hello', os.path.abspath(live.BIN)],
        env=dict(os.environ, RUNE_HOME=str(live.HOME), PWD=str(gone)),
        capture_output=True, text=True, timeout=30)
    said = shell.stdout + shell.stderr
    assert shell.returncode != 0, said
    assert "is now a different folder" in said and f"cd {gone}" in said, said
    assert "os error" not in said, said
    passed.append("a deleted working folder is named with the cd that enters the new one")

    window = live.Window(["-project", str(live.PROJECT)])
    try:
        live.until(lambda: "model-alpha" in header(window), "first paint", window)

        # 2a. The Models page: arrows to model-bravo, Enter.
        window.type("/model")
        window.send(b"\r")
        time.sleep(0.4)
        window.send(b"\r")
        live.until(lambda: window.shows("model-bravo"), "models page", window, 10)
        # Filtered to the one model, the cursor has one place to go.
        window.type("bravo")
        live.until(lambda: not window.shows("model-alpha       "), "filtered", window, 10)
        window.send(b"\x1b[B")
        live.until(lambda: "❯ model-bravo" in window.screen.text(), "cursor on model-bravo", window, 10)
        time.sleep(0.5)
        window.drain()
        assert "❯ model-bravo" in window.screen.text(), window.screen.text()
        window.send(b"\r")
        live.until(lambda: "fake  /  model-bravo" in window.screen.text(), "model-bravo active", window, 10)
        end = time.monotonic() + 3
        while time.monotonic() < end:
            window.drain()
            assert "fake  /  model-bravo" in window.screen.text(), window.screen.text()
            time.sleep(0.1)
        window.send(b"\x1b")
        time.sleep(0.4)
        window.send(b"\x1b")
        live.until(lambda: "model-bravo" in header(window), "back to the composer", window, 10)
        holds(window, "model-bravo", 2, "the draft went back to the model before")
        live.until(lambda: drafted_model() == "model-bravo", "drafts.json holds model-bravo", window, 10)
        passed.append("a model chosen on the Models page of a draft stays chosen")

        # 2b. `/model <id>` in the draft.
        window.type("/model model-alpha")
        window.send(b"\r")
        live.until(lambda: "model-alpha" in header(window), "model-alpha active", window, 10)
        holds(window, "model-alpha", 3, "/model went back to the model before")
        live.until(lambda: drafted_model() == "model-alpha", "drafts.json holds model-alpha", window, 10)
        passed.append("/model <id> in a draft stays chosen")

        # 3. A 9router route starts Off.
        window.type(f"/model router:{ROUTE}")
        window.send(b"\r")
        live.until(lambda: ROUTE in header(window), "the route active", window, 10)
        holds(window, ROUTE, 2, "the route was not kept")
        title = header(window)
        assert f"{ROUTE} ·" in title.replace("\x1b", ""), f"a level is shown beside the route: {title!r}"
        calls = live.ask(window, provider, "first question", live.text("Answer one."), expect="Answer one.")
        body = calls[-1]["body"]
        assert body.get("reasoning_effort") == "none" and "reasoning" not in body, body
        passed.append("a 9router route starts Off: no level shown, none asked for")

        window.type("/thinking on")
        window.send(b"\r")
        live.until(lambda: "Route default — /thinking minimal|low|medium|high|xhigh|max" in window.screen.text(),
                   "the route default and the router's levels said", window, 10)
        calls = live.ask(window, provider, "on question", live.text("Answer on."), expect="Answer on.")
        assert "reasoning_effort" not in calls[-1]["body"], calls[-1]["body"]
        passed.append("/thinking on is the route default: the router's levels named, none sent")

        window.type("/thinking max")
        window.send(b"\r")
        live.until(lambda: f"{ROUTE} max" in header(window), "max beside the route", window, 10)
        calls = live.ask(window, provider, "max question", live.text("Answer max."), expect="Answer max.")
        assert calls[-1]["body"].get("reasoning_effort") == "max", calls[-1]["body"]
        passed.append("/thinking max sends max")

        window.type("/thinking low")
        window.send(b"\r")
        live.until(lambda: f"{ROUTE} low" in header(window), "low beside the route", window, 10)
        calls = live.ask(window, provider, "second question", live.text("Answer two."), expect="Answer two.")
        assert calls[-1]["body"].get("reasoning_effort") == "low", calls[-1]["body"]

        window.type("/thinking default")
        window.send(b"\r")
        live.until(lambda: f"{ROUTE} low" not in header(window), "the level gone", window, 10)
        calls = live.ask(window, provider, "third question", live.text("Answer three."), expect="Answer three.")
        assert "reasoning_effort" not in calls[-1]["body"], calls[-1]["body"]
        passed.append("/thinking low sends low; /thinking default sends none again")
    finally:
        window.close()
        for line in passed:
            print("PASS", line)
    shutil.rmtree(live.ROOT, ignore_errors=True)
    print(f"{len(passed)}/7 passed")


if __name__ == "__main__":
    main()
