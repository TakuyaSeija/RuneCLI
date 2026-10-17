//! Escape battery — a regression net for what an approved command can still do.
//!
//! Every command here is **approved by the user**: the harness answers every
//! permission prompt with "allow once". So this file does not test the
//! permission gate. It tests what is left when the gate has already said yes —
//! since 2026-09-28 that is **only the kernel sandbox** (and Landlock), because
//! the gate no longer refuses anything on its own: the owner's rule is that the
//! system never says no by itself. The catastrophic deny-list class and the
//! sensitive paths (`.ssh`, `.bashrc`, `.cargo/config.toml`) are now wizard
//! questions — the battery approves, so they run, and a sensitive candidate
//! drops confinement for that one call so the answer is never a lie about what
//! it opened.
//!
//! # What changed when paths became answerable (2026-08-31)
//!
//! An allow used to mean "run this command" and nothing else, so a case that
//! named a directory outside the workspace was stopped by the kernel however the
//! user answered. That was also the bug: a person who allowed `ls ~/Downloads`
//! watched it fail anyway. A prompt now names the paths it would open, and an
//! allow opens them — which is why several cases below moved from `Stopped` to
//! `Runs`. Each of those is a directory the user was shown by name.
//!
//! # What changed when refusing became answering (2026-09-28)
//!
//! The cases below used to be `Refused` — the engine decided alone and no
//! prompt could change that. They are `Runs` now: asked, approved, and executed
//! with the card's promise kept. A `$HOME` is faked for the whole battery so
//! the approved commands exercise the real code against throwaway fixtures —
//! the battery must never write the owner's actual home.
//!
//! Each case carries what it is *expected* to do today, including the ones that
//! are expected to succeed. Those are not oversights: they are the accepted
//! surface, written down, so that closing one later is a deliberate change to
//! this file rather than a surprise. Run with `--nocapture` to read the table.

use std::path::PathBuf;
use std::sync::Arc;

use runeengine::engine::{Config, Engine};
use runeengine::proto::{Command, Decision, Event};
use runeengine::provider::scripted::Scripted;
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Expect {
    /// The command ran and succeeded. Accepted surface.
    Runs,
    /// The command ran and the kernel or the filesystem stopped it.
    Stopped,
    /// The engine stopped the call before any shell saw it — an input the call
    /// could not even name. No case expects this any more: the gate asks
    /// instead of refusing, so a battery command that reaches the gate always
    /// runs.
    Refused,
}

struct P {
    engine: Engine,
    events: std::sync::mpsc::Receiver<Event>,
    project: PathBuf,
}

/// The engine's own directory is deliberately **not** an ancestor of the
/// project.
///
/// It used to be: the probe put `rune.db`, `data/` and `project/` in one
/// directory, so the project's parent *was* the engine store. That was
/// invisible until path grants arrived and the parent-chain cases started being
/// refused for containing the engine's own files — the right answer to the
/// wrong question. A real install has the two far apart, and so does this now.
struct Layout {
    project: PathBuf,
    store: PathBuf,
}

fn layout() -> Layout {
    let id = std::process::id();
    let work = std::env::temp_dir()
        .join("rune-test")
        .join(format!("escape-{id}"));
    let store = std::env::temp_dir().join(format!("rune-escape-engine-{id}"));
    let _ = std::fs::remove_dir_all(&work);
    let _ = std::fs::remove_dir_all(&store);
    // The owner's question was "project at a/b/c/d — can the agent reach c, or
    // above?", so the chain is built here rather than borrowed from whatever
    // happens to be above the temp directory. Pointing `grandparent` at the
    // shared `/tmp/rune-test` made the case depend on how many stale directories
    // previous runs had left there: `ls` printed thousands of lines, the tool's
    // output cap cut the transcript, and the exit marker the check looks for went
    // with it. A case that fails by history is not a case.
    let project = work.join("a/b/c/d");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&store).unwrap();
    Layout { project, store }
}

fn probe() -> (P, PathBuf) {
    let l = layout();
    let cfg = Config::at(
        l.store.join("rune.db"),
        l.store.join("data"),
        Arc::new(Scripted::new(Vec::new())),
    )
    .unwrap();
    let (mut engine, events) = Engine::start(cfg).unwrap();
    engine.open(l.project.to_str().unwrap()).unwrap();
    (
        P {
            engine,
            events,
            project: l.project,
        },
        l.store,
    )
}

/// Returns (outcome, whether the user was asked, transcript).
fn shell(p: &mut P, command: &str) -> (Expect, bool, String) {
    p.engine.handle(Command::ToolInvoke {
        id: 1,
        session: 1,
        name: "terminal_use".into(),
        args: json!({ "action": "probe",
                      "command": command }),
    });
    let mut asked = false;
    loop {
        match p
            .events
            .recv_timeout(std::time::Duration::from_secs(40))
            .unwrap()
        {
            Event::PermissionRequest { req, .. } => {
                asked = true;
                p.engine.handle(Command::PermissionReply {
                    id: 2,
                    req,
                    decision: Decision::AllowOnce,
                });
            }
            Event::Ack {
                ok: false, error, ..
            } => {
                // The engine refused the call itself — report it rather than
                // waiting for a result that will never come.
                return (Expect::Refused, asked, error.unwrap_or_default());
            }
            Event::ToolResult {
                summary,
                content: body,
                ..
            } => {
                // A completed call can no longer be an engine refusal — the
                // gate asks instead. A failed exit is the kernel or the
                // filesystem having the last word, which is `Stopped`.
                let outcome = if summary.contains("ok") {
                    Expect::Runs
                } else {
                    Expect::Stopped
                };
                return (outcome, asked, format!("{summary} | {body}"));
            }
            _ => {}
        }
    }
}

/// The same question asked of the **file tools** rather than a shell.
///
/// The memory annex (2026-08-31) gave the jail a second permitted root, and a
/// second root is a second door. The shell cannot see memory at all — the kernel
/// sandbox never grants `~/.rune` — so this is the only path that can reach it,
/// and therefore the only path that can be talked into reaching past it.
///
/// `inspect_file` is a batch tool: one file failing does not fail the call, so
/// the verdict is in the body, not in `ok`. That distinction matters here —
/// "the jail refused" and "the file was not there" must never be read as the
/// same result, or a case could pass for having a typo in its path.
fn read_file(p: &mut P, path: &str) -> (Expect, String) {
    p.engine.handle(Command::ToolInvoke {
        id: 3,
        session: 1,
        name: "inspect_file".into(),
        args: json!({ "action": "probe", "path": path }),
    });
    loop {
        match p
            .events
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap()
        {
            Event::PermissionRequest { req, .. } => {
                p.engine.handle(Command::PermissionReply {
                    id: 4,
                    req,
                    decision: Decision::AllowOnce,
                });
            }
            Event::Ack {
                ok: false, error, ..
            } => return (Expect::Refused, error.unwrap_or_default()),
            Event::ToolResult {
                ok, content: body, ..
            } => {
                let outcome = if !ok || body.contains("error:") {
                    Expect::Stopped
                } else {
                    Expect::Runs
                };
                return (outcome, body);
            }
            _ => {}
        }
    }
}

#[test]
fn escape_battery() {
    // The cases name Linux's tools and paths (systemd units, `unshare`,
    // seccomp's refusals, `/etc`, `docker.sock`); macOS's and Windows's
    // confinement is held to the same lines by `sandbox.rs` and their
    // platform tests.
    if runeengine::platform::OS != runeengine::platform::Os::Linux {
        return;
    }
    // The battery is approved throughout, and since 2026-09-28 approved means
    // *runs* — including the sensitive class. It must never do that against
    // the owner's real home, so a throwaway `$HOME` is built here, before the
    // engine binds its jail, sandbox and forbidden list. The fixtures below
    // are what the cases expect to find; everything else about the fake home
    // is absent, exactly like a fresh account.
    let fake_home = std::env::temp_dir()
        .join("rune-test")
        .join(format!("escape-home-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&fake_home);
    for dir in [".ssh", ".cargo", ".cache", ".rune", "Project"] {
        std::fs::create_dir_all(fake_home.join(dir)).unwrap();
    }
    std::fs::write(fake_home.join(".bashrc"), "original\n").unwrap();
    std::fs::write(fake_home.join(".ssh/known_hosts"), "host\n").unwrap();
    std::fs::write(fake_home.join("Project/notes.txt"), "notes\n").unwrap();
    std::fs::write(fake_home.join(".rune/probe"), "rune\n").unwrap();
    std::env::set_var("HOME", &fake_home);
    let home = fake_home.display().to_string();

    let (mut p, store_root) = probe();
    let proj = p.project.display().to_string();

    // Removed first, deliberately. If the file survives from an earlier run the
    // rule anchors on the *file* rather than on /tmp, which is a grant a person
    // could reasonably give — and the case would then pass or fail by history
    // rather than by behaviour.
    let shared_tmp = format!("/tmp/.rune-probe-tmp-{}", std::process::id());
    let _ = std::fs::remove_file(&shared_tmp);

    // The owner's question: project at a/b/c/d — can the agent reach c, or above?
    let parent = p.project.parent().unwrap().display().to_string();
    let grandparent = p
        .project
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .display()
        .to_string();
    std::fs::write(PathBuf::from(&parent).join("parent-secret.txt"), "secret\n").unwrap();

    // This probe's own engine directory — its database and every project's
    // folder (memory, checkpoints, transcripts), and in a real install the
    // vault. No prompt may open it, which is why the case below names the real
    // one rather than a path that happens not to exist.
    let store_dir = store_root.display().to_string();
    // This project's folder under the engine's home, and the memory inside it —
    // the jail's annex. The index is written before any case runs, so a
    // `Stopped` on it can only be the kernel's answer, never a missing file.
    let folder = runeengine::project::find(&store_root, &p.project)
        .expect("the probe's project has its folder");
    let mem_dir = folder.join("memory").display().to_string();
    std::fs::write(folder.join("memory/MEMORY.md"), "- a note\n").unwrap();
    let folder = folder.display().to_string();

    let cases: Vec<(&str, String, Expect)> = vec![
        // ── the workspace itself ──────────────────────────────────────────
        ("write inside project", format!("echo ok > {proj}/inside.txt"), Expect::Runs),

        // ── the parent chain: a/b/c/d opened at d ─────────────────────────
        ("list parent (c)", format!("ls {parent}"), Expect::Runs),
        ("read file in parent (c)", format!("cat {parent}/parent-secret.txt"), Expect::Runs),
        ("read via ../ from project", format!("cd {proj} && cat ../parent-secret.txt"), Expect::Stopped),
        ("write into parent (c)", format!("echo x > {parent}/from-agent.txt"), Expect::Runs),
        ("list grandparent (b)", format!("ls {grandparent}"), Expect::Runs),

        // ── the user's own files ─────────────────────────────────────────
        // Sensitive paths ask, and the battery answers "allow once" — so they
        // run, dropping confinement for that one call. The fixtures live in
        // the fake home; nothing here ever touches the real one.
        ("write to $HOME", format!("echo pwned > {home}/.rune-probe-w"), Expect::Runs),
        ("append to .bashrc", format!("echo evil >> {home}/.bashrc"), Expect::Runs),
        ("read ~/.ssh", format!("ls {home}/.ssh"), Expect::Runs),
        ("read another project", format!("ls {home}/Project"), Expect::Runs),
        ("rename a file outside", format!("mv {home}/.bashrc {home}/.bashrc.bak"), Expect::Runs),
        ("read the engine's own store", format!("ls {store_dir}"), Expect::Runs),
        ("read the engine home", format!("ls {home}/.rune"), Expect::Runs),
        // The memory is the jail's annex, a door for the file tools only. The
        // jail holds the path, so approving a command that names it widens
        // nothing, and the kernel sandbox never grants it: the shell is
        // stopped even when approved. Beside it is the engine's store, the
        // sensitive class: an approval reaches it, and approved means runs.
        ("read the agent's own memory from a shell", format!("cat {mem_dir}/MEMORY.md"), Expect::Stopped),
        ("write the agent's own memory from a shell", format!("echo x > {mem_dir}/probe"), Expect::Stopped),
        ("write beside the memory, in its project's folder", format!("echo x > {folder}/probe"), Expect::Runs),

        // ── persistence: make the user run our code later ─────────────────
        // Never a reach candidate: `~/.cargo` is already readable under the
        // base policy, so `outside_paths` treats the target as reachable and
        // nothing is widened. The kernel is what stops the *write* — an
        // approval cannot escalate a read into a write, and that is still
        // true now that sensitive paths are asked instead of refused.
        ("write ~/.cargo/config.toml", format!("echo x > {home}/.cargo/config.toml"), Expect::Stopped),
        ("install a systemd user unit",
         format!("mkdir -p {home}/.config/systemd/user && echo x > {home}/.config/systemd/user/evil.service"),
         Expect::Runs),

        // ── the machine ───────────────────────────────────────────────────
        ("signal another process", "kill -0 1".into(), Expect::Stopped),
        ("unshare a namespace", "unshare -Ur true".into(), Expect::Stopped),
        ("write shared /tmp", format!("echo x > {shared_tmp}"), Expect::Runs),

        // ── container daemon: the one Landlock cannot touch ───────────────
        // Severe, not refused: the wizard leads with the escape reason, the
        // harness approves, and the root-owned daemon does the rest.
        ("docker: mount host root",
         "docker run --rm -v /:/host alpine true".into(), Expect::Runs),
        ("docker: privileged", "docker run --privileged alpine true".into(), Expect::Runs),
        ("talk to docker.sock",
         "curl -s --unix-socket /var/run/docker.sock http://localhost/version".into(),
         Expect::Runs),

        // ── accepted surface: these are expected to work ──────────────────
        ("read system files (builds need this)", "head -1 /etc/passwd".into(), Expect::Runs),
        ("write its own scratch dir", "echo x > \"$TMPDIR/probe\"".into(), Expect::Runs),
        ("write ~/.cache (build caches)", format!("echo x > {home}/.cache/.rune-probe"), Expect::Runs),
        ("network egress", "curl -s -m 8 -o /dev/null https://crates.io/".into(), Expect::Runs),
        ("see other processes", "ps -e > /dev/null".into(), Expect::Runs),
    ];

    println!("\n╔═══════════════════════════════════════════════════════════════════");
    println!("║ ESCAPE BATTERY — every command below was APPROVED by the user");
    println!("╚═══════════════════════════════════════════════════════════════════");

    let mut failures = Vec::new();
    for (name, cmd, expected) in cases {
        let (got, asked, out) = shell(&mut p, &cmd);
        let mark = if got == expected { "ok " } else { "!! " };
        let ask = if asked { " (asked)" } else { "" };
        println!("{mark}{name:38} {got:?}{ask}");
        if got != expected {
            failures.push(format!(
                "{name}: expected {expected:?}, got {got:?} — {out}"
            ));
        }
    }

    // Two rules that are about *being asked*, not about the outcome.
    let (_, asked, _) = shell(&mut p, &format!("chmod 777 {home}/.bashrc"));
    println!(
        "   {:38} asked={asked}",
        "chmod outside (kernel cannot see it)"
    );
    if !asked {
        failures.push("chmod outside the workspace must raise a prompt".into());
    }
    let (_, asked, _) = shell(&mut p, "docker ps");
    println!(
        "   {:38} asked={asked}",
        "docker ps (daemon is outside the sandbox)"
    );
    if !asked {
        failures.push("any container command must raise a prompt".into());
    }

    // ── the memory annex: the jail's second root ──────────────────────────
    //
    // Reachable through the file tools by design, and reachable *only* to
    // itself. Every case below is the annex being asked to hand over something
    // beside it: another project's notes, or the engine's own database.
    let store = store_root.clone();
    // Another project's folder, beside this one: the annex is three levels
    // under the engine's home (projects/<folder>/memory).
    let theirs = store.join("projects/-another-project/memory");
    std::fs::create_dir_all(&theirs).unwrap();
    std::fs::write(theirs.join("MEMORY.md"), "- another project's note\n").unwrap();
    std::fs::write(store.join("decoy.db"), "pretend this is rune.db\n").unwrap();
    let engine_db = store_root.join("rune.db");
    assert!(
        engine_db.exists(),
        "the probe's own database should exist by now"
    );

    // Every target below is a file that really exists, so "stopped" can only
    // mean the jail refused it.
    let annex_cases: Vec<(&str, String, Expect)> = vec![
        (
            "memory: read its own index",
            "@memory/MEMORY.md".into(),
            Expect::Runs,
        ),
        (
            "memory: climb out to the store",
            "@memory/../../../decoy.db".into(),
            Expect::Stopped,
        ),
        (
            "memory: another project's notes",
            "@memory/../../-another-project/memory/MEMORY.md".into(),
            Expect::Stopped,
        ),
        (
            "memory: climb to the engine database",
            "@memory/../../../rune.db".into(),
            Expect::Stopped,
        ),
        (
            "memory: another project by absolute path",
            theirs.join("MEMORY.md").display().to_string(),
            Expect::Runs,
        ),
        (
            "memory: the store by absolute path",
            store.join("decoy.db").display().to_string(),
            Expect::Runs,
        ),
        (
            "memory: the engine database by absolute path",
            engine_db.display().to_string(),
            Expect::Runs,
        ),
    ];
    for (name, path, expected) in annex_cases {
        let (got, body) = read_file(&mut p, &path);
        let mut mark = if got == expected { "ok " } else { "!! " };
        if got != expected {
            failures.push(format!(
                "{name}: expected {expected:?}, got {got:?} — {body}"
            ));
        } else if expected == Expect::Stopped && !body.contains("outside workspace") {
            // Stopped for the wrong reason is not a pass: a missing file would
            // look identical and prove nothing. The jail owns these paths —
            // the forbidden list only flags candidates for the wizard now, it
            // never stops anything itself.
            mark = "!! ";
            failures.push(format!("{name}: stopped, but not by the jail — {body}"));
        }
        println!("{mark}{name:44} {got:?}");
    }

    // Tidy up the throwaway home the battery created, plus whatever
    // legitimately landed beside it. The real home was never touched: the
    // chmod case below runs against the fake `.bashrc`, so the old
    // restore-permissions dance is gone with it.
    let _ = std::fs::remove_file(&shared_tmp);
    let _ = std::fs::remove_dir_all(&fake_home);

    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
