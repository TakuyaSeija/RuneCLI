#!/usr/bin/env python3
"""Owner, 2026-10-16: what Rune shows and uses of a key is what is on disk.
A key whose file is gone is never shown as there; a store that cannot be
opened says why everywhere, is never written over, never gets a new master
key beside it, never stops Rune from starting, and is set aside only on the
person's explicit yes."""
import os
from pathlib import Path
import pty
import re
import select
import subprocess
import tempfile
import time

BIN = Path(os.environ.get("RUNE_BIN", "target/debug/rune")).resolve()
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
        pid, fd = pty.fork()
        if pid == 0:
            os.chdir(project)
            os.execve(str(BIN), ["rune"], dict(env, TERM="xterm-256color"))
        out, deadline, ended = b"", time.time() + 5, None
        while time.time() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.2)
            if ready:
                try:
                    out += os.read(fd, 65536)
                except OSError:
                    pass
            done, status = os.waitpid(pid, os.WNOHANG)
            if done:
                ended = status
                break
        if ended is None:
            os.kill(pid, 9)
            os.waitpid(pid, 0)
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
