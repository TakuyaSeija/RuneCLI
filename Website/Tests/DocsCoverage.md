# Website documentation contract

Public reference version: Rune 0.11.9 (`documentedVersion` in `Server/Docs.go`). It states the build the manual was checked against, not a second application version; `DocsFacts_test.go` fails when the workspace version moves on, so each release's user-visible changes are reviewed in the manual before the number changes.

## Sources of truth

- `Cli/src/args.rs` (`USAGE`): command-line syntax and flags; every command on the help screen must appear in the command-line reference.
- `Engine/src/settings/providers.rs` (`PRESETS`): every model provider preset, its key hint and address.
- `Engine/src/web/*.rs` (`ADAPTER`): every web service and whether it searches, fetches or both.
- `Cli/src/ui/keys.rs` and `editing.rs`: slash commands and canonical keys.
- `Engine/src/settings/Configuration.rs`: factory defaults and value types.
- `.rune/skills/rune-settings-guide/`: current user-facing navigation, scope, reset, effects and risks.
- `docs/architecture.md`: public architecture statements must agree, without publishing private deployment details or engine implementation paths.
- Actual installed binary in an isolated data home: `-help`, `-version`, `-config defaults`, `-config explain <key>` and a scripted PTY session.

The settings reference (`Pages/Docs/Reference.html`) is generated, never edited by hand: `python3 -I Website/Tools/build_settings_reference.py --bin <rune>` reads every key from the field table, its factory value from the table, from the binary's `-config explain` in a throwaway home and from the official help's reference tables, and stops on any disagreement. The explanation and the moment a change applies are the binary's words; accepted values are the help's. An explanation that names an internal the public site never shows is replaced by the help's own purpose line for that key, and the script names those keys.

No real account is needed for the website tests. Do not run provider/doctor tests on the owner's home to verify documentation.

## Interactive terminal coverage

- `Tools/test_session_data.py`: complete public rows, row-preserving redaction, exact saved model capacity, authoritative CLI lexer/diff and palette, no discarded fold tails.
- `Server/Demo_test.go`: all 145 requests and 207 calls, all prose including text-only requests, original failures, final accounting, 64 file snapshots, native disclosure and escaped markup.
- `Tests/TerminalTest.mjs`: actual Chrome clicks across the full Thinking hit area, default cursor, no pointer focus box, content-only drag-copy with cleared highlight, scrolling/disclosure without auto-pause, smart pinned PLAN/FILES heading, borderless last-row Jump, all approval cards and final totals. Its external CDP rAF fixture (`ReplayClock.mjs`) drives the excerpt's own schedule (six chapters, about 80 seconds) without a production speed override.
- `Tests/OverviewTest.mjs`: the centred hero (title lines widening, lead wider than the title, no image), the single curl command, a uniformly scaled 960×600 canvas above 920px, the terminal at its own size on tablets, the still final report instead of the replay on phones, seven responsive widths.
- `Tests/BrowserTest.mjs`: all articles, merged install redirects and supporting pages at five widths, contrast/overflow, link behavior, the language menu and a language choice keeping the reading position exactly, real-clock Pause/no-auto-pause, no-JS and reduced motion.
- `Tests/TerminalPolishTest.mjs`: SVG visibility, exact neutral effort color across Amber/Violet/Cyan, redacted ports, failure bullets and native no-JS disclosure.

The public illustration retains Ask-mode simulated approvals by owner choice. Its source session ran in Full mode; a compact label and tooltip say so. Context capacity is 1,000,000 from the exact model's saved metadata, not an historical maximum claim. Redactions are labelled and no model request is made by a replay. The excerpt shows six chapters of the 35-minute session, paced for reading (prose streams whole words, then holds for most of its reading time), and says that its streaming, pacing and approvals are simulated. Phones show the session's own final report instead, unchanged and still.

## Changelog

`Pages/Changelog.html` lists every published build, newest first, and its sidebar groups the releases by minor version (`0.11`, `0.10`, ...) with each group's versions inside it — one or ten of them, however many were published under that minor. A version whose changes were recorded shows them in three layers — a title naming the change, a paragraph saying what changed and why, and short detail lines to scan (owner, 2026-11-06) — and a version published only so an older build stays installable says that what it changed was not recorded, in every language, rather than inventing a line.

## Article coverage

The catalogue in `Server/Docs.go` owns routes, sidebar ordering and pagination. An article is one `Pages/Docs/*.html` template with stable plain-text h2 IDs. Startup validates those headings and derives the table of contents.

| Area | Articles |
| --- | --- |
| Getting started | Introduction, Install & update (Linux, macOS, Windows), Platform support, Interface, Sessions & context |
| Everyday use | Slash commands, CLI reference, Keyboard & mouse, Manual commands, Instructions & attachments, Review/undo/redo |
| Settings | Overview, Configuration file, Settings reference (generated), Connections, Model providers, Models, Generation, Auxiliary models, Runtime, Tools, Terminal, Web (with every web service), Appearance |
| Extensions | Skills, MCP |
| Trust | Security, Classifier, Privacy, Credentials, Release verification, Architecture, Troubleshooting |

This is detailed product documentation, not an exhaustive generated dump of every advanced setting or external provider API. For any new key/command, update its relevant article and regression test; retain the installed `-help`/`explain` route for exact version-specific behavior.

## Automated checks

Run from `Website/`:

```
go test ./...
go vet ./...
```

`Server/DocsFacts_test.go` checks the manual against the application: the reference version against the workspace version, the settings reference against every field and its factory value, the provider table against every preset, the web table against every service with its search/fetch support, and the command-line reference against every command on the help screen. Each check was shown to fail on a deliberately wrong fact. `Server/License_test.go` holds the License page to the shipped `LICENSE.txt`, word for word, where that file is present. `Server/Docs_test.go` checks routing, native grouped navigation, heading contracts, all local links/fragments, version labeling, honest demo wording and exactly one curl command. `Server_test.go` retains security headers, request boundaries, signed-catalog and installer end-to-end tests. `Tools/build_brand_assets.py` derives every served brand file from the owner's originals in `Website/Brand/` (`--check` proves the files on disk are exactly its output). `Server/Brand_test.go` holds the owner's logo (2026-10-31): two marks on every page, the retired glyph gone, each tab size and the touch icon present, every icon served as an immutable PNG, and each file's real dimensions and transparency. Public product file names are allowed in documentation; private machine/deployment details remain forbidden.

Start an isolated local website server, then run from the repository root:

```
node Website/Tests/BrowserTest.mjs <origin> target/test-tmp/website-browser
```

For the published-state browser audit, export signed-test-release HTML through `TestExportPublishedBrowserFixture` with `WEBSITE_EXPORT_FIXTURE=<absolute-dir>`, then pass that directory as `WEBSITE_FIXTURE_DIR` to the same browser test. CDP overrides only the three catalog-dependent documents (Home, Changelog, Verification); actual hashed assets still come from the local server. This does not publish a release or modify deployment data.

The Chrome test keeps its profile/temp data in `target/test-tmp`, with sandbox enabled. It audits every article and supporting page at 320/390/768/1024/1440 px; checks contrast, layout, headings and local fonts; exercises sidebar, TOC, redirect, copy, centre-out underlines, default amber theme, terminal macOS window controls, Pause/Replay, fixed-height permission transitions, reduced motion and JavaScript-disabled content. Screenshots and JSON reports are test artifacts, not website assets.

The terminal is a **replay of one recorded session**, not a captured benchmark and not an invented script. `UI/Data/Session655.json` is generated by `Tools/build_session_data.py` from that session's own `session.jsonl`, `toolcalls.jsonl`, `tasks.json`, `meta.json`, `token_usage` and `session_files`; the builder refuses to write the file if any line still names a host, a home path or an internal module. `Server/Demo.go` lays those facts out the way `Cli/src/ui/draw.rs` does — a step is `  ● heading` with its body from cell 6, a closed box is at most 76 columns with its label interrupting the top edge, a command is one truncated row, a created file's box is labelled by its language — so the page cannot drift from the binary by being drawn twice.

The page is complete before any script runs: the whole transcript and the session's final accounting are server-rendered, and a reader without JavaScript sees the session as it ended. The replay only decides what is visible at a moment. Session figures change when a request settles, never on a timer, because that is when the engine reports them.

The transcript and plan/file panel scroll independently. Reading away from the newest row holds the reader's place, never the replay clock; only explicit Pause or offscreen/hidden stops playback. Clicks, folds, drag selection, hover and focus never auto-pause. Sidebar keeps SESSIONS fixed and changes the pinned lower heading from PLAN to FILES CHANGED as the active section changes, with above/below row markers and a continuous rule. Jump is a borderless last-row label, not a floating card.

Its logo is the three original 11×16 RGBA theme assets represented as SVG paths. The session ran in Full mode; the replay shows approval cards the way Ask mode would ask them, and says so.

## Installers and releases

- `UI/Install/install.sh` (Linux, macOS) and `UI/Install/install.ps1` (Windows) are served as plain text, byte for byte what the commands run. `Server_test.go` runs `install.sh` end to end on Linux and macOS (macOS CI with the system's LibreSSL) and `install.ps1` in Windows PowerShell 5.1 and PowerShell 7 (Windows CI), including both DER signature edge cases, a tampered archive and a foreign signature.
- Releases are signed with ECDSA P-256 over SHA-256 (`Ops/Release/Sign`), checked by `openssl dgst -sha256 -verify` and by Windows CNG. The catalog accepts `rune-<v>-linux-<arch>-<libc>.tar.gz`, `rune-<v>-macos-<arch>.tar.gz` and `rune-<v>-windows-<arch>.zip`.
- CI installs the binaries it built through the real installers on Linux (x86_64 glibc and musl, arm64), macOS (Apple silicon, Intel) and Windows (x64 in both PowerShells, ARM64), from a release staged and signed for that job only (`Tools/stage_test_release.py`).
