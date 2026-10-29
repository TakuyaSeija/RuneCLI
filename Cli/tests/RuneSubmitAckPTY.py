"""A sent message must not wait forever for an edit ACK already confirmed by state.

    python3 -I Cli/tests/RuneSubmitAckPTY.py [path/to/rune]

A real stdio engine talks to a real window through an authenticated test socket.
The bridge holds one edit ACK, not its authoritative Composer event. No summary
is requested; no provider other than the isolated local HTTP fixture is used.
"""
import importlib.util
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import sys
import threading
import time

spec = importlib.util.spec_from_file_location("live", Path(__file__).with_name("RuneLiveProviderPTY.py"))
live = importlib.util.module_from_spec(spec)
spec.loader.exec_module(live)


class Bridge:
    def __init__(self):
        self.lock = threading.RLock()
        self.events = []
        self.owners = {}
        self.clients = []
        self.hold = False
        self.held_id = None
        self.held_ack = None
        self.holding = threading.Event()
        self.confirmed = threading.Event()
        self.protocol = None
        self.ready = threading.Event()
        self.trace = (live.ROOT / "bridge-events.jsonl").open("w", buffering=1)
        self.stderr = (live.ROOT / "engine-stderr.txt").open("w")
        env = dict(os.environ, RUNE_HOME=str(live.HOME), HOME=str(live.ROOT), TERM="dumb")
        env.pop("RUNE_HANDOVER", None)
        self.host = subprocess.Popen([live.BIN, "-project", str(live.PROJECT), "-host"],
                                     env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=self.stderr, text=True, bufsize=1)
        threading.Thread(target=self.read_engine, daemon=True).start()
        assert self.ready.wait(30), "stdio engine did not become ready"
        run = live.HOME / "run"
        run.mkdir(exist_ok=True)
        self.path = run / "session_1.sock"
        self.key = secrets.token_hex(32)
        self.path.with_suffix(".key").write_text(self.key)
        self.listener = socket.socket(socket.AF_UNIX)
        previous = os.getcwd()
        os.chdir(run)
        try:
            self.listener.bind(self.path.name)
        finally:
            os.chdir(previous)
        self.listener.listen()
        threading.Thread(target=self.accept, daemon=True).start()

    def send_host(self, command, owner=None):
        with self.lock:
            if owner is not None:
                self.owners[command["id"]] = owner
            self.trace.write(json.dumps({"at": time.monotonic(), "command": command}) + "\n")
            self.host.stdin.write(json.dumps(command) + "\n")
            self.host.stdin.flush()

    def send_client(self, client, event):
        try:
            client.sendall((json.dumps(event) + "\n").encode())
        except OSError:
            pass

    def read_engine(self):
        for line in self.host.stdout:
            event = json.loads(line)
            with self.lock:
                self.events.append(event)
                self.trace.write(json.dumps({"at": time.monotonic(), "event": event}) + "\n")
                if event.get("t") == "ready":
                    self.protocol = event["protocol"]
                    self.ready.set()
                if event.get("t") == "composer" and event.get("request") == self.held_id:
                    self.confirmed.set()
                if event.get("t") == "ack":
                    if event.get("id") == self.held_id:
                        self.held_ack = event
                        self.holding.set()
                        continue
                    owner = self.owners.pop(event.get("id"), None)
                    if owner is not None:
                        self.send_client(owner, event)
                else:
                    for client in self.clients:
                        self.send_client(client, event)

    def accept(self):
        while True:
            try:
                client, _ = self.listener.accept()
            except OSError:
                return
            threading.Thread(target=self.serve, args=(client,), daemon=True).start()

    def serve(self, client):
        try:
            reader = client.makefile("r")
            hello = json.loads(reader.readline())
            assert hello.get("cmd") == "session.hello" and hello.get("key") == self.key
            with self.lock:
                self.send_client(client, {"t": "ack", "id": 0, "ok": True,
                                          "data": {"protocol": self.protocol}})
                self.clients.append(client)
            for line in reader:
                command = json.loads(line)
                with self.lock:
                    if command.get("cmd") == "composer.edit" and self.hold and self.held_id is None:
                        self.held_id = command["id"]
                self.send_host(command, client)
        except (OSError, ValueError):
            pass
        finally:
            with self.lock:
                if client in self.clients:
                    self.clients.remove(client)
            client.close()

    def release(self):
        with self.lock:
            if self.held_ack is not None:
                owner = self.owners.pop(self.held_id, None)
                if owner is not None:
                    self.send_client(owner, self.held_ack)
                self.held_ack = None

    def close(self):
        self.release()
        self.listener.close()
        with self.lock:
            for client in self.clients:
                try:
                    client.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
        self.host.stdin.close()
        try:
            self.host.wait(15)
        except subprocess.TimeoutExpired:
            self.host.kill()
            self.host.wait(5)
        self.stderr.close()


def main():
    provider = live.Provider()
    (live.HOME / "rune.toml").write_text(
        '[providers]\ndefault="fake"\n[[providers.list]]\nname="fake"\n'
        f'base_url="http://127.0.0.1:{provider.port}/v1"\n'
        'api="openai"\nauth={header="Authorization",prefix="Bearer "}\n'
        '[[providers.list.models]]\nid="fake-model"\nthinking=false\ncontext=1000000\n')
    bridge = Bridge()
    window = None
    try:
        provider.queue(live.text("Seed final response."))
        bridge.send_host({"cmd": "prompt", "id": 900000001, "session": 1,
                          "text": "seed question", "attach": []})
        live.until(lambda: any(e.get("t") == "done" for e in bridge.events), "seed final was not settled")
        window = live.Window(["-project", str(live.PROJECT), "-resume", "1"])
        live.until(lambda: window.shows("Seed final response."), "resumed first paint", window)
        provider.queue(live.text("Message after final answered."))
        bridge.hold = True
        window.send(b"\x1b[200~message after final\x1b[201~\r")
        live.until(lambda: bridge.holding.is_set() and bridge.confirmed.is_set(),
                   "test did not hold an ACK after publishing confirmed composer state", window)
        live.until(lambda: window.shows("Message after final answered."),
                   "submit is stranded behind a held edit ACK although the engine already confirmed it", window, timeout=6)
        assert not any(e.get("t") in ("compacting", "compacted") for e in bridge.events), "this test must not involve compaction"
        assert bridge.held_ack is not None, "the answer must precede the held ACK"
        live.until(lambda: live.transcript()[-1].get("work", {}).get("finished"), "new final stored", window)
        provider.queue(live.text("Next message answered separately."))
        window.type("next independent message")
        composer = live.session_dir(1) / "composer.json"
        live.until(lambda: json.loads(live.RuneTerminal.read_document(composer)).get("text") == "next independent message",
                   "typing after the submitted message was not saved separately", window)
        bridge.release()
        window.send(b"\r")
        live.until(lambda: window.shows("Next message answered separately."), "late ACK disturbed the next message", window)
        live.until(lambda: live.transcript()[-1].get("work", {}).get("finished"), "next final stored", window)
        users = [r["content"] for r in live.transcript() if r.get("role") == "user"]
        assert users == ["seed question", "message after final", "next independent message"], users
        assert not any(e.get("t") in ("compacting", "compacted") for e in bridge.events)
        print("PASS submit after final proceeds on authoritative confirmation without waiting for the late ACK")
        print("PASS a late ACK preserves later typing and the next message stays separate")
        print("PASS no compaction, reconnect, extra Enter or duplicate user message")
    finally:
        bridge.release()
        if window is not None:
            window.close()
        bridge.close()
        provider.server.shutdown()


if __name__ == "__main__":
    if live.WINDOWS:
        print("skip ACK socket bridge: Unix-socket transport; editor state regressions run on every OS")
        shutil.rmtree(live.ROOT, ignore_errors=True)
    else:
        passed = False
        try:
            main()
            passed = True
        finally:
            if passed:
                shutil.rmtree(live.ROOT, ignore_errors=True)
            else:
                print(f"kept for inspection: {live.ROOT}", file=sys.stderr)
