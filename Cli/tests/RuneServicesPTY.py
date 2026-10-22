"""Real PTY regression for the account/request boundary (owner, 2026-10-15).

Connections stores endpoint/key only, without probing or choosing a tool.
Tools adds checked providers, opens an empty gateway route field with no
catalog/examples, selects exact connection:route targets and deletes only
providers. Search default and fetch built-in remain independent; selections
reach the real model request and /logs. Everything uses an isolated RUNE_HOME
and loopback stand-ins; no production key or third-party vendor is tested.
"""
import http.server
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import threading
import time
import urllib.parse

spec = importlib.util.spec_from_file_location("live", Path(__file__).with_name("RuneLiveProviderPTY.py"))
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)


class SearXNG(http.server.BaseHTTPRequestHandler):
    """Answers JSON when `json` is enabled, 403 otherwise — as SearXNG does."""
    json_enabled = True
    asked = []

    def do_GET(self):
        query = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
        SearXNG.asked.append(self.path)
        if query.get("format") != ["json"] or not SearXNG.json_enabled:
            body = b"<html><title>403 Forbidden</title></html>"
            self.send_response(403)
            self.send_header("Content-Type", "text/html")
        else:
            body = json.dumps({
                "query": query.get("q", [""])[0],
                "results": [{"title": "Landlock — Linux security module",
                             "url": "https://landlock.io/", "content": "Unprivileged sandboxing",
                             "engine": "duckduckgo"}],
                "answers": [],
            }).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


class NineRouter(http.server.BaseHTTPRequestHandler):
    """Answers as 9router 0.5.99 does: `GET /v1/models/web` lists each vendor's
    services as `<vendor>/search` and `<vendor>/fetch`; `POST /v1/search`
    serves a vendor it knows by its id alone, `ag` among them though unlisted
    (a chat model's search), and refuses anything else, `exa/search` too."""
    listed = ["exa", "glm"]
    unlisted = ["ag"]
    asked = []
    catalogs = []

    def reply(self, status, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.startswith("/v1/models/web"):
            NineRouter.catalogs.append(self.path)
            data = [{"id": f"{v}/{name}", "kind": kind, "owned_by": v}
                    for v in self.listed
                    for name, kind in (("search", "webSearch"), ("fetch", "webFetch"))]
            self.reply(200, {"object": "list", "data": data})
        else:
            self.reply(404, {"error": {"message": "not found"}})

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")
        NineRouter.asked.append((self.path, body))
        vendor = body.get("provider") or body.get("model") or ""
        if vendor not in self.listed + self.unlisted:
            self.reply(400, {"error": {"message": f"Unknown provider: {vendor}", "code": 400}})
        elif self.path == "/v1/web/fetch":
            self.reply(200, {"provider":vendor, "title":"Page", "content":{"text":"Page text", "format":"markdown"}})
        else:
            self.reply(200, {"provider": vendor, "results": [
                {"title": f"found by {vendor}", "url": "https://example.com/", "snippet": "s"}],
                "answer": None})

    def log_message(self, *args):
        pass


def scroll_to(window, text):
    """Move down the list until `text` is on the screen; search vendors sit
    below every model preset."""
    for _ in range(160):
        # Each step drawn before the next is sent: a row passed unseen is a
        # row the test never finds.
        time.sleep(0.12)
        window.drain()
        if live.row_with(window, text) is not None:
            return
        window.send(b"\x1b[B")
    raise AssertionError((text, window.screen.text()))


def click_text(window, text):
    row = live.row_with(window, text)
    assert row is not None, (text, window.screen.text())
    col = live.column_of(window, row, text)
    live.click(window, row + 1, col + 1)


def marked(window):
    """The line the cursor is on, or None."""
    return next((line for line in window.screen.text().split("\n") if "❯" in line), None)


def select(window, *needles):
    """Move the cursor to the row holding every needle — one key at a time,
    each drawn before the next is chosen (a debug build draws slowly)."""
    for _ in range(60):
        window.drain()
        row = live.row_with(window, *needles)
        assert row is not None, (needles, window.screen.text())
        if "❯" in window.screen.text().split("\n")[row]:
            return
        before = marked(window)
        mark = live.row_with(window, "❯")
        window.send(b"\x1b[B" if mark is None or mark < row else b"\x1b[A")
        deadline = time.time() + 5
        while time.time() < deadline and marked(window) == before:
            time.sleep(0.05)
            window.drain()
    raise AssertionError((needles, window.screen.text()))


def providers_page(a, heading):
    click_text(a, "Tools")
    live.until(lambda: a.shows("Overview / Tools") and not a.shows("PROVIDERS"), "Tools", a)
    a.send(b"\x1b[H")
    live.until(lambda: a.shows("TERMINAL (terminal_use)"), "Tools from top", a)
    scroll_to(a, heading)
    for _ in range(8):
        lines = a.screen.text().split("\n")
        group = live.row_with(a, heading)
        row = next((i for i in range(group + 1, len(lines)) if "Provider" in lines[i]), None)
        if row is not None:
            break
        a.send(b"\x1b[B")
        time.sleep(0.15)
        a.drain()
    assert row is not None, a.screen.text()
    live.click(a, row + 1, live.column_of(a, row, "Provider") + 1)
    live.until(lambda: a.shows("PROVIDERS") and a.shows("Add provider"), "provider manager", a)


def add_from_account(a, name):
    click_text(a, "Add provider")
    live.until(lambda: a.shows("CONNECTED ACCOUNTS"), "connected accounts", a)
    select(a, name)
    a.send(b"\r")


def gateway(a, router):
    click_text(a, "Connections")
    live.until(lambda: a.shows("CONNECTIONS") and a.shows("Add connection"), "Connections", a)
    click_text(a, "Add connection")
    live.until(lambda: a.shows("MODEL PROVIDERS"), "vendors", a)
    scroll_to(a, "9router · Search · Fetch")
    select(a, "9router · Search · Fetch")
    a.send(b"\r")
    live.until(lambda: a.shows("Key name / 9router"), "account setup", a)
    a.send(b"\x1b")
    live.until(lambda: a.shows("Save account"), "account form", a)
    for gone in ("Route", "Web search", "Web fetch", "SERP zone"):
        assert gone not in a.screen.text(), a.screen.text()
    select(a, "Base URL")
    a.send(b"\r")
    live.until(lambda: a.shows("Base URL / 9router"), "address", a)
    a.send(b"\x15")
    a.type(router)
    a.send(b"\r")
    live.until(lambda: a.shows("Key name / 9router"), "key label", a)
    a.send(b"\r")
    live.until(lambda: a.shows("API key / 9router"), "optional key", a)
    a.send(b"\r")
    live.until(lambda: a.shows("9router account saved"), "account saved", a)
    assert not NineRouter.asked, NineRouter.asked
    for gone in ("QUICK ACTIONS", "CONFIGURATION", "SERVICES", "Request timeout", "New-session fallback"):
        assert gone not in a.screen.text(), a.screen.text()
    print("ok Connections saves access only, without routes, request settings, or network probes")

    providers_page(a, "WEB SEARCH (web_search)")
    add_from_account(a, "9router")
    live.until(lambda: a.shows("Add a search route"), "empty route input", a)
    text = a.screen.text()
    assert "exa" not in text and "glm" not in text and "e.g." not in text, text
    a.type("ag/search")
    a.send(b"\r")
    live.until(lambda: a.shows("ag checked and kept"), "kept after request", a)
    assert NineRouter.asked[-1][1]["model"] == "ag", NineRouter.asked
    assert len(NineRouter.asked) == 1, NineRouter.asked
    live.until(lambda: live.row_with(a, "9router:ag", "Use") is not None, "full identity kept", a)
    assert 'provider = "9router:ag"' not in (live.HOME / "rune.toml").read_text()
    select(a, "9router:ag", "Use")
    a.send(b"\r")
    live.until(lambda: a.shows("9router:ag chosen"), "selected exact target", a)
    live.until(lambda: live.row_with(a, "9router:ag", "Chosen") is not None, "selection marked", a)
    assert 'provider = "9router:ag"' in (live.HOME / "rune.toml").read_text()
    assert not NineRouter.catalogs, NineRouter.catalogs
    print("ok Tools Add is empty and checked; choosing 9router:ag is a separate proved step")

    add_from_account(a, "9router")
    live.until(lambda: a.shows("Add a search route"), "second route", a)
    a.type("exa")
    a.send(b"\r")
    live.until(lambda: a.shows("exa checked and kept"), "second provider kept", a)
    a.send(b"\x1b")
    live.until(lambda: a.shows("WEB SEARCH (web_search)"), "back to Tools", a)
    providers_page(a, "WEB SEARCH (web_search)")
    live.until(lambda: live.row_with(a, "9router:exa", "Use") is not None, "kept after navigation", a)
    select(a, "9router:exa", "Use")
    a.send(b"\x1b[3~")
    live.until(lambda: a.shows("exa removed from 9router"), "delete provider", a)
    live.until(lambda: live.row_with(a, "9router:exa", "Use") is None, "removed only from tool", a)
    assert 'search = ["ag"]' in (live.HOME / "rune.toml").read_text()
    assert 'name = "9router"' in (live.HOME / "rune.toml").read_text()
    print("ok providers survive navigation; Delete removes the tool provider, not its account")

    providers_page(a, "WEB FETCH (web_fetch)")
    add_from_account(a, "9router")
    live.until(lambda: a.shows("Add a fetch route"), "fetch input", a)
    a.type("exa")
    a.send(b"\r")
    live.until(lambda: a.shows("exa checked and kept"), "fetch provider", a)
    live.until(lambda: live.row_with(a, "Default — Rune's built-in reader", "Chosen") is not None,
               "fetch still built-in until chosen", a)
    assert any(path == "/v1/web/fetch" and body["model"] == "exa" for path, body in NineRouter.asked)
    select(a, "9router:exa", "Use")
    a.send(b"\r")
    live.until(lambda: a.shows("9router:exa chosen"), "fetch chosen", a)
    select(a, "Default — Rune's built-in reader")
    a.send(b"\r")
    live.until(lambda: live.row_with(a, "Default — Rune's built-in reader", "Chosen") is not None,
               "fetch default restored", a)
    print("ok fetch providers are independently added and selected in Tools; default remains built-in")


def main():
    provider = live.Provider()
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), SearXNG)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    searxng = f"http://127.0.0.1:{server.server_address[1]}"
    nine = http.server.ThreadingHTTPServer(("127.0.0.1", 0), NineRouter)
    threading.Thread(target=nine.serve_forever, daemon=True).start()
    router = f"http://127.0.0.1:{nine.server_address[1]}/v1"
    (live.HOME / "rune.toml").write_text(
        '[ui]\nmouse = true\nsidebar = false\n[host]\nidle_lifetime = "1s"\n'
        '[providers]\ndefault = "fake"\n[[providers.list]]\nname = "fake"\n'
        f'base_url = "http://127.0.0.1:{provider.port}/v1"\n'
        'auth = { header = "Authorization", prefix = "Bearer " }\n'
        '[[providers.list.models]]\nid = "fake-model"\nthinking = false\ncontext = 65536\n'
    )
    a = live.Window(["-project", str(live.PROJECT)], rows=50, cols=170)
    try:
        live.until(lambda: a.shows("fake-model"), "first paint", a)
        a.send(b"\x07")
        live.until(lambda: a.shows("01 / MODELS"), "Settings", a)
        click_text(a, "Connections")
        live.until(lambda: a.shows("CONNECTIONS") and a.shows("Add connection"), "Connections", a)
        click_text(a, "Add connection")
        live.until(lambda: a.shows("MODEL PROVIDERS"), "vendors listed", a)
        scroll_to(a, "SearXNG · Search")
        SearXNG.json_enabled = False
        click_text(a, "SearXNG · Search")
        live.until(lambda: a.shows("Base URL / searxng"), "address", a)
        a.type(searxng)
        a.send(b"\r")
        live.until(lambda: a.shows("searxng account saved"), "saved access without request", a)
        assert not SearXNG.asked, SearXNG.asked
        providers_page(a, "WEB SEARCH (web_search)")
        add_from_account(a, "searxng")
        live.until(lambda: a.shows("search: formats:"), "real request refuses broken provider", a)
        assert "[services.list.search]" not in (live.HOME / "rune.toml").read_text()
        assert 'name = "searxng"' in (live.HOME / "rune.toml").read_text()
        print("ok unproven access can be saved; failed Tools add keeps account but adds no provider")
        SearXNG.json_enabled = True
        add_from_account(a, "searxng")
        live.until(lambda: a.shows("its check passed"), "real check succeeds", a)
        live.until(lambda: live.row_with(a, "searxng", "In use · default") is not None, "first search default", a)
        gateway(a, router)
        live.press_escape(a, 4)
        live.until(lambda: a.shows("ask, or / for commands"), "conversation", a)
        calls = live.ask(a, provider, "Search for landlock",
                         live.tool("web_search", {"action":"Look it up", "query":"landlock"}),
                         live.text("Found it."), expect="Found it.")
        sent = json.dumps(live.messages_of(calls[0]))
        assert "web search: 9router (ag)" in sent, sent[:2000]
        result = next(r for r in live.transcript() if r.get("tool_name") == "web_search")
        assert "found by ag" in result["content"], result
        a.type("/logs all")
        a.send(b"\r")
        live.until(lambda: a.shows("9router · 1 result(s)"), "search logged", a)
        print("ok the chosen exact provider serves the model and is recorded in /logs")
        print("PASS Tools providers / Connections access PTY")
    except BaseException:
        (live.ROOT / "FailedScreen.txt").write_text(a.screen.text())
        print("FAILED fixture:", live.ROOT)
        raise
    finally:
        a.close()
        server.shutdown()
        nine.shutdown()


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
