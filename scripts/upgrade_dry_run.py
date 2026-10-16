#!/usr/bin/env python3
"""Release gate: the new build upgrades real homes, not only test fixtures.

1. A copy of a real home (default ~/.rune): credentials, locks, runtime and
   backups are never copied; the database is copied through SQLite's backup
   API (it is in WAL mode). The copy is upgraded by the new build, then
   checked: format, configuration, choices kept.
2. A home written natively by every older Rune binary given (or found in
   ~/.local/share/RuneBackups): each starts a fresh isolated home, sets a few
   choices it knows, and the new build takes it over.

Nothing outside the scratch folder is written; the real home is only read.
Usage: python3 scripts/upgrade_dry_run.py [--bin target/release/rune]
       [--home ~/.rune] [--old BIN ...] [--no-old]
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import sqlite3
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
NEVER = {"master.key", "secrets.vault", "secrets.lock", "Configuration.lock",
         "run", "backups", "product-skills"}


def copy_home(real: Path, into: Path) -> None:
    into.mkdir(parents=True)
    for entry in real.iterdir():
        name = entry.name
        if name in NEVER or name.endswith(".lock") or name.startswith("rune.db"):
            continue
        if entry.is_dir() and not entry.is_symlink():
            shutil.copytree(entry, into / name, symlinks=True,
                            ignore=shutil.ignore_patterns("*.lock", "*.sock", "*.key"))
        elif entry.is_file():
            shutil.copy2(entry, into / name)
    db = real / "rune.db"
    if db.is_file():
        source = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        target = sqlite3.connect(into / "rune.db")
        with target:
            source.backup(target)
        source.close()
        target.close()


def run(binary: Path, home: Path, *args, check=True):
    env = dict(os.environ, RUNE_HOME=str(home), HOME=str(home.parent), NO_COLOR="1")
    env.pop("RUNE_HANDOVER", None)
    result = subprocess.run([str(binary), *args], cwd=home.parent, env=env, text=True,
                            capture_output=True, timeout=600)
    if check and result.returncode != 0:
        raise SystemExit(f"FAIL {binary} {' '.join(args)}:\n{result.stdout}\n{result.stderr}")
    return result


def fmt(home: Path) -> int:
    return json.loads((home / "version.json").read_text())["version"]


CHOICES = [("ui.theme", "cyan"), ("retry.max_attempts", "3"),
           ("terminal.command_timeout", "90s")]


def in_force(binary: Path, home: Path) -> dict:
    """What each chosen key holds in force, as that build explains it."""
    held = {}
    for key, _ in CHOICES:
        said = run(binary, home, "-config", "explain", key, check=False)
        first = said.stdout.splitlines()[0] if said.returncode == 0 and said.stdout else ""
        if first.startswith(key):
            held[key] = first.split("=", 1)[1].split("(")[0].strip().strip('"')
    return held


def upgrade(new: Path, home: Path, label: str, old: Path | None = None) -> None:
    before = fmt(home) if (home / "version.json").exists() else None
    chosen = in_force(old, home) if old else {}
    config = home / "rune.toml"
    choices = config.read_text() if config.exists() else None
    ready = run(new, home, "-upgrade-check", check=False)
    if ready.returncode != 0:
        raise SystemExit(f"FAIL {label}: the new build refuses to take it over:\n{ready.stderr}")
    expected = next(int(l.split()[1]) for l in ready.stdout.splitlines() if l.startswith("format "))
    started = run(new, home, "-config", "check")
    left = [l for l in started.stderr.splitlines() if "left as it was" in l]
    after = fmt(home)
    if after != expected:
        raise SystemExit(f"FAIL {label}: format {after}, expected {expected}")
    run(new, home, "-settings")
    now = in_force(new, home)
    lost = {k: (v, now.get(k)) for k, v in chosen.items() if now.get(k) != v}
    if lost:
        raise SystemExit(f"FAIL {label}: a choice changed (before, after): {lost}")
    changed = config.exists() and choices is not None and config.read_text() != choices
    print(f"PASS {label}: format {before} -> {after}, {len(chosen)} choice(s) kept"
          + (", rune.toml rewritten (review below)" if changed else ", rune.toml unchanged")
          + (f", {len(left)} item(s) left as they were" if left else ""))
    for line in left:
        print("   ", line)
    if changed:
        diff = subprocess.run(["diff", "-u", "-", str(config)], input=choices, text=True,
                              capture_output=True)
        print(diff.stdout)


def old_binaries() -> list[Path]:
    store = Path.home() / ".local/share/RuneBackups"
    found = {}
    for path in sorted(store.glob("*/*")):
        if not path.is_file() or path.stat().st_size < 5_000_000 or not os.access(path, os.X_OK):
            continue
        try:
            version = subprocess.run([str(path), "-version"], capture_output=True, text=True,
                                     timeout=10).stdout.split()[-1]
        except (OSError, subprocess.SubprocessError, IndexError):
            continue
        found.setdefault(version, path)
    return [found[v] for v in sorted(found, key=lambda v: tuple(int(p) for p in v.split(".")))]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", default=str(ROOT / "target/release/rune"))
    parser.add_argument("--home", default=str(Path.home() / ".rune"))
    parser.add_argument("--old", nargs="*", default=None)
    parser.add_argument("--no-old", action="store_true")
    args = parser.parse_args()
    new = Path(args.bin).resolve()
    scratch_root = ROOT / "target" / "test-tmp"
    scratch_root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="UpgradeDryRun-", dir=scratch_root) as temp:
        temp = Path(temp)
        real = Path(args.home).expanduser()
        if real.is_dir():
            copy = temp / "real" / ".rune"
            copy_home(real, copy)
            upgrade(new, copy, f"copy of {real}")
        if args.no_old:
            return
        binaries = [Path(p) for p in args.old] if args.old is not None else old_binaries()
        for old in binaries:
            version = run(old, temp, "-version", check=False).stdout.split()[-1]
            home = temp / f"old-{version}" / ".rune"
            home.mkdir(parents=True)
            # Written by that build itself: its marker, layout and choices.
            run(old, home, "-config", "check", check=False)
            for key, value in CHOICES:
                run(old, home, "-config", "set", key, value, check=False)
            if not (home / "version.json").exists():
                print(f"SKIP {version}: it wrote no home")
                continue
            upgrade(new, home, f"home written by {version}", old)


if __name__ == "__main__":
    sys.exit(main())
