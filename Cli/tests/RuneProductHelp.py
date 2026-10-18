#!/usr/bin/env python3
"""Installed help is offline and independent of the source checkout or real home."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

WINDOWS = sys.platform == "win32"
BIN = Path(sys.argv[1] if len(sys.argv) > 1 else os.environ.get(
    "RUNE_BIN", "target/debug/rune.exe" if WINDOWS else "target/debug/rune")).resolve()
BASE = Path(__file__).resolve().parents[2] / "target" / "test-tmp"
BASE.mkdir(parents=True, exist_ok=True)

with tempfile.TemporaryDirectory(prefix="ProductHelp-", dir=BASE) as temp:
    base = Path(temp)
    home = base / "fresh-home"
    user = base / "isolated-user"
    project = base / "unrelated-project"
    user.mkdir()
    project.mkdir()
    env = dict(os.environ, HOME=str(user), RUNE_HOME=str(home), NO_COLOR="1")

    def run(*args, ok=True):
        result = subprocess.run([str(BIN), *args], cwd=project, env=env,
                                text=True, capture_output=True, timeout=60)
        assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
        return result.stdout

    run("-upgrade-check")
    assert not home.exists(), "probe must not create even an empty home"
    inventory = json.loads(run("-skills", "list"))
    help_skill = next(s for s in inventory["skills"] if s["id"] == "rune-settings-guide:builtin")
    assert help_skill["source"] == "rune-product" and help_skill["scope"] == "product"
    package = Path(help_skill["package_root"])
    docs = list(package.rglob("*.md"))
    assert len(docs) >= 90
    assert not (home / "rune.toml").exists(), "help doesn't configure a provider"
    assert not (home / "master.key").exists()
    shown = json.loads(run("-skills", "show", help_skill["id"]))
    assert "Rune user settings guide" in shown["content"]
    assert "terminal.command_timeout" in (package / "tools/terminal-use.md").read_text()
    assert all("Source authorities" not in p.read_text() for p in docs)
    assert not (user / ".rune").exists(), "explicit RUNE_HOME stays isolated"
    run("-skills", "disable", help_skill["id"])
    listed = json.loads(run("-skills", "list"))
    assert next(s for s in listed["skills"] if s["id"] == help_skill["id"])["disabled"]
    run("-skills", "enable", help_skill["id"])
    print("PASS fresh home, standalone binary, official discovery/show and disable/enable")

    # An old home gets the managed bundle but keeps its chosen configuration/personal files.
    upgraded = base / "old-home"
    upgraded.mkdir()
    (upgraded / "version.json").write_text('{"version":19}')
    choices = '[ui]\ntheme = "cyan" # keep my choice\n'
    (upgraded / "rune.toml").write_text(choices)
    personal = upgraded / "skills" / "mine"
    personal.mkdir(parents=True)
    (personal / "SKILL.md").write_text('---\nname: mine\ndescription: Personal guide\n---\nKeep this.\n')
    env["RUNE_HOME"] = str(upgraded)
    run("-upgrade-check")
    assert json.loads((upgraded / "version.json").read_text())["version"] == 19
    listed = json.loads(run("-skills", "list"))
    assert any(s["id"] == help_skill["id"] for s in listed["skills"])
    assert any(s["name"] == "mine" for s in listed["skills"])
    assert (upgraded / "rune.toml").read_text() == choices
    assert "Keep this." in (personal / "SKILL.md").read_text()
    assert json.loads((upgraded / "version.json").read_text())["version"] == 20
    assert not (upgraded / "master.key").exists()
    print("PASS format19 upgrade preserves user choices and personal skills")

    # Owner, 2026-10-16: one changed byte or an editor's swap file in the
    # official folder refused every start. A damaged copy is replaced whole by
    # the build's own text, and nothing stops.
    official = next(s for s in listed["skills"] if s["id"] == help_skill["id"])
    package = Path(official["package_root"])
    resource = package / "tools" / "terminal-use.md"
    original = resource.read_text()
    resource.write_text("modified reference")
    (package / "menus" / ".tools.md.swp").write_text("swap")
    (package / ".directory").write_text("[Dolphin]")
    run("-upgrade-check")
    assert "Configuration is valid" in run("-config", "check")
    listed = json.loads(run("-skills", "list"))
    assert any(s["id"] == help_skill["id"] for s in listed["skills"])
    assert resource.read_text() == original, "the build's own copy is put back"
    assert not (package / "menus" / ".tools.md.swp").exists()
    assert not (package / ".directory").exists()
    leftovers = [p.name for p in package.parent.parent.iterdir()
                 if not p.name.startswith((".lock", ".in-use-")) and p.name != package.parent.name]
    assert leftovers == [], leftovers
    print("PASS damaged managed help is replaced whole; starts and checks go on")

    # The capability switch decides whether help is installed at all.
    switched = base / "switched-off"
    switched.mkdir()
    (switched / "rune.toml").write_text("[capabilities]\nenabled = false\n")
    env["RUNE_HOME"] = str(switched)
    assert "Configuration is valid" in run("-config", "check")
    run("-skills", "list")
    assert not (switched / "product-skills").exists()
    print("PASS capabilities switched off: no help is written")
