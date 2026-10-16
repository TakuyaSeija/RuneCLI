#!/usr/bin/env python3
"""A failed data upgrade on any machine: the person's data stays as it was,
Rune never replaces the installed build (no `rune.prev`, owner 2026-10-16), a
process handed over goes back to the build that handed it, and the newest
build is reached once the cause is gone."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

BIN = Path(os.environ.get("RUNE_BIN", "target/debug/rune")).resolve()
BASE = Path(__file__).resolve().parents[2] / "target" / "test-tmp"
BASE.mkdir(parents=True, exist_ok=True)

with tempfile.TemporaryDirectory(prefix="UpgradeWayHome-", dir=BASE) as temp:
    base = Path(temp)
    # An install as any person has it — plus a file named like the old
    # developer rollback, which must stay untouched.
    install = base / "bin"
    install.mkdir()
    rune = install / "rune"
    shutil.copy2(BIN, rune)
    decoy = install / "rune.prev"
    decoy.write_text("not a program")
    installed = sorted(p.name for p in install.iterdir())
    stamp = rune.stat().st_ino

    home = base / "home"
    home.mkdir()
    (home / "version.json").write_text('{"version":19}')
    # The step's backup folder cannot be made: the upgrade stops before it
    # changes anything.
    (home / "backups").write_text("not a folder")
    user = base / "user"
    user.mkdir()
    project = base / "project"
    project.mkdir()
    env = dict(os.environ, HOME=str(user), RUNE_HOME=str(home), NO_COLOR="1")
    env.pop("RUNE_HANDOVER", None)

    def run(*args, extra=None, fds=()):
        return subprocess.run([str(rune), *args], cwd=project, env=dict(env, **(extra or {})),
                              text=True, capture_output=True, timeout=60, pass_fds=fds)

    def unchanged_install():
        assert sorted(p.name for p in install.iterdir()) == installed, list(install.iterdir())
        assert rune.stat().st_ino == stamp, "the installed build is never replaced"
        assert decoy.read_text() == "not a program"

    # A person starting Rune: told plainly, data unchanged, nothing replaced.
    started = run("-config", "check")
    assert started.returncode != 0, started
    said = started.stderr
    assert "could not be updated" in said and "Nothing of yours was changed" in said, said
    assert "rune.prev" not in said and "previous build at" not in said, said
    assert json.loads((home / "version.json").read_text())["version"] == 19
    unchanged_install()
    print("PASS a fresh start that cannot upgrade says why, changes nothing, replaces nothing")

    # Every window of the build before is held back for a while.
    probe = run("-upgrade-check")
    assert probe.returncode != 0 and "ready" not in probe.stdout, probe
    refused = json.loads((home / "run" / "Upgrade.refused").read_text())
    assert refused["attempts"] == 1, refused
    print("PASS a failed upgrade holds handovers back instead of every window failing at once")

    # A process another build handed over goes back to that build — here a
    # stand-in that records how it was started — with the same arguments and
    # the handover record still there to read.
    previous = base / "previous-build"
    seen = base / "seen.json"
    previous.write_text(
        "#!/usr/bin/env python3\n"
        "import json, os, sys\n"
        f"open({str(seen)!r}, 'w').write(json.dumps({{'args': sys.argv[1:], "
        "'record': os.environ.get('RUNE_HANDOVER')}))\n"
    )
    previous.chmod(0o700)
    fd = os.open(previous, os.O_RDONLY)
    os.set_inheritable(fd, True)
    record = json.dumps({"format": 2, "sessions": [], "program": fd})
    back = run("-config", "check", extra={"RUNE_HANDOVER": record}, fds=(fd,))
    os.close(fd)
    assert back.returncode == 0, back
    assert "Staying on the previous build" in back.stderr, back.stderr
    went = json.loads(seen.read_text())
    assert went["args"] == ["-config", "check"], went
    assert json.loads(went["record"])["program"] == fd, went
    unchanged_install()
    print("PASS a handed-over process goes back to the build that handed it, on any machine")

    # Its wait over and the cause gone, the newest build takes the home over.
    refused["at"] -= 2 * 3600 * 1000
    (home / "run" / "Upgrade.refused").write_text(json.dumps(refused))
    (home / "backups").unlink()
    ready = run("-upgrade-check")
    assert ready.returncode == 0 and "ready" in ready.stdout, ready
    done = run("-config", "check")
    assert done.returncode == 0, done
    assert json.loads((home / "version.json").read_text())["version"] >= 20
    assert not (home / "run" / "Upgrade.refused").exists()
    unchanged_install()
    print("PASS the newest build is reached once the cause is gone")
