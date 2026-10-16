#!/usr/bin/env python3
"""Exercise Git policy failures in isolated repos, never the owner's index."""

from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


class GitPolicyTests(unittest.TestCase):
    def setUp(self):
        base = ROOT / "target/test-tmp"
        base.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(prefix="GitPolicyRegression.", dir=base)
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        self.copy_policy(self.repo)
        subprocess.run(["git", "init", "-q", "-b", "main"], cwd=self.repo, check=True)

    def copy_policy(self, destination):
        (destination / "scripts").mkdir(parents=True)
        for name in (
            ".gitignore", ".gitattributes", ".editorconfig", "rust-toolchain.toml",
            "README.md", "AGENTS.md", "CLAUDE.md",
            "docs/git.md", "docs/architecture.md", ".github/workflows/studio-ci.yml",
            "scripts/check_git_policy.py", "scripts/test_git_policy.py",
            "Cargo.toml", "Cargo.lock", ".cargo/config.toml",
            "Engine/Cargo.toml", "Engine/Schema.sql", "Engine/src/lib.rs",
            "Cli/Cargo.toml", "Cli/src/main.rs",
        ):
            (destination / name).parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / name, destination / name)

    def check(self, success, marker="", repo=None):
        result = subprocess.run(
            [sys.executable, "scripts/check_git_policy.py", "--check-tracked"],
            cwd=repo or self.repo, text=True, capture_output=True
        )
        self.assertEqual(result.returncode, 0 if success else 1,
                         result.stdout + result.stderr)
        if marker:
            self.assertIn(marker, result.stderr)

    def test_clean_standalone_repository_passes(self):
        self.check(True)

    def test_cargo_lock_must_remain_visible(self):
        with (self.repo / ".gitignore").open("a") as file:
            file.write("\nCargo.lock\n")
        self.check(False, "project input is ignored: Cargo.lock")

    def test_build_output_must_be_ignored(self):
        path = self.repo / ".gitignore"
        path.write_text(path.read_text().replace("\ntarget/\n", "\n"))
        self.check(False, "local/private file is not ignored: target/debug/rune")

    def test_tmp_rule_must_be_relative_to_studio_repo(self):
        path = self.repo / ".gitignore"
        path.write_text(path.read_text().replace("\n/tmp/\n", "\n/Studio/tmp/\n"))
        self.check(False, "local/private file is not ignored: tmp/1.txt")

    def test_rgba_assets_must_stay_binary(self):
        path = self.repo / ".gitattributes"
        path.write_text(path.read_text().replace("*.rgba binary\n", ""))
        self.check(False, "wrong Git attribute: Cli/Assets/CyanLogo16.rgba")

    def test_force_staged_local_environment_is_rejected(self):
        (self.repo / ".env").write_text("EXAMPLE=synthetic-placeholder\n")
        subprocess.run(["git", "add", "-f", ".env"], cwd=self.repo, check=True)
        self.check(False, "ignored file is already tracked: .env")

    def test_local_cargo_credentials_do_not_become_project_inputs(self):
        (self.repo / ".cargo/credentials.toml").write_text("synthetic-placeholder\n")
        self.check(True)

    def test_required_source_file_cannot_be_missing(self):
        (self.repo / "Cargo.toml").unlink()
        self.check(False, "required project file is missing: Cargo.toml")

    def test_instruction_files_must_stay_synchronized(self):
        with (self.repo / "CLAUDE.md").open("a") as file:
            file.write("\nDifferent instructions\n")
        self.check(False, "AGENTS.md and CLAUDE.md are not synchronized")

    def test_instruction_injection_must_not_truncate_owner_rules(self):
        for name in ("AGENTS.md", "CLAUDE.md"):
            (self.repo / name).write_text("x" * 32769)
        self.check(False, "AGENTS.md exceeds")

    def test_nested_repository_is_rejected(self):
        subprocess.run(["git", "init", "-q", "-b", "main"],
                       cwd=self.repo / "Engine", check=True)
        self.check(False, "nested Git repository is not allowed: Engine")

    def test_shared_documentation_cannot_be_ignored(self):
        with (self.repo / ".gitignore").open("a") as file:
            file.write("\ndocs/\n")
        self.check(False, "project input is ignored: docs/architecture.md")

    def test_git_at_parent_instead_of_studio_is_rejected(self):
        subproject = self.repo / "Studio"
        self.copy_policy(subproject)
        self.check(False, "repository root must be", repo=subproject)


if __name__ == "__main__":
    unittest.main(verbosity=2)
