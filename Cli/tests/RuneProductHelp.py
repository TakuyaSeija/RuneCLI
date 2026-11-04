#!/usr/bin/env python3
"""Installed help is offline and independent of the source checkout or real home."""
import json
import os
from pathlib import Path
import shutil
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

    probe = run("-upgrade-check")
    current_format = next(int(line.split()[1]) for line in probe.splitlines() if line.startswith("format "))
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
    terminal_help = (package / "tools/terminal-use.md").read_text()
    assert all(word in terminal_help for word in ("terminal.command_timeout", "full access once", "whitespace before", "does not automatically replay", "PowerShell", "Cmd"))
    display_help = (package / "appearance/layout.md").read_text()
    assert all(word in display_help for word in ("Open in browser", "Copy complete destination", "copying only", "does not run the file"))
    compact_help = (package / "auxiliary/compact.md").read_text()
    assert all(word in compact_help for word in ("Compacting", "spinner", "Esc twice", "waits in order"))
    troubleshooting = (package / "troubleshooting.md").read_text()
    assert all(word in troubleshooting for word in ("waiting to be sent", "provider received", "Compacting", "continues that resume"))
    assert all("Source authorities" not in p.read_text() for p in docs)
    assert not (user / ".rune").exists(), "explicit RUNE_HOME stays isolated"
    run("-skills", "disable", help_skill["id"])
    listed = json.loads(run("-skills", "list"))
    assert next(s for s in listed["skills"] if s["id"] == help_skill["id"])["disabled"]
    run("-skills", "enable", help_skill["id"])
    print("PASS fresh home, standalone binary, official discovery/show and disable/enable")

    # Owner, 2026-10-30: an install without a scope stays with its project,
    # never mixed into every other one; an update keeps nothing beside it; and
    # the official help's name is never taken, by an install or by a copy.
    def write_skill(folder, name, text):
        folder.mkdir(parents=True)
        (folder / "SKILL.md").write_text(f"---\nname: {name}\ndescription: Fixture guide\n---\n{text}\n")
    source = base / "sources" / "team-style"
    write_skill(source, "team-style", "Follow the team style.")
    installed = json.loads(run("-skills", "install", str(source)))
    assert installed["scope"] == "project" and "approval_required" not in installed, installed
    assert os.path.samefile(installed["installed"], project / ".rune" / "skills" / "team-style")
    assert not (home / "skills").exists(), "nothing was offered to every other project"
    updated = json.loads(run("-skills", "update", str(source)))
    assert updated["replaced"] is True
    assert [p.name for p in (project / ".rune" / "skills").iterdir()] == ["team-style"], "an update keeps no copy beside it"
    personal = json.loads(run("-skills", "install", str(source), "--scope", "user"))
    assert personal["scope"] == "user"
    assert os.path.samefile(personal["installed"], home / "skills" / "team-style")
    official_name = help_skill["name"]
    write_skill(base / "sources" / official_name, official_name, "Not the official help.")
    for scope in ("user", "project"):
        run("-skills", "install", str(base / "sources" / official_name), "--scope", scope, ok=False)
    assert not (home / "skills" / official_name).exists()
    write_skill(project / ".rune" / "skills" / official_name, official_name, "Not the official help.")
    named = [s for s in json.loads(run("-skills", "list"))["skills"] if s["name"] == official_name]
    assert sorted((s["id"] == help_skill["id"], s["shadowed"]) for s in named) == [(False, True), (True, False)], named
    assert "Rune user settings guide" in json.loads(run("-skills", "show", official_name))["content"]
    shutil.rmtree(project / ".rune")
    shutil.rmtree(home / "skills")
    print("PASS installs stay with their project unless --scope user, updates keep no copy, the official name is never taken")

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
    assert json.loads((upgraded / "version.json").read_text())["version"] == current_format
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
