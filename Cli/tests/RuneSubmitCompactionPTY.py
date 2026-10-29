"""A manual summary cannot make a sent message look frozen or reorder it.

    python3 -I Cli/tests/RuneSubmitCompactionPTY.py [path/to/rune]

Uses the existing real-terminal/HTTP fixtures, isolated from the owner's home.
A held request on one project does not hold another on the same model. The
summary's status is visible and animated in a second window; queued messages
are answered in order without reopening or sending Continue.
"""
import json
import shutil
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import RuneLiveProviderPTY as fixture
import RuneTerminal


def main():
    provider = fixture.Provider()
    (fixture.HOME / "rune.toml").write_text(
        '[providers]\ndefault = "fake"\n'
        '[[providers.list]]\nname = "fake"\n'
        f'base_url = "http://127.0.0.1:{provider.port}/v1"\n'
        'api = "openai"\nauth = { header = "Authorization", prefix = "Bearer " }\n'
        '[[providers.list.models]]\nid = "fake-model"\nthinking = false\ncontext = 65536\n'
        '[network]\nstream_idle_timeout = "5s"\n'
        '[host]\nidle_lifetime = "3s"\n'
    )
    windows = []
    release = threading.Event()
    entered = threading.Event()

    def held_summary(handler, stream):
        fixture.open_stream(handler)
        entered.set()
        deadline = time.monotonic() + 60
        while not release.wait(0.1):
            if time.monotonic() >= deadline:
                raise AssertionError("test never released the summary")
            handler.wfile.write(b": summary is working\n\n")
            handler.wfile.flush()
        fixture.say(handler, fixture.delta(content="Older instructions and answers preserved."))
        fixture.say(handler, fixture.finish("stop"))
        fixture.close_stream(handler)

    try:
        a = fixture.Window(["-project", str(fixture.PROJECT)])
        windows.append(a)
        fixture.until(lambda: a.shows("fake-model"), "first paint", a)
        for n in range(7):
            answer = f"Seed answer {n}."
            fixture.ask(a, provider, f"seed question {n}", fixture.text(answer), expect=answer)
            fixture.until(lambda: fixture.transcript()[-1].get("work", {}).get("finished"),
                          "seed response settled", a)
        provider.queue(held_summary)
        a.type("/compact")
        a.send(b"\r")
        fixture.until(entered.is_set, "manual summary never started", a)
        fixture.until(lambda: a.shows("Compacting"), "manual summary has no active status", a)
        b = fixture.Window(["-project", str(fixture.PROJECT), "-resume", "1"])
        windows.append(b)

        def both_show_work():
            a.drain()
            return a.shows("Compacting") and b.shows("Compacting")

        fixture.until(both_show_work, "joining window lost the compaction status", b)
        frames = set()
        end = time.monotonic() + 1.2
        while time.monotonic() < end:
            a.drain()
            b.drain()
            frames.add(a.screen.text().splitlines()[-1])
            time.sleep(0.03)
        assert len(frames) > 1, "the compaction spinner/timer is frozen"
        print("PASS manual compaction animates in the original and joining window")

        # Both messages are submitted while the summary owns admission.
        a.type("first queued message")
        a.send(b"\r")
        pending_path = fixture.session_dir(1) / "composer.json"
        def is_queued(message):
            state = json.loads(RuneTerminal.read_document(pending_path))
            return (not state.get("text") and any(entry.get("text") == message
                    for entry in state.get("pending", [])))

        fixture.until(lambda: is_queued("first queued message"),
                      "first message was not durably queued", a)
        a.type("second queued message")
        a.send(b"\r")
        fixture.until(lambda: is_queued("second queued message"),
                      "second message was not durably queued", a)

        other_project = fixture.ROOT / "OtherProject"
        other_project.mkdir()
        other = fixture.Window(["-project", str(other_project)])
        windows.append(other)
        fixture.until(lambda: other.shows("fake-model"), "other project first paint", other)
        fixture.ask(other, provider, "independent request", fixture.text("Other project answered."),
                    expect="Other project answered.")
        assert not release.is_set(), "the summary was not held during the parallel request"
        print("PASS a held request does not serialize a different project using the same model")

        provider.queue(fixture.text("Both queued messages answered."))
        release.set()
        fixture.until(lambda: a.shows("Both queued messages answered."),
                      "queued work required reopening or Continue", a)
        fixture.until(lambda: b.shows("Both queued messages answered."),
                      "the joining window missed the answer", b)
        users = [m["content"] for m in fixture.transcript() if m.get("role") == "user"]
        assert users[-2:] == ["first queued message", "second queued message"], users
        for message in users[-2:]:
            assert users.count(message) == 1, users
        queued_rows = [m for m in fixture.transcript()
                       if m.get("role") == "user" and m.get("content") in users[-2:]]
        assert all(m.get("work", {}).get("source") == queued_rows[0]["id"]
                   for m in queued_rows), "a post-final message inherited the previous work"
        screen = a.screen.text()
        assert screen.index("first queued message") < screen.index("second queued message") < screen.index("Both queued messages answered."), screen
        print("PASS messages stay in send order, once, and resume without reopening")

        # Match the reported recovery: the last window leaves a held summary,
        # then a resumed window sends Continue. Its new message cannot overtake
        # what the host acknowledged before that departure.
        release.clear()
        entered.clear()
        provider.queue(held_summary)
        a.type("/compact")
        a.send(b"\r")
        fixture.until(entered.is_set, "second summary never started", a)
        a.type("message sent before leaving")
        a.send(b"\r")
        # Leave at once, without waiting for a composer-edit or prompt ACK.
        # Rune must keep running long enough to send what Enter committed,
        # rather than preserve it as a draft that Continue can be glued onto.
        a.send(b"\x03\x03")

        def submission_settled():
            return (is_queued("message sent before leaving") or any(
                m.get("role") == "user" and m.get("content") == "message sent before leaving"
                for m in fixture.transcript()))

        fixture.until(submission_settled, "exit lost Enter's message before host acceptance", a)
        # An exit paused for an unconfirmed submit says to retry once saved.
        a.close()
        b.close()
        # No owner process is killed: these are this fixture's own windows.
        resumed = fixture.Window(["-project", str(fixture.PROJECT), "-resume", "1"])
        windows.append(resumed)
        fixture.until(lambda: resumed.shows("fake-model"), "resumed first paint", resumed)
        provider.queue(fixture.text("Recovery answered in order."),
                       fixture.text("Recovery answered in order."))
        release.set()
        resumed.type("Continue")
        resumed.send(b"\r")

        def continued():
            rows = fixture.transcript()
            sent = [m for m in rows if m.get("role") == "user" and m.get("content") == "Continue"]
            return (sent and rows[-1].get("role") == "assistant"
                    and rows[-1].get("turn", 0) >= sent[-1]["turn"]
                    and rows[-1].get("work", {}).get("finished"))

        fixture.until(continued, "Continue was not persisted and answered", resumed)
        users = [m["content"] for m in fixture.transcript() if m.get("role") == "user"]
        assert users[-2:] == ["message sent before leaving", "Continue"], users
        assert users.count("message sent before leaving") == 1, users
        assert users.count("Continue") == 1, users
        print("PASS a resumed Continue cannot overtake a message sent before leaving")
    finally:
        release.set()
        for window in windows:
            window.close()
        provider.server.shutdown()
        for pid in RuneTerminal.holding(fixture.HOME, fixture.PROJECT):
            RuneTerminal.stop(pid)


if __name__ == "__main__":
    passed = False
    try:
        main()
        passed = True
    finally:
        if passed:
            shutil.rmtree(fixture.ROOT, ignore_errors=True)
        else:
            print(f"kept for inspection: {fixture.ROOT}", file=sys.stderr)
