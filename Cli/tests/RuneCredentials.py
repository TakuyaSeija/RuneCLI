#!/usr/bin/env python3
"""Owner, 2026-10-16: what Rune shows and uses of a key is what is on disk.
A key whose file is gone is never shown as there; a store that cannot be
opened says why everywhere, is never written over, never gets a new master
key beside it, never stops Rune from starting, and is set aside only on the
person's explicit yes."""
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
import RuneTerminal

BIN = Path(sys.argv[1] if len(sys.argv) > 1 else os.environ.get(
    "RUNE_BIN", "target/debug/rune.exe" if RuneTerminal.WINDOWS else "target/debug/rune")).resolve()
BASE = Path(__file__).resolve().parents[2] / "target" / "test-tmp"
BASE.mkdir(parents=True, exist_ok=True)

with tempfile.TemporaryDirectory(prefix="Credentials-", dir=BASE) as temp:
    base = Path(temp)
    home = base / "home" / ".rune"
    home.mkdir(parents=True)
    project = base / "project"
    project.mkdir()
    env = dict(os.environ, HOME=str(base / "home"), RUNE_HOME=str(home), NO_COLOR="1")
    env.pop("RUNE_HANDOVER", None)

    def run(*args, ok=True):
        result = subprocess.run([str(BIN), *args], cwd=project, env=env, text=True,
                                capture_output=True, timeout=60)
        assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
        return result.stdout + result.stderr

    def window_starts():
        window = RuneTerminal.Terminal([BIN], dict(env, TERM="xterm-256color"), 34, 120, cwd=str(project))
        out, deadline, ended = b"", time.time() + 5, None
        while time.time() < deadline:
            out += window.read()
            if not window.alive():
                ended = True
                break
            time.sleep(0.2)
        out += window.read()
        if ended is None:
            window.kill()
        text = re.sub(rb"\x1b\[[0-9;?<>=]*[a-zA-Z]|\x1b\][^\x07]*\x07", b"", out).decode("utf-8", "replace")
        return ended is None, text

    vault, key = home / "secrets.vault", home / "master.key"
    run("-provider", "key", "mimo", "sk-test-0000001")
    assert "1 key(s)" in run("-provider", "keys", "mimo")

    vault.unlink()
    said = run("-provider", "keys", "mimo", ok=False)
    assert "Key missing" in said and "secrets.vault" in said, said
    print("PASS a key whose store file is gone is said to be missing, not shown")

    run("-provider", "key", "mimo", "sk-test-0000002")
    key.unlink()
    held = vault.read_bytes()
    said = run("-provider", "keys", "mimo", ok=False)
    assert "Key unavailable" in said and "master.key is missing" in said, said
    assert "CREDENTIALS UNAVAILABLE" in run("-provider", "list")
    run("-provider", "key", "mimo", "sk-test-0000003", ok=False)
    assert not key.exists(), "never a new master key beside a store it cannot open"
    assert vault.read_bytes() == held, "never written over"
    assert "Configuration is valid" in run("-config", "check")
    alive, screen = window_starts()
    assert alive, screen[-400:]
    print("PASS a store without its master key is named, untouched, and Rune still starts")

    # Its master key back, the store itself cut off: that cause is named.
    key.write_bytes(bytes(range(32)))
    vault.write_bytes(b"\x00" * 8)
    alive, screen = window_starts()
    assert alive, screen[-400:]
    assert "cut off" in run("-provider", "keys", "mimo", ok=False)
    print("PASS a damaged store never stops a window")

    said = run("-provider", "new-store", ok=False)
    assert "--yes" in said and vault.exists(), said
    run("-provider", "new-store", "--yes")
    aside = list((home / "backups").glob("Credentials-*"))
    assert len(aside) == 1 and (aside[0] / "secrets.vault").read_bytes() == b"\x00" * 8
    assert (aside[0] / "master.key").read_bytes() == bytes(range(32)), "the pair stays together"
    assert not vault.exists() and not key.exists()
    assert "Key missing" in run("-provider", "keys", "mimo", ok=False)
    run("-provider", "key", "mimo", "sk-test-0000004")
    assert "1 key(s)" in run("-provider", "keys", "mimo")
    print("PASS a new store only on the person's yes; the old one kept aside; keys work again")
