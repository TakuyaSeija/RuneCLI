#!/usr/bin/env python3
"""Keep Studio inputs visible and local/private artifacts out of Git.

The probes use Git itself without creating files or modifying the index.
Only the Python standard library is required.
"""

import argparse
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]

# A policy that passes in an empty repo does not prove the project is complete.
REQUIRED = (
    ".gitignore", ".gitattributes", ".editorconfig", "rust-toolchain.toml",
    "README.md", "AGENTS.md", "CLAUDE.md",
    "docs/git.md", "docs/architecture.md", ".github/workflows/studio-ci.yml",
    "scripts/check_git_policy.py", "scripts/test_git_policy.py",
    "Cargo.toml", "Cargo.lock", ".cargo/config.toml",
    "Engine/Cargo.toml", "Engine/Schema.sql", "Engine/src/lib.rs",
    "Cli/Cargo.toml", "Cli/src/main.rs",
)
INSTRUCTION_BUDGET = 32768

IGNORED = (
    "target/debug/rune",
    "target/test-tmp/fixture/session.jsonl",
    "target/tmp/provider-request-audit/history.json",
    "Engine/target/release/app",
    "tmp/1.txt",
    "tmp/2.txt",
    "tmp/nested/notes.md",
    "Cli/tests/__pycache__/fixture.cpython-313.pyc",
    "Cli/tests/fixture.pyc",
    ".venv/bin/python",
    ".pytest_cache/state",
    ".coverage",
    ".rune/projects/fixture/memory/MEMORY.md",
    ".rune/run/session_1.lock",
    ".rune/rune.toml",
    "rune.toml",
    "rune.db",
    "rune.db-wal",
    "rune.db-shm",
    "master.key",
    "secrets.vault",
    "secrets.lock",
    ".env",
    ".env.local",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    "private.pem",
    "private.key",
    "private.p12",
    "private.pfx",
    "id_rsa",
    "id_ed25519",
    ".claude/settings.local.json",
    ".claude/scheduled_tasks.lock",
    ".claude/projects/session.jsonl",
    ".claude/history.jsonl",
    ".claude/.credentials.json",
    ".claude/cache/state.json",
    "nested/.rune/run/session_2.key",
    "nested/.cargo/credentials.toml",
    ".vscode/settings.json",
    ".idea/workspace.xml",
    "Engine/src/example.rs.bk",
    "Engine/src/example.rs.swp",
    "Engine/src/example.rs~",
    "default.profraw",
    ".DS_Store",
    "Thumbs.db",
)

KEPT = (
    ".gitignore",
    ".gitattributes",
    "README.md",
    "AGENTS.md",
    "CLAUDE.md",
    "LICENSE",
    "docs/git.md",
    "scripts/check_git_policy.py",
    "scripts/test_git_policy.py",
    ".github/workflows/studio-ci.yml",
    "Cargo.toml",
    "Cargo.lock",
    ".cargo/config.toml",
    "Engine/Cargo.toml",
    "Engine/Schema.sql",
    "Cli/Cargo.toml",
    "Cli/tests/Questions.json",
    "Cli/tests/deltas.json",
    "Engine/tests/fixtures/sanitized.jsonl",
    "Engine/tests/fixtures/sanitized.log",
    ".env.example",
    ".env.development.example",
    ".env.sample",
    ".env.template",
    ".rune/skills/example/SKILL.md",
    ".rune/skills/example/assets/example.png",
    "nested/.rune/skills/example/SKILL.md",
    ".claude/settings.json",
)


def git(*args, input_paths=None, accepted=(0,)):
    data = None if input_paths is None else "".join(p + "\0" for p in input_paths)
    result = subprocess.run(
        ["git", *args], cwd=ROOT, input=data, text=True, capture_output=True
    )
    if result.returncode not in accepted:
        raise RuntimeError(result.stderr.strip() or f"git exited {result.returncode}")
    return result.stdout


def ignored(paths):
    if not paths:
        return set()
    output = git(
        "check-ignore", "--no-index", "-z", "--stdin",
        input_paths=paths, accepted=(0, 1)
    )
    return set(filter(None, output.split("\0")))


def attributes(paths):
    values = git(
        "check-attr", "-z", "--stdin", "text", "eol", "diff", "merge",
        input_paths=paths
    ).split("\0")
    result = {}
    for offset in range(0, len(values) - 1, 3):
        path, name, value = values[offset:offset + 3]
        result.setdefault(path, {})[name] = value
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check-tracked", action="store_true",
        help="also reject tracked files that violate the ignore policy"
    )
    args = parser.parse_args()
    try:
        git_root = Path(git("rev-parse", "--show-toplevel").strip()).resolve()
    except RuntimeError as error:
        print(f"Git policy FAILED: {error}", file=sys.stderr)
        return 1
    if git_root != ROOT:
        print(f"Git policy FAILED: repository root must be {ROOT}, not {git_root}",
              file=sys.stderr)
        return 1
    errors = []

    missing = set(IGNORED) - ignored(IGNORED)
    errors.extend(f"local/private file is not ignored: {p}" for p in sorted(missing))

    errors.extend(f"required project file is missing: {p}" for p in REQUIRED
                  if not (ROOT / p).is_file())
    agents, claude = ROOT / "AGENTS.md", ROOT / "CLAUDE.md"
    if agents.is_file() and claude.is_file():
        if agents.read_bytes() != claude.read_bytes():
            errors.append("AGENTS.md and CLAUDE.md are not synchronized")
        if len(agents.read_text(encoding="utf-8")) > INSTRUCTION_BUDGET:
            errors.append("AGENTS.md exceeds the 32768-character opening-context budget")
    source = set(KEPT) | set(REQUIRED)
    for folder in ("Engine/src", "Engine/tests", "Cli/src", "Cli/tests", "Cli/Assets",
                   ".github", "docs", "scripts"):
        for path in (ROOT / folder).rglob("*"):
            if (path.is_file() and "__pycache__" not in path.parts
                    and path.suffix not in {".pyc", ".pyo", ".swp", ".swo", ".bk"}
                    and not path.name.endswith("~")):
                source.add(path.relative_to(ROOT).as_posix())
    source.add("Cli/verify_steer_e2e.py")
    errors.extend(f"project input is ignored: {p}" for p in sorted(ignored(source)))
    # Do not traverse ignored runtime/build trees or a nested repository itself.
    for directory, folders, files in os.walk(ROOT):
        rel = Path(directory).relative_to(ROOT)
        if rel.parts and ".git" in (*folders, *files):
            errors.append(f"nested Git repository is not allowed: {rel.as_posix()}")
        folders[:] = [name for name in folders if name not in
                      {".git", "target", "tmp", "__pycache__", ".venv", "venv"}]
    for entry in git("ls-files", "--stage", "-z").split("\0"):
        if entry.startswith("160000 "):
            errors.append(f"submodule requires an explicit reviewed policy: {entry.split(chr(9), 1)[-1]}")

    checks = {
        "Engine/src/lib.rs": {"text": "auto", "eol": "lf"},
        "Cargo.lock": {"text": "auto", "eol": "lf"},
        "README.md": {"text": "auto", "eol": "lf"},
        "example.cmd": {"text": "set", "eol": "crlf"},
        "Cli/Assets/CyanLogo.png": {"text": "unset", "diff": "unset", "merge": "unset"},
        "Cli/Assets/CyanLogo16.rgba": {"text": "unset", "diff": "unset", "merge": "unset"},
    }
    actual = attributes(checks)
    for path, expected in checks.items():
        for name, value in expected.items():
            if actual.get(path, {}).get(name) != value:
                errors.append(f"wrong Git attribute: {path} {name} (expected {value})")

    if args.check_tracked:
        tracked = list(filter(None, git("ls-files", "-z").split("\0")))
        errors.extend(f"ignored file is already tracked: {p}" for p in sorted(ignored(tracked)))

    if errors:
        print("Git policy FAILED:", file=sys.stderr)
        for error in errors:
            print(f"  - {error}", file=sys.stderr)
        return 1
    print(f"Git policy PASS: Studio repository root, {len(IGNORED)} exclusion probes, "
          f"{len(source)} project inputs, {len(checks)} attribute probes.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
