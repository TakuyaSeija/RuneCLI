# AGENTS.md
# Project: Rune (Atoryn Studio)
# Purpose: Persistent workspace rules for AI coding agents. Keep synchronized with CLAUDE.md.

> **Source of Truth**: `docs/` (`architecture.md`, `deploy.md`, `website.md`, `git.md`) are references, not absolute truth—they may contain outdated or immature architecture. **Truth is in the source code.**

---

## 1. Quick Commands

### Build & Run
- Build workspace: `cargo build --workspace`
- Release build (Linux Docker, background): `scripts/build_linux_release.sh [gnu|musl]`
- Runtime config / storage: `rune -config check|explain` | `rune -storage [list|prune]`

### Testing
- Full suite: `cargo test --workspace --no-fail-fast`
- Single crate/test: `cargo test -p runeengine <filter>` (or `-p runecli`)
- Cross-check targets: `scripts/xcheck.sh <triple> clippy ...`
- Git policy check: `python3 scripts/check_git_policy.py --check-tracked && python3 -B scripts/test_git_policy.py`

### Lint & Format
- Linter: `cargo clippy --workspace --all-targets -- -D warnings`
- Formatter: `cargo fmt --all -- --check`

---

## 2. Architecture & Invisible Rules

- **Stack & Crates**: Rust workspace (`Engine/` = `runeengine` core/tools/security/store; `Cli/` = `runecli` UI/`-p`/`-host`).
- **Deep Contracts**: Read `docs/architecture.md` before changing engine, protocol, provider, store, sandbox, session, UI, or classifier. Read `docs/website.md` before changing `Website/`. Read `docs/deploy.md` before running `DeployStudio.sh`.
- **Single Security Gates** (no bypasses): `perm::Gate` (tools), `ToolCtx::write_guard` (file writes), `net` (network), `platform/` (OS-specific code via `platform::boundary`).
- **Static vs Dynamic**: Prompt/tool schemas are static (`action` is always first field; use `field_given_str` for optional text). Live values are append-only context rows. Never estimate tokens from bytes/chars.
- **Hot-Reload Config**: All defaults live in `FIELDS` (`settings/Configuration.rs`), never hardcoded fallbacks. `FIXED` keys are constants—never written to `rune.toml` or documented publicly.
- **Upgrades**: Stored-shape changes require `upgrade/vN.rs` + `VERSION` bump + `every_older_format_reaches_this_build` test. No runtime legacy fallback.
- **Dependencies**: Keep in-tree implementations (glob, regex, walk, pty, landlock, SSE). Only add crates listed/approved in `Cargo.toml`.
- **No Emojis**: Forbidden in production code, comments, and messages (technical UI glyphs allowed).

---

## 3. Workflow & Release Efficiency

1. **Read Memo First**: Check `@memory/MEMORY.md` (and relevant notes) before starting; update it after audits/changes.
2. **Efficient Release (CI > Local)**:
   - Never duplicate work: if GitHub Actions (`crossplatform` with `[ci macos]`) already builds/releases version `X`, do **not** rebuild `X` locally.
   - Flow: green local gate → push `crossplatform` `[ci macos]` (version untouched) → wait for all `studio-ci.yml` jobs green → bump version → publish via `Ops/Release/PublishRune.sh --ci-run <run-id>` → install `~/.local/bin/rune`.
   - **Owner tests first**: a local release build installed to `~/.local/bin/rune` is allowed, but never publish (DEV or PROD Releases) or deploy the website until the owner has tested it personally and says so.
   - Versioning: `0.Y.Z` (`Z` 0–9, `Y` 0–99). Layered changelog required in all 10 languages.
3. **Never Edit Profiles**: Installers never modify shell profiles; link/place binary in a writable `PATH` dir.

---

## 4. Behavioral Guardrails & Owner Rules

- **Ambiguous → Ask**: Do not guess on uncertain requirements or user-owned decisions.
- **Many Problems, One Plan**: If a task has multiple issues and some are open/undecided, fix none yet—report the full set and wait for approval.
- **Regression Test**: Every fix requires a regression test and running affected tests.
- **Isolated Test Data**: Tests must use `target/test-tmp/` or `RUNE_HOME=$(mktemp -d)`—never mutate live `~/.rune/` or read/print `master.key` / `secrets.vault`.
- **Speak Owner's Language**: Match the language of the owner's prompt.

---

## 5. Git & Known Pitfalls

- **Git Boundary**: `Studio/` is the standalone repo root (`https://github.com/TakuyaSeija/RuneCLI.git`); parent `Atoryn/` must stay free of Studio Git/config. Never silently stash, reset, commit, or push. See `docs/git.md`.
- **Known Parallel Flakes**: `store::drafts` and `session_ipc` child-host tests (1s socket wait) — rerun alone before calling a regression. Under high load (≳ 8), `agent.rs` 10s-wait tests may flake.
- **Engine Project Instructions**: Engine reads `AGENTS.md` (`AGENTS_FILE` in `session/prompt.rs`), not `CLAUDE.md`.
