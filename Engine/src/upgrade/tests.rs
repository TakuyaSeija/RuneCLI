use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::*;
use crate::platform::fixture::Restricted;

pub(super) fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .join("rune-test")
        .join(format!("upgrade-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A configuration as Rune 0.1.4 wrote and accepted it.
const OLD_CONFIG: &str = r#"version = 1
keep_turns = 7 # kept from the very first config

# The owner's note about the classifier.
[classifier]
excluded_tools = ["inspect_files", "find_files"]

[inspect_files]
max_output_chars = 1234

[tools.find]
default_limit = 50

[terminal]
background_on_timeout = true

[[providers.list]]
name = "mimo"
base_url = "https://api.xiaomimimo.com/v1"
auth = { header = "Authorization", prefix = "Bearer " }

[[providers.list]]
name = "router"
base_url = "http://localhost:20128/v1"
auth = { header = "Authorization", prefix = "Bearer " }
"#;

struct OldHome {
    store: PathBuf,
    project: PathBuf,
    other: PathBuf,
}

/// A home exactly as Rune 0.1.4 left it: sessions, memory, undo blobs and
/// scratch at the top, keyed by project, its project registry, its archive,
/// a database at schema 19 and an old configuration.
fn old_home(name: &str) -> OldHome {
    let base = temp(name);
    let store = base.join("home");
    let project = base.join("work/app");
    let other = base.join("work/other");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let project = crate::platform::canonical(&project).unwrap();
    let other = crate::platform::canonical(&other).unwrap();
    std::fs::create_dir_all(&store).unwrap();
    {
        let db = crate::store::Db::open(&store.join("rune.db"), std::time::Duration::from_secs(5))
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO sessions (id, project, title, created_at, updated_at) VALUES (1, ?1, 'one', 1, 1)",
                [project.to_string_lossy()],
            )
            .unwrap();
        db.conn().pragma_update(None, "user_version", 19).unwrap();
    }
    let key = crate::project::project_key(&project);
    let write = |path: PathBuf, text: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    write(store.join("sessions/1/session.jsonl"), "{}\n");
    write(
        store.join("sessions/2/meta.json"),
        // JSON escapes a Windows path's backslashes; `format!` would not.
        &serde_json::json!({ "project": other.to_string_lossy() }).to_string(),
    );
    write(store.join("sessions/9/tools_result/x"), "stray");
    write(
        store.join("sessions/Drafts.json"),
        r#"{"clock":0,"projects":{}}"#,
    );
    write(store.join(format!("memory/{key}/MEMORY.md")), "- a note\n");
    write(
        store.join("memory/home-me-app/MEMORY.md"),
        "- an older copy\n",
    );
    write(store.join(format!("objects/{key}/ab/cdef")), "blob");
    write(store.join(format!("scratch/{key}/1/tmp.txt")), "scratch");
    write(
        store.join(format!("Capabilities/{key}/Session1.json")),
        "{}",
    );
    write(
        store.join(format!("projects/{key}.json")),
        &serde_json::json!({ "project": project.to_string_lossy(), "key": key }).to_string(),
    );
    write(store.join("sessions_index.json"), "{}");
    write(store.join("legacy/BeforeSchema19.db"), "old");
    write(store.join("rune.toml"), OLD_CONFIG);
    OldHome {
        store,
        project,
        other,
    }
}

/// Every file under `dir` with its bytes, leaving out the folders an upgrade
/// is allowed to add (`backups/`, `run/`).
fn tree(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if rel == "backups" || rel == "run" || rel.starts_with("rune.db") {
                continue;
            }
            if path.is_dir() {
                out.insert(format!("{rel}/"), Vec::new());
                walk(root, &path, out);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

fn schema(store: &Path) -> i64 {
    rusqlite::Connection::open(store.join("rune.db"))
        .unwrap()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_fresh_home_is_current_and_marked_as_this_builds() {
    let store = temp("fresh");
    assert_eq!(state(&store, &store).unwrap(), State::Current);
    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert_eq!(marked(&store).unwrap(), Some(VERSION));
    assert!(!pending(&store).unwrap());
}

/// The release this module was written for, end to end: every project's
/// data in its folder, nothing deleted, the configuration speaking format 2
/// with every comment kept, and the database at its new schema with a copy
/// of the old one beside it.
#[test]
fn a_home_rune_0_1_4_wrote_is_upgraded_whole() {
    let home = old_home("whole");
    let store = &home.store;
    assert_eq!(state(store, store).unwrap(), State::Older(1));
    assert!(pending(store).unwrap());
    let mut progress = Vec::new();
    prepare(store, store, &[], &mut |p| progress.push(p)).unwrap();
    assert_eq!(
        progress.last(),
        Some(&Progress::Done {
            from: 1,
            to: VERSION
        })
    );
    assert_eq!(state(store, store).unwrap(), State::Current);
    assert!(!pending(store).unwrap());

    let app = crate::project::find(store, &home.project).expect("its folder");
    let other = crate::project::find(store, &home.other).expect("its folder");
    assert!(app.join("sessions/1/session.jsonl").is_file());
    assert!(other.join("sessions/2/meta.json").is_file());
    assert_eq!(
        std::fs::read_to_string(app.join("memory/MEMORY.md")).unwrap(),
        "- a note\n"
    );
    assert!(app.join("objects/ab/cdef").is_file());
    assert!(app.join("scratch/1/tmp.txt").is_file());
    assert!(store.join("drafts.json").is_file());
    for gone in [
        "sessions",
        "memory",
        "objects",
        "scratch",
        "Capabilities",
        "legacy",
        "sessions_index.json",
    ] {
        assert!(!store.join(gone).exists(), "{gone} is still at the top");
    }
    // Set aside, never deleted.
    let backup = std::fs::read_dir(store.join("backups"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("Upgrade2-")
        })
        .expect("the upgrade's backup folder");
    for kept in [
        "obsolete/sessions/9/tools_result/x",
        "obsolete/memory/home-me-app/MEMORY.md",
        "obsolete/Capabilities",
        "obsolete/sessions_index.json",
        "obsolete/legacy/BeforeSchema19.db",
        "rune.toml",
        "plan.json",
    ] {
        assert!(backup.join(kept).exists(), "{kept} was not kept");
    }

    let config = std::fs::read_to_string(store.join("rune.toml")).unwrap();
    assert!(config.contains("[inspect_file]"), "{config}");
    assert!(!config.contains("inspect_files"), "{config}");
    assert!(
        config.contains("# The owner's note about the classifier."),
        "{config}"
    );
    assert!(config.contains(r#"api = "mimo""#), "{config}");
    assert!(config.contains(r#"api = "compatible""#), "{config}");
    let loaded = crate::settings::configuration::Configuration::load(store).unwrap();
    // Carried along through every format, then retired by format 11: undo
    // history has no limit of its own any more.
    assert!(!config.contains("keep_turns"), "{config}");
    assert_eq!(loaded.number("inspect_file.max_output_chars"), 1234);
    assert_eq!(loaded.number("find_files.matches"), 50);
    assert_eq!(
        loaded.get("classifier.excluded_tools"),
        &serde_json::json!(["inspect_file", "find_files"])
    );

    assert_eq!(schema(store), crate::store::SCHEMA_VERSION);
    assert!(store
        .join(format!(
            "backups/BeforeSchema{}.db",
            crate::store::SCHEMA_VERSION
        ))
        .is_file());
    // Once is enough: the next start does nothing.
    let before = tree(store);
    prepare(store, store, &[], &mut |_| panic!("nothing to report")).unwrap();
    assert_eq!(tree(store), before);
}

/// An upgrade that cannot finish leaves the home exactly as the previous
/// build had it — every file, every byte — so that build can go on.
#[test]
fn a_failed_upgrade_is_undone_to_the_byte() {
    let home = old_home("undo");
    let store = &home.store;
    // A folder for the project already holds a session 1: the move of the
    // old session 1 cannot happen, half way through the layout.
    let app = crate::project::open(store, &home.project).unwrap();
    std::fs::create_dir_all(app.join("sessions/1")).unwrap();
    let before = tree(store);
    let schema_before = schema(store);

    let error = prepare(store, store, &[], &mut |_| {})
        .unwrap_err()
        .to_string();
    assert!(error.contains("undone"), "{error}");
    assert_eq!(tree(store), before);
    assert_eq!(schema(store), schema_before);
    assert_eq!(state(store, store).unwrap(), State::Older(1));
    assert!(!store.join(ACTIVE).exists());
}

/// A crash part way through is undone by whichever build starts next, and
/// the upgrade then runs again from the beginning.
#[test]
fn an_upgrade_cut_short_is_undone_then_done_again() {
    let home = old_home("crash");
    let store = &home.store;
    let app = crate::project::open(store, &home.project).unwrap();
    // As if a process stopped after one rename of a planned upgrade.
    let backup = store.join("backups/Upgrade2-0");
    std::fs::create_dir_all(&backup).unwrap();
    let plan = Plan {
        moves: vec![(store.join("sessions/1"), app.join("sessions/1"))],
        created: vec![app.clone()],
        ..Plan::default()
    };
    plan.save(&backup).unwrap();
    std::fs::create_dir_all(app.join("sessions")).unwrap();
    std::fs::rename(store.join("sessions/1"), app.join("sessions/1")).unwrap();
    crate::settings::secrets::atomic_write(
        &store.join(ACTIVE),
        backup.to_string_lossy().as_bytes(),
    )
    .unwrap();
    assert!(pending(store).unwrap());

    prepare(store, store, &[], &mut |_| {}).unwrap();
    assert_eq!(state(store, store).unwrap(), State::Current);
    let app = crate::project::find(store, &home.project).unwrap();
    assert!(app.join("sessions/1/session.jsonl").is_file());
    assert!(!store.join("sessions").exists());
}

/// Nothing moves while a previous build still has a session open: the
/// upgrade says which one it waits for and goes on the moment it closes.
#[test]
fn the_upgrade_waits_for_the_previous_build_to_let_go() {
    let home = old_home("barrier");
    let store = home.store.clone();
    std::fs::create_dir_all(store.join("run")).unwrap();
    let lease =
        crate::lock::shared(&store.join("run/session_5.lock"), crate::lock::Wait::Yes).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = {
        let store = store.clone();
        std::thread::spawn(move || {
            prepare(&store, &store, &[], &mut |p| {
                let _ = tx.send(p);
            })
        })
    };
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(10)).unwrap(),
        Progress::Waiting(vec![5])
    );
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert!(
        store.join("sessions").is_dir(),
        "nothing moved while it was open"
    );
    drop(lease);
    worker.join().unwrap().unwrap();
    assert_eq!(state(&store, &store).unwrap(), State::Current);
}

/// A host that `exec`d this build holds its sessions' ownership locks across
/// the exec. They are this process's own: the upgrade goes ahead rather than
/// wait for itself forever — while a lease another process holds still
/// counts.
#[test]
fn the_upgrade_does_not_wait_for_the_ownership_locks_this_process_carries() {
    let home = old_home("own-locks");
    let store = home.store.clone();
    std::fs::create_dir_all(store.join("run")).unwrap();
    // Held through a descriptor of its own, as one inherited across an exec.
    let inherited = crate::lock::exclusive(
        &store.join("run/session_7.Host.lock"),
        crate::lock::Wait::Yes,
    )
    .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = {
        let store = store.clone();
        std::thread::spawn(move || {
            prepare(&store, &store, &[7], &mut |p| {
                let _ = tx.send(p);
            })
        })
    };
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Progress::Done { .. }) => break,
            Ok(Progress::Waiting(sessions)) => {
                // Let the worker go before failing, so the test ends.
                drop(inherited);
                let _ = worker.join();
                panic!("it waited for its own lock: {sessions:?}");
            }
            Ok(_) => {}
            Err(e) => panic!("no progress: {e}"),
        }
    }
    worker.join().unwrap().unwrap();
    assert_eq!(state(&store, &store).unwrap(), State::Current);
    drop(inherited);
}

/// Hosts that took this build over together each carry their sessions'
/// ownership locks across the exec and queue for the upgrade behind whichever
/// process runs it — here one that carries none, as a window. It does not
/// wait for their locks (in 0.2.8 three hosts and a window waited for one
/// another for good); a lock with no note, or a note naming a process that is
/// not the one alive under that number, is still waited for. And the notes go
/// when the upgrade is over for their process.
#[test]
fn hosts_that_took_this_build_over_together_do_not_wait_for_one_another() {
    let home = old_home("carried-locks");
    let store = home.store.clone();
    std::fs::create_dir_all(store.join("run")).unwrap();
    // Two other hosts of this build: each holds its lock through a descriptor
    // of its own and has noted it.
    let lock = |id: i64| {
        crate::lock::exclusive(
            &crate::store::run_files::host_lock(&store, id),
            crate::lock::Wait::Yes,
        )
        .unwrap()
    };
    let (seven, eight) = (lock(7), lock(8));
    let me = std::process::id();
    let started = crate::platform::process_started(me).expect("this process's start");
    let note = |id: i64, started: u64| {
        std::fs::write(
            crate::store::run_files::carried(&store, id),
            serde_json::to_vec(&Carrier { pid: me, started }).unwrap(),
        )
        .unwrap()
    };
    note(7, started);
    // A note for a process that only has this number now: not a carrier.
    note(8, started + 1);
    let run = |own: Vec<i64>| {
        let (tx, rx) = std::sync::mpsc::channel();
        let store = store.clone();
        let worker = std::thread::spawn(move || {
            prepare(&store, &store, &own, &mut |p| {
                let _ = tx.send(p);
            })
        });
        (worker, rx)
    };
    let (worker, rx) = run(Vec::new());
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Progress::Waiting(sessions)) => {
                assert_eq!(
                    sessions,
                    vec![8],
                    "only the lock nobody of this build carries"
                );
                break;
            }
            Ok(Progress::Done { .. }) => panic!("it did not wait for a lock nobody noted"),
            Ok(_) => {}
            Err(e) => panic!("no progress: {e}"),
        }
    }
    // The note was the stranger's; once it is this host's, the way is clear.
    note(8, started);
    loop {
        match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Progress::Done { .. }) => break,
            Ok(_) => {}
            Err(e) => panic!("it kept waiting for carried locks: {e}"),
        }
    }
    worker.join().unwrap().unwrap();
    assert_eq!(state(&store, &store).unwrap(), State::Current);
    drop((seven, eight));

    // A process that carries locks notes them while it waits, and takes the
    // notes away when the upgrade is over for it.
    let _ = std::fs::remove_file(crate::store::run_files::carried(&store, 7));
    let _ = std::fs::remove_file(crate::store::run_files::carried(&store, 8));
    write_marker(&store, VERSION - 1).unwrap();
    let held = lock(9);
    let gate = crate::lock::exclusive(&store.join(LOCK), crate::lock::Wait::Yes).unwrap();
    let (worker, rx) = run(vec![9]);
    let note = crate::store::run_files::carried(&store, 9);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !note.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "no note while it waits"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let carrier: Carrier = serde_json::from_slice(&std::fs::read(&note).unwrap()).unwrap();
    assert!(carrier.pid == me && carrier.started == started);
    drop(gate);
    while !matches!(
        rx.recv_timeout(std::time::Duration::from_secs(10)),
        Ok(Progress::Done { .. })
    ) {}
    worker.join().unwrap().unwrap();
    assert!(!note.exists(), "the note outlived the upgrade");
    drop(held);
}

/// A home a newer build wrote is never touched by an older one.
#[test]
fn a_newer_home_is_refused_and_left_alone() {
    let store = temp("newer");
    write_marker(&store, VERSION + 1).unwrap();
    std::fs::write(store.join("rune.toml"), "version = 1\n").unwrap();
    let before = tree(&store);
    assert!(prepare(&store, &store, &[], &mut |_| {}).is_err());
    assert!(check(&store, &store).is_err());
    assert_eq!(tree(&store), before);
}

/// The readiness check a running build asks of an installed one changes
/// nothing, and refuses what the upgrade could not finish.
#[test]
fn the_check_changes_nothing_and_refuses_what_would_fail() {
    let home = old_home("check");
    let store = &home.store;
    let before = tree(store);
    check(store, store).unwrap();
    assert_eq!(tree(store), before);

    std::fs::write(
        store.join("rune.toml"),
        format!("{OLD_CONFIG}\n[classifier.typo]\nenable = true\n"),
    )
    .unwrap();
    assert!(check(store, store).is_err());
}

/// A configuration already in format 2 is left byte for byte as it is, and
/// is never mistaken for an old one.
#[test]
fn a_current_configuration_is_not_rewritten() {
    let reference = crate::settings::configuration::Configuration::reference();
    assert_eq!(v2::config(&reference).unwrap(), reference);
    assert_eq!(v3::config(&reference).unwrap(), reference);
    let dir = temp("current-config");
    std::fs::write(dir.join("rune.toml"), &reference).unwrap();
    assert!(!v2::configured_by_version_1(&dir));
    std::fs::write(dir.join("rune.toml"), OLD_CONFIG).unwrap();
    assert!(v2::configured_by_version_1(&dir));
}

/// The engine refuses a home still in an older format instead of filing new
/// sessions beside old ones it cannot see.
#[test]
fn the_engine_does_not_open_an_old_home() {
    let home = old_home("engine");
    let config = crate::engine::Config::at(
        home.store.join("rune.db"),
        temp("engine-settings"),
        std::sync::Arc::new(crate::provider::scripted::Scripted::new(Vec::new())),
    )
    .unwrap();
    let error = crate::engine::Engine::start(config).err().expect("refused");
    assert!(error.to_string().contains("older format"), "{error}");
}

/// `rune.toml` as Rune 0.1.5 laid it out: the retry step and cap were
/// engine-internal then, so `[retry]` held only the retry count.
fn config_0_1_5() -> String {
    let reference = crate::settings::configuration::Configuration::reference();
    let mut table = String::new();
    let mut out = String::new();
    for line in reference.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            table = trimmed.to_string();
            if table == "[retry]" {
                out.push_str("# The owner's note about retries.\n");
            }
        }
        if table == "[retry]"
            && (trimmed.starts_with("initial_delay") || trimmed.starts_with("max_delay"))
        {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.replace("max_attempts = 5", "max_attempts = 7")
}

/// A home 0.1.5 left: format 2, its configuration without the retry curve,
/// the `scratch/probe` its sandbox report made at the top, and the outputs it
/// kept under `sessions/<id>/tools_result/` — of session 7, whose folder is
/// there, and of session 8, whose folder is not.
fn home_0_1_5(name: &str) -> PathBuf {
    let store = temp(name);
    write_marker(&store, 2).unwrap();
    std::fs::write(store.join("rune.toml"), config_0_1_5()).unwrap();
    std::fs::create_dir_all(store.join("scratch/probe")).unwrap();
    let session = store.join("projects/-w-app/sessions/7");
    std::fs::create_dir_all(&session).unwrap();
    std::fs::write(session.join("session.jsonl"), "{}\n").unwrap();
    for (path, text) in [
        ("sessions/7/tools_result/terminal_use/bg_aaaa.txt", "a log"),
        (
            "sessions/7/tools_result/terminal_use/long.txt",
            "long output",
        ),
        (
            "sessions/8/tools_result/manage_process/gone.txt",
            "of a deleted session",
        ),
    ] {
        std::fs::create_dir_all(store.join(path).parent().unwrap()).unwrap();
        std::fs::write(store.join(path), text).unwrap();
    }
    store
}

/// Format 2 → 3: the retry curve shows in `rune.toml` with everything the
/// person wrote kept, and the stray top-level `scratch/` is set aside, never
/// deleted.
#[test]
fn a_home_rune_0_1_5_wrote_is_upgraded_to_format_3() {
    let store = home_0_1_5("v3");
    assert_eq!(state(&store, &store).unwrap(), State::Older(2));
    assert!(pending(&store).unwrap());
    let before = tree(&store);
    check(&store, &store).unwrap();
    assert_eq!(tree(&store), before, "the check changes nothing");

    let mut progress = Vec::new();
    prepare(&store, &store, &[], &mut |p| progress.push(p)).unwrap();
    assert_eq!(
        progress.last(),
        Some(&Progress::Done {
            from: 2,
            to: VERSION
        })
    );
    assert_eq!(state(&store, &store).unwrap(), State::Current);
    assert!(
        !store.join("scratch").exists(),
        "scratch is still at the top"
    );
    assert!(
        !store.join("sessions").exists(),
        "sessions is still at the top"
    );
    let results = store.join("projects/-w-app/sessions/7/tools_result/terminal_use");
    assert_eq!(
        std::fs::read_to_string(results.join("bg_aaaa.txt")).unwrap(),
        "a log"
    );
    assert_eq!(
        std::fs::read_to_string(results.join("long.txt")).unwrap(),
        "long output"
    );

    let backups: Vec<_> = std::fs::read_dir(store.join("backups"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    let backup = backups
        .iter()
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("Upgrade3-")
        })
        .expect("the upgrade's backup folder");
    assert!(backup.join("obsolete/scratch/probe").is_dir());
    assert_eq!(
        std::fs::read_to_string(
            backup.join("obsolete/sessions/8/tools_result/manage_process/gone.txt")
        )
        .unwrap(),
        "of a deleted session",
        "an output of a session that no longer exists is kept aside"
    );
    // The first step that rewrote the configuration kept it as it was.
    let mut saved: Vec<_> = backups
        .iter()
        .filter(|p| p.join("rune.toml").is_file())
        .collect();
    saved.sort_by_key(|p| {
        p.file_name().unwrap().to_string_lossy()[7..]
            .split('-')
            .next()
            .unwrap()
            .parse::<u32>()
            .unwrap()
    });
    assert_eq!(
        std::fs::read_to_string(saved[0].join("rune.toml")).unwrap(),
        config_0_1_5(),
        "the configuration as it was is kept"
    );

    // The file holds the person's choices and words; the retry step and cap
    // nobody chose follow this build's defaults (owner, 2026-10-14).
    let config = std::fs::read_to_string(store.join("rune.toml")).unwrap();
    assert!(
        config.contains("# The owner's note about retries."),
        "{config}"
    );
    assert!(config.contains("max_attempts = 7"), "{config}");
    assert!(!config.contains("initial_delay"), "{config}");
    assert!(!config.contains("max_delay"), "{config}");
    let loaded = crate::settings::configuration::Configuration::load(&store).unwrap();
    assert_eq!(loaded.number("retry.max_attempts"), 7);
    assert_eq!(
        loaded.duration("retry.max_delay"),
        std::time::Duration::from_secs(60)
    );

    let after = tree(&store);
    prepare(&store, &store, &[], &mut |_| panic!("nothing to report")).unwrap();
    assert_eq!(tree(&store), after, "once is enough");
}

/// A format-3 configuration that would not load stops the step before
/// anything is written: the home stays exactly as 0.1.5 left it.
#[test]
fn a_step_3_that_cannot_finish_leaves_the_0_1_5_home_as_it_was() {
    let store = home_0_1_5("v3-undo");
    std::fs::write(
        store.join("rune.toml"),
        format!("{}\n[retry.typo]\nenable = true\n", config_0_1_5()),
    )
    .unwrap();
    let before = tree(&store);
    assert!(check(&store, &store).is_err());
    let error = prepare(&store, &store, &[], &mut |_| {})
        .unwrap_err()
        .to_string();
    assert!(error.contains("undone"), "{error}");
    assert_eq!(tree(&store), before);
    assert_eq!(state(&store, &store).unwrap(), State::Older(2));
    assert!(store.join("scratch/probe").is_dir());
}

/// A step from a marked format that is cut short is undone to that format —
/// the marker included — and then done again; an unmarked home would read as
/// this build's and the step would never run.
#[test]
fn a_step_3_cut_short_is_undone_to_format_2_then_done_again() {
    let store = home_0_1_5("v3-crash");
    // As if a process stopped right after setting the scratch folder aside.
    let backup = store.join("backups/Upgrade3-0");
    std::fs::create_dir_all(&backup).unwrap();
    let plan = Plan {
        moves: vec![(store.join("scratch"), backup.join("obsolete/scratch"))],
        marker: Some(2),
        ..Plan::default()
    };
    plan.make_moves(&backup).unwrap();
    std::fs::create_dir_all(store.join("run")).unwrap();
    crate::settings::secrets::atomic_write(
        &store.join(ACTIVE),
        backup.to_string_lossy().as_bytes(),
    )
    .unwrap();
    assert!(pending(&store).unwrap());

    undo(&store, &store, &backup).unwrap();
    assert_eq!(marked(&store).unwrap(), Some(2));
    assert!(store.join("scratch/probe").is_dir());

    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert_eq!(state(&store, &store).unwrap(), State::Current);
    assert!(!store.join("scratch").exists());
}

/// A home 0.2.7 left: format 3, a session's composer under its old name, a
/// host's log under its old name, and a configuration without the stall
/// limit.
fn home_0_2_7(name: &str) -> PathBuf {
    let store = temp(name);
    write_marker(&store, 3).unwrap();
    std::fs::write(
        store.join("rune.toml"),
        "version = 1\n\n[retry]\nmax_attempts = 5\n",
    )
    .unwrap();
    let session = store.join("projects/-w-app/sessions/12");
    std::fs::create_dir_all(&session).unwrap();
    std::fs::write(session.join("session.jsonl"), "{}\n").unwrap();
    std::fs::write(
        session.join("Composer.json"),
        r#"{"revision":3,"text":"unsent words"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(store.join("run")).unwrap();
    std::fs::write(store.join("run/Session12Host.log"), "host said this\n").unwrap();
    store
}

/// Format 3 → 4: one spelling for what Rune keeps — the composer and the host
/// log renamed with their contents as they were, the locks untouched — and the
/// stall limit shown in `rune.toml` with everything the person wrote kept.
#[test]
fn a_home_rune_0_2_7_wrote_is_upgraded_to_format_4() {
    let store = home_0_2_7("v4");
    assert_eq!(state(&store, &store).unwrap(), State::Older(3));
    let before = tree(&store);
    check(&store, &store).unwrap();
    assert_eq!(tree(&store), before, "the check changes nothing");

    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert_eq!(state(&store, &store).unwrap(), State::Current);
    let session = store.join("projects/-w-app/sessions/12");
    assert!(gone_or_same(
        &session.join("Composer.json"),
        &session.join("composer.json")
    ));
    assert_eq!(
        std::fs::read_to_string(session.join("composer.json")).unwrap(),
        r#"{"revision":3,"text":"unsent words"}"#,
        "renamed, not rewritten"
    );
    assert!(!store.join("run/Session12Host.log").exists());
    assert_eq!(
        std::fs::read_to_string(store.join("run/session_12.host.log")).unwrap(),
        "host said this\n"
    );
    // The stall limit is the person's to set; unset, it is this build's default.
    let loaded = crate::settings::configuration::Configuration::load(&store).unwrap();
    assert_eq!(loaded.number("retry.max_attempts"), 5);
    assert_eq!(
        loaded.duration("network.stream_idle_timeout"),
        std::time::Duration::from_secs(300)
    );
    assert!(!crate::settings::configuration::is_internal_invariant(
        "network.stream_idle_timeout"
    ));
}

/// A composer already under the new name is never overwritten by the old one:
/// the old one is set aside in the backup.
#[test]
fn an_old_composer_never_overwrites_a_new_one() {
    let store = home_0_2_7("v4-both");
    let session = store.join("projects/-w-app/sessions/12");
    std::fs::write(session.join("composer.json"), "{\"revision\":9}").unwrap();
    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert_eq!(
        std::fs::read_to_string(session.join("composer.json")).unwrap(),
        "{\"revision\":9}"
    );
    assert!(gone_or_same(
        &session.join("Composer.json"),
        &session.join("composer.json")
    ));
}

/// The old name is gone — or, on a filesystem that ignores case (Windows,
/// macOS), is the new name itself, which the step leaves as it is.
fn gone_or_same(old: &Path, new: &Path) -> bool {
    std::fs::symlink_metadata(old).is_err() || crate::platform::same_file(old, new)
}

/// A step 4 that fails is undone: the home is 0.2.7's again, byte for byte.
#[test]
fn a_step_4_that_cannot_finish_leaves_the_0_2_7_home_as_it_was() {
    let store = home_0_2_7("v4-fail");
    let before = tree(&store);
    // A configuration this build cannot read stops the step after it began.
    std::fs::write(
        store.join("rune.toml"),
        "version = 1\n[network\nstream_idle_timeout = \"5m\"\n",
    )
    .unwrap();
    let before_config = tree(&store);
    let error = prepare(&store, &store, &[], &mut |_| {})
        .unwrap_err()
        .to_string();
    assert!(error.contains("undone"), "{error}");
    let mut after = tree(&store);
    after.retain(|path, _| !path.starts_with("backups/") && !path.starts_with("run/Upgrade"));
    let mut expected = before_config;
    expected.retain(|path, _| !path.starts_with("run/Upgrade"));
    assert_eq!(after, expected);
    assert_eq!(state(&store, &store).unwrap(), State::Older(3));
    let _ = before;
}

/// `drafts.json` as Rune 0.2.9 left it: a window's words, a chosen model, and
/// the empty tombstones every window closed without a word left behind — the
/// newest of them after the words.
const DRAFTS_0_2_9: &str = r#"{
  "clock": 40,
  "projects": {
    "k1": {
      "project": "/w/app",
      "generation": 2,
      "entries": [
        {"token": "a", "order": 10, "composer": {"revision": 3, "text": "unsent words", "attachments": [], "pending": [], "preference_changed": false, "cleared": false}},
        {"token": "b", "order": 20, "composer": {"revision": 1, "text": "", "attachments": [], "pending": [], "preference_changed": false, "cleared": true}},
        {"token": "c", "order": 30, "composer": {"revision": 2, "text": "", "attachments": [], "pending": [], "preference_changed": true, "cleared": true}},
        {"token": "d", "order": 35, "composer": {"revision": 1, "text": "", "attachments": [], "pending": [], "preference_changed": false, "cleared": false}}
      ]
    },
    "k2": {"project": "/w/other", "generation": 0, "entries": []}
  }
}"#;

fn home_0_2_9(name: &str) -> PathBuf {
    let store = temp(name);
    write_marker(&store, 4).unwrap();
    std::fs::create_dir_all(store.join("run")).unwrap();
    std::fs::write(store.join("drafts.json"), DRAFTS_0_2_9).unwrap();
    store
}

/// Format 4 → 5: every draft entry holding nothing goes; the newest one's
/// order is remembered as the project's `cleared`, so a new window is not
/// handed the older words; the entries with data stay as they were.
#[test]
fn a_home_rune_0_2_9_wrote_is_upgraded_to_format_5() {
    let store = home_0_2_9("v5");
    assert_eq!(state(&store, &store).unwrap(), State::Older(4));
    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert_eq!(state(&store, &store).unwrap(), State::Current);
    let after: serde_json::Value =
        serde_json::from_slice(&std::fs::read(store.join("drafts.json")).unwrap()).unwrap();
    let project = &after["projects"]["k1"];
    let tokens: Vec<&str> = project["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["token"].as_str().unwrap())
        .collect();
    assert_eq!(tokens, ["a", "c"], "the chosen model is data and stays");
    assert_eq!(project["cleared"], 35);
    assert_eq!(project["generation"], 2);
    assert_eq!(after["projects"]["k2"]["entries"], serde_json::json!([]));
    assert!(
        after["projects"]["k2"].get("cleared").is_none(),
        "untouched"
    );
    // The file as it was is in the backup.
    let backup = std::fs::read_dir(store.join("backups"))
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("Upgrade5-"))
        .unwrap()
        .path();
    assert_eq!(
        std::fs::read_to_string(backup.join("drafts.json")).unwrap(),
        DRAFTS_0_2_9
    );
}

/// The store reads the upgraded file as the tombstones read: the emptied
/// composition is the newest, so a new window opens empty.
#[test]
fn after_format_5_a_new_window_is_not_handed_older_words() {
    let mut registry: serde_json::Value = serde_json::from_str(DRAFTS_0_2_9).unwrap();
    assert!(v5::fold(&mut registry));
    // Only "a" (order 10) and "c" (order 30) are left; "d" at 35 was emptied
    // after both, and "c" is the newest kept — but older than the clear.
    let project = &registry["projects"]["k1"];
    let newest = project["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["order"].as_u64().unwrap())
        .max()
        .unwrap();
    assert!(newest < project["cleared"].as_u64().unwrap());
    // Folding again changes nothing.
    let once = registry.clone();
    assert!(!v5::fold(&mut registry));
    assert_eq!(registry, once);
}

/// Undoing step 5 puts `drafts.json` back byte for byte.
#[test]
fn a_step_5_undone_puts_the_drafts_back() {
    let store = home_0_2_9("v5-undo");
    let backup = store.join("backups/Upgrade5-test");
    std::fs::create_dir_all(&backup).unwrap();
    let mut plan = Plan {
        marker: Some(4),
        ..Plan::default()
    };
    plan.save(&backup).unwrap();
    v5::apply(&store, &backup, &mut plan).unwrap();
    assert_ne!(
        std::fs::read_to_string(store.join("drafts.json")).unwrap(),
        DRAFTS_0_2_9
    );
    undo(&store, &store, &backup).unwrap();
    assert_eq!(
        std::fs::read_to_string(store.join("drafts.json")).unwrap(),
        DRAFTS_0_2_9
    );
    assert_eq!(state(&store, &store).unwrap(), State::Older(4));
}

/// A home Rune 0.3.6 left: format 5, its database at schema 20 — a session
/// that changed one file in turn 2 and another in turn 3, the second's
/// checkpoint already pruned.
fn home_0_3_6(name: &str) -> PathBuf {
    let store = temp(name);
    write_marker(&store, 5).unwrap();
    std::fs::create_dir_all(store.join("run")).unwrap();
    let db =
        crate::store::Db::open(&store.join("rune.db"), std::time::Duration::from_secs(5)).unwrap();
    db.conn()
        .execute_batch(
            "ALTER TABLE session_files DROP COLUMN last_at;
             ALTER TABLE session_files DROP COLUMN first_mode;
             ALTER TABLE session_files DROP COLUMN redo_blob;
             ALTER TABLE session_files DROP COLUMN redo_mode;
             ALTER TABLE session_files DROP COLUMN redo_at;
             INSERT INTO sessions (id, project, title, created_at, updated_at) VALUES (1, '/w/app', 'one', 1, 1);
             INSERT INTO checkpoints (session_id, turn, created_at, manifest)
               VALUES (1, 2, 1700000000000, '[{\"path\":\"run.sh\",\"blob\":\"h1\",\"mode\":33261}]');
             INSERT INTO session_files (session_id, path, first_blob, first_turn, last_turn, change)
               VALUES (1, 'run.sh', 'h1', 2, 2, 'modified'),
                      (1, 'gone.rs', 'h2', 3, 3, 'modified');",
        )
        .unwrap();
    db.conn().pragma_update(None, "user_version", 20).unwrap();
    drop(db);
    store
}

/// Format 5 → 6: the file ledger learns when each file was last changed and
/// the mode it had, from the checkpoints still kept; the database as it was
/// is kept first, and the next start does nothing.
#[test]
fn a_home_rune_0_3_6_wrote_is_upgraded_to_format_6() {
    let store = home_0_3_6("v6");
    assert_eq!(state(&store, &store).unwrap(), State::Older(5));
    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert_eq!(state(&store, &store).unwrap(), State::Current);
    assert_eq!(schema(&store), crate::store::SCHEMA_VERSION);
    assert!(store
        .join(format!(
            "backups/BeforeSchema{}.db",
            crate::store::SCHEMA_VERSION
        ))
        .is_file());
    let db =
        crate::store::Db::open(&store.join("rune.db"), std::time::Duration::from_secs(5)).unwrap();
    let files = db.session_files(1).unwrap();
    assert_eq!(files[0].path, "run.sh");
    assert_eq!(files[0].last_at, Some(1_700_000_000_000));
    assert_eq!(files[0].first_mode, Some(33261));
    assert_eq!(files[1].path, "gone.rs");
    assert_eq!((files[1].last_at, files[1].first_mode), (None, None));
    drop(db);
    let before = tree(&store);
    prepare(&store, &store, &[], &mut |_| panic!("nothing to report")).unwrap();
    assert_eq!(tree(&store), before);
}

/// A home Rune 0.3.7 left: format 6, its database at schema 21 — the file
/// ledger without a redo.
fn home_0_3_7(name: &str) -> PathBuf {
    let store = temp(name);
    write_marker(&store, 6).unwrap();
    std::fs::create_dir_all(store.join("run")).unwrap();
    let db =
        crate::store::Db::open(&store.join("rune.db"), std::time::Duration::from_secs(5)).unwrap();
    db.conn()
        .execute_batch(
            "ALTER TABLE session_files DROP COLUMN redo_blob;
             ALTER TABLE session_files DROP COLUMN redo_mode;
             ALTER TABLE session_files DROP COLUMN redo_at;
             INSERT INTO sessions (id, project, title, created_at, updated_at) VALUES (1, '/w/app', 'one', 1, 1);
             INSERT INTO session_files (session_id, path, first_blob, first_turn, last_turn, change, last_at)
               VALUES (1, 'a.rs', 'h1', 2, 2, 'modified', 5);",
        )
        .unwrap();
    db.conn().pragma_update(None, "user_version", 21).unwrap();
    drop(db);
    store
}

/// Format 6 → 7: the file ledger can hold a redo; nothing has one yet, and
/// the rows are otherwise as they were.
#[test]
fn a_home_rune_0_3_7_wrote_is_upgraded_to_format_7() {
    let store = home_0_3_7("v7");
    assert_eq!(state(&store, &store).unwrap(), State::Older(6));
    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert_eq!(state(&store, &store).unwrap(), State::Current);
    assert_eq!(schema(&store), crate::store::SCHEMA_VERSION);
    assert!(store.join("backups/BeforeSchema22.db").is_file());
    let db =
        crate::store::Db::open(&store.join("rune.db"), std::time::Duration::from_secs(5)).unwrap();
    let file = db.session_file(1, "a.rs").unwrap().unwrap();
    assert_eq!(
        (file.last_at, file.redo_at, file.redo_blob),
        (Some(5), None, None)
    );
}

/// A step 7 that cannot finish — the database cannot be written — is undone:
/// the home reads as format 6 again and its database is schema 21, as 0.3.7
/// left it, so that build goes on.
#[test]
fn a_step_7_that_cannot_finish_leaves_the_0_3_7_home_as_it_was() {
    let store = home_0_3_7("v7-fail");
    let db = store.join("rune.db");
    crate::platform::set_file_mode(&db, 0o444).unwrap();
    let error = prepare(&store, &store, &[], &mut |_| {})
        .unwrap_err()
        .to_string();
    let _ = crate::platform::set_file_mode(&db, 0o644);
    // Undone part way — or, where a read-only file cannot even be opened to
    // write (Windows), refused before anything changed; either way the home
    // is 0.3.7's, as the rest says.
    assert!(
        error.contains("undone") || error.contains("denied"),
        "{error}"
    );
    assert_eq!(state(&store, &store).unwrap(), State::Older(6));
    assert_eq!(schema(&store), 21);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let has_redo: bool = conn
        .query_row(
            "SELECT count(*) > 0 FROM pragma_table_info('session_files') WHERE name = 'redo_at'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!has_redo, "the schema 22 column is not left behind");
}

#[test]
fn web_fetch_upgrade_seeds_separate_limits_without_overwriting_explicit_values() {
    let old = "# Owner's web limits\n[web_search]\ntimeout = \"13s\" # keep this\nmax_output_chars = 4321\n[classifier]\nexcluded_tools = [\"web_search\", \"inspect_file\"]\n";
    let upgraded = v8::config(old).unwrap();
    let config: serde_json::Value = toml_edit::de::from_str(&upgraded).unwrap();
    assert_eq!(config["web_search"]["timeout"], "13s");
    assert_eq!(config["web_fetch"]["timeout"], "13s");
    assert_eq!(config["web_search"]["max_output_chars"], 4321);
    assert_eq!(config["web_fetch"]["max_output_chars"], 4321);
    assert_eq!(
        config["classifier"]["excluded_tools"],
        serde_json::json!(["web_search", "inspect_file", "web_fetch"])
    );
    assert!(upgraded.contains("Owner's web limits") && upgraded.contains("keep this"));
    assert_eq!(
        upgraded.matches("keep this").count(),
        1,
        "the value is seeded, not the comment beside the search key: {upgraded}"
    );
    assert_eq!(
        v8::config(&upgraded).unwrap(),
        upgraded,
        "the split is idempotent"
    );

    let explicit = "[web_search]\ntimeout = \"13s\"\nmax_output_chars = 4321\n[web_fetch]\ntimeout = \"29s\"\nmax_output_chars = 6789\n[classifier]\nexcluded_tools = []\n";
    let upgraded = v8::config(explicit).unwrap();
    let config: serde_json::Value = toml_edit::de::from_str(&upgraded).unwrap();
    assert_eq!(config["web_fetch"]["timeout"], "29s");
    assert_eq!(config["web_fetch"]["max_output_chars"], 6789);
    assert_eq!(
        config["classifier"]["excluded_tools"],
        serde_json::json!([])
    );
    let reviewed = v8::config("[classifier]\nexcluded_tools = [\"inspect_file\"]\n").unwrap();
    let config: serde_json::Value = toml_edit::de::from_str(&reviewed).unwrap();
    assert_eq!(
        config["classifier"]["excluded_tools"],
        serde_json::json!(["inspect_file"])
    );
}

#[test]
fn additive_audit_code_upgrade_keeps_old_jsonl_byte_for_byte() {
    let store = temp("audit-code-v9");
    write_marker(&store, 8).unwrap();
    let folder = store.join("projects/synthetic/sessions/1");
    std::fs::create_dir_all(&folder).unwrap();
    let audit = folder.join("toolcalls.jsonl");
    let old = b"{\"id\":\"p\",\"status\":\"failed\",\"summary\":\"old error\"}\n";
    std::fs::write(&audit, old).unwrap();
    let before = tree(&store);
    check(&store, &store).unwrap();
    assert_eq!(tree(&store), before);
    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert_eq!(state(&store, &store).unwrap(), State::Current);
    assert_eq!(std::fs::read(audit).unwrap(), old);
    assert_eq!(marked(&store).unwrap(), Some(VERSION));
}

/// Format 10 adds to background records and nothing else: a journal an
/// earlier build wrote is kept byte for byte and still read, its start with
/// no identity or tree mark.
#[test]
fn additive_background_upgrade_keeps_old_journals_byte_for_byte() {
    let store = temp("background-v10");
    write_marker(&store, 9).unwrap();
    let folder = store.join("projects/synthetic/sessions/1");
    std::fs::create_dir_all(&folder).unwrap();
    let journal = folder.join("background.jsonl");
    let old = b"{\"event\":\"started\",\"task_id\":\"bg_old\",\"pid\":77,\"command\":\"cargo test\",\"out_file\":\"/x/bg_old.txt\",\"started_at\":1}\n{\"event\":\"ended\",\"task_id\":\"bg_old\",\"status\":{\"Completed\":{\"exit_code\":0,\"finished_at\":2}}}\n";
    std::fs::write(&journal, old).unwrap();
    let before = tree(&store);
    check(&store, &store).unwrap();
    assert_eq!(tree(&store), before, "the check changes nothing");
    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert_eq!(state(&store, &store).unwrap(), State::Current);
    assert_eq!(std::fs::read(&journal).unwrap(), old);
    assert_eq!(marked(&store).unwrap(), Some(VERSION));
    for line in std::str::from_utf8(old).unwrap().lines() {
        let record: crate::session::background::BackgroundRecord =
            serde_json::from_str(line).unwrap();
        if let crate::session::background::BackgroundRecord::Started { identity, tree, .. } = record
        {
            assert!(identity.is_none() && tree.is_none());
        }
    }
}

#[test]
fn old_layout_steps_do_not_mask_web_limits_with_new_defaults() {
    let old = "[web_search]\ntimeout = \"13s\"\nmax_output_chars = 4321\n[classifier]\nexcluded_tools = [\"web_search\"]\n";
    for text in [v3::config(old).unwrap(), v4::config(old).unwrap()] {
        let config: serde_json::Value =
            toml_edit::de::from_str(&v8::config(&text).unwrap()).unwrap();
        assert_eq!(config["web_fetch"]["timeout"], "13s");
        assert_eq!(config["web_fetch"]["max_output_chars"], 4321);
    }
}

#[test]
fn format_8_checks_without_writing_then_backs_up_and_can_restore_the_configuration() {
    let store = temp("web-tools-v8");
    write_marker(&store, 7).unwrap();
    let old = "[web_search]\ntimeout = \"13s\"\nmax_output_chars = 4321\n";
    std::fs::write(store.join("rune.toml"), old).unwrap();
    let before = tree(&store);
    check(&store, &store).unwrap();
    assert_eq!(tree(&store), before, "the check changes nothing");
    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert_eq!(state(&store, &store).unwrap(), State::Current);
    let config = crate::settings::configuration::Configuration::load(&store).unwrap();
    assert_eq!(config.number("web_fetch.max_output_chars"), 4321);
    assert_eq!(
        config.duration("web_fetch.timeout"),
        std::time::Duration::from_secs(13)
    );
    let backup = std::fs::read_dir(store.join("backups"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("Upgrade8-")
        })
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(backup.join("rune.toml")).unwrap(),
        old
    );
    undo(&store, &store, &backup).unwrap();
    assert_eq!(
        std::fs::read_to_string(store.join("rune.toml")).unwrap(),
        old
    );
    assert_eq!(state(&store, &store).unwrap(), State::Older(7));
    std::fs::remove_dir_all(store).unwrap();
}

/// Format 11 (owner, 2026-10-11): the five retired limits leave `rune.toml`
/// however they were written, every other value and comment stays, and the
/// four limits Settings now shows are written with their defaults. Checked
/// without a write first; undone to the byte.
#[test]
fn format_11_retires_five_limits_and_shows_four() {
    let store = temp("retired-v11");
    write_marker(&store, 10).unwrap();
    let old = "# The owner's own words.\n\
               attachments = { snapshot_max_size = \"1MiB\", max_text_chars = 500 }\n\
               [tools]\nmax_actions_per_response = 12 # mine\nmax_parallel = 3\n\
               [scan]\nrespect_ignore = false\nhidden = false\nmax_depth = 9\n\
               [checkpoint]\nkeep_turns = 7\n";
    std::fs::write(store.join("rune.toml"), old).unwrap();
    let before = tree(&store);
    check(&store, &store).unwrap();
    assert_eq!(tree(&store), before, "the check changes nothing");
    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert_eq!(marked(&store).unwrap(), Some(VERSION));
    let text = std::fs::read_to_string(store.join("rune.toml")).unwrap();
    for (_, key) in v11::RETIRED {
        assert!(!text.contains(key), "{key} survived:\n{text}");
    }
    assert!(text.contains("# The owner's own words."), "{text}");
    assert!(
        text.contains("max_actions_per_response = 12 # mine"),
        "{text}"
    );
    let config = crate::settings::configuration::Configuration::load(&store).unwrap();
    assert_eq!(config.number("scan.max_depth"), 9, "a kept value is kept");
    assert_eq!(config.number("attachments.max_text_chars"), 500);
    // Shown in Settings, the person's to set — and, unset, not written: the
    // file holds choices only (owner, 2026-10-14).
    for shown in [
        "terminal.max_background_log_size",
        "scan.max_entries",
        "scan.regex_steps",
    ] {
        let (table, key) = shown.split_once('.').unwrap();
        let value: serde_json::Value = toml_edit::de::from_str(&text).unwrap();
        assert!(value[table][key].is_null(), "{shown} written:\n{text}");
        assert!(!crate::settings::configuration::is_internal_invariant(
            shown
        ));
    }
    let backup = std::fs::read_dir(store.join("backups"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("Upgrade11-")
        })
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(backup.join("rune.toml")).unwrap(),
        old
    );
    undo(&store, &store, &backup).unwrap();
    assert_eq!(
        std::fs::read_to_string(store.join("rune.toml")).unwrap(),
        old
    );
    assert_eq!(state(&store, &store).unwrap(), State::Older(10));
    std::fs::remove_dir_all(store).unwrap();
}

/// A retired key written the dotted way, or inside an inline table, goes too;
/// a configuration that names none of them is left byte for byte.
#[test]
fn format_11_retires_keys_written_any_way_and_leaves_others_alone() {
    let dotted = "scan.hidden = true\ncheckpoint.keep_turns = 3\n";
    let retired = v11::retire(dotted).unwrap();
    assert!(
        !retired.contains("hidden") && !retired.contains("keep_turns"),
        "{retired}"
    );
    let inline = "tools = { max_parallel = 2, max_actions_per_response = 9 }\n";
    let retired = v11::retire(inline).unwrap();
    assert!(!retired.contains("max_parallel"), "{retired}");
    assert!(
        retired.contains("max_actions_per_response = 9"),
        "{retired}"
    );
    let untouched = "# nothing retired here\n[scan]\nmax_depth = 4\n";
    assert_eq!(v11::retire(untouched).unwrap(), untouched);
}

#[test]
fn format_12_preserves_explicit_refresh_and_is_read_only_when_checked() {
    let store = temp("refresh-v12");
    write_marker(&store, 11).unwrap();
    let old =
        "# owner choice\n[checkpoint]\nrefresh_interval = \"250ms\"\n[retry]\nmax_attempts = 9\n";
    std::fs::write(store.join("rune.toml"), old).unwrap();
    let before = tree(&store);
    check(&store, &store).unwrap();
    assert_eq!(tree(&store), before);
    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert_eq!(marked(&store).unwrap(), Some(VERSION));
    let config = crate::settings::configuration::Configuration::load(&store).unwrap();
    assert_eq!(
        config.duration("checkpoint.refresh_interval"),
        std::time::Duration::from_millis(250)
    );
    assert_eq!(config.number("retry.max_attempts"), 9);
    let after = tree(&store);
    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert_eq!(
        tree(&store),
        after,
        "upgrade is one ordered step, not a runtime fallback"
    );
    std::fs::remove_dir_all(store).unwrap();
}

/// Format 17 (owner, 2026-10-14): a home that searched through its Serper key
/// keeps searching through it — now as the `serper` connection, the first one
/// added — with the endpoint the person had chosen as its address.
#[test]
fn a_format_16_home_with_a_serper_key_keeps_searching_through_serper() {
    let store = temp("v17-serper");
    write_marker(&store, 16).unwrap();
    let old = "# my search\n[web_search]\nendpoint = \"https://proxy.example/serper/search\"\nresults = 5\n";
    std::fs::write(store.join("rune.toml"), old).unwrap();
    crate::settings::Secrets::load(&store)
        .unwrap()
        .set("serper", "k")
        .unwrap();
    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert_eq!(marked(&store).unwrap(), Some(VERSION));
    let config = crate::settings::configuration::Configuration::load(&store).unwrap();
    assert_eq!(config.number("web_search.results"), 5);
    let services = crate::settings::Services::load(&store).unwrap();
    let serper = services
        .first(crate::settings::services::Service::Search)
        .unwrap();
    assert_eq!(serper.name, "serper");
    assert_eq!(
        serper.base_url.as_deref(),
        Some("https://proxy.example/serper")
    );
    let text = std::fs::read_to_string(store.join("rune.toml")).unwrap();
    assert!(
        text.contains("# my search") && !text.contains("endpoint"),
        "{text}"
    );
    let backup = std::fs::read_dir(store.join("backups"))
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("Upgrade17-"))
        .unwrap()
        .path();
    assert_eq!(
        std::fs::read_to_string(backup.join("rune.toml")).unwrap(),
        old,
        "the file as it was stays in the backup"
    );
}

/// A Serper key with no `rune.toml` at all: the step writes an empty file
/// first, so an undo — by any build — leaves one every build reads.
#[test]
fn a_serper_key_without_a_configuration_is_carried_and_undone_to_an_empty_file() {
    let store = temp("v17-nofile");
    write_marker(&store, 16).unwrap();
    crate::settings::Secrets::load(&store)
        .unwrap()
        .set("serper", "k")
        .unwrap();
    let backup = store.join("backups").join("Upgrade17-test");
    std::fs::create_dir_all(&backup).unwrap();
    let mut plan = Plan {
        marker: Some(16),
        ..Plan::default()
    };
    plan.save(&backup).unwrap();
    v17::apply(&store, &backup, &mut plan).unwrap();
    plan.save(&backup).unwrap();
    let services = crate::settings::Services::load(&store).unwrap();
    assert_eq!(services.list.len(), 1);
    undo(&store, &store, &backup).unwrap();
    assert_eq!(
        std::fs::read_to_string(store.join("rune.toml")).unwrap(),
        ""
    );
    assert_eq!(marked(&store).unwrap(), Some(16));
}

/// No Serper key, no connection: a home that never searched gets no search,
/// and its vault is never created by the step that looked for one.
#[test]
fn a_home_without_a_serper_key_gets_no_connection_and_no_vault() {
    let store = temp("v17-nokey");
    write_marker(&store, 16).unwrap();
    std::fs::write(store.join("rune.toml"), "[web_search]\ntimeout = \"9s\"\n").unwrap();
    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert!(crate::settings::Services::load(&store)
        .unwrap()
        .list
        .is_empty());
    assert!(!store.join("master.key").exists());
    assert!(!store.join(crate::settings::secrets::VAULT_FILE).exists());
}

/// A running build asks the new one `-upgrade-check` before it hands over; a
/// format 17 home whose route in use is not kept yet — every 9router home
/// then — is what format 18 puts right, never a reason to refuse (the first
/// draft of 0.5.7 refused it, caught by a dry run on the owner's file).
#[test]
fn a_format_17_home_with_a_route_in_use_passes_the_upgrade_check() {
    let store = temp("v18-check");
    write_marker(&store, 17).unwrap();
    std::fs::write(
        store.join("rune.toml"),
        "[[services.list]]\nname = \"9router\"\napi = \"9router\"\n\n\
         [services.list.search]\nadded = 7\nroute = \"ag\"\n",
    )
    .unwrap();
    check(&store, &store).expect("ready");
    assert_eq!(marked(&store).unwrap(), Some(17), "a check changes nothing");
    std::fs::write(
        store.join("rune.toml"),
        "[[services.list]]\nname = \"9router\"\napi = \"9router\"\n\n\
         [services.list.search]\nadded = 7\n",
    )
    .unwrap();
    assert!(
        check(&store, &store).is_err(),
        "a home no step can put right is still refused"
    );
}

/// Format 18 (owner, 2026-10-15): a gateway connection keeps the routes the
/// person added; the route each service uses today is the first one kept, so
/// the next route chosen does not erase it.
#[test]
fn a_format_17_home_keeps_each_route_in_use_as_its_first_kept_route() {
    let store = temp("v18-routes");
    write_marker(&store, 17).unwrap();
    let old = "[web_fetch]\ntimeout = \"120s\" # Search request timeout.\n\n\
               [[services.list]]\nname = \"9router\"\napi = \"9router\"\n\n\
               [services.list.search]\nadded = 7\nroute = \"ag\"\n";
    std::fs::write(store.join("rune.toml"), old).unwrap();
    prepare(&store, &store, &[], &mut |_| {}).unwrap();
    assert_eq!(marked(&store).unwrap(), Some(VERSION));
    let services = crate::settings::Services::load(&store).unwrap();
    let nine = services.get("9router").unwrap();
    assert_eq!(nine.routes.search, ["ag"]);
    assert!(nine.routes.fetch.is_empty());
    let text = std::fs::read_to_string(store.join("rune.toml")).unwrap();
    assert!(text.contains("# Public page request timeout."), "{text}");
    let backup = std::fs::read_dir(store.join("backups"))
        .unwrap()
        .flatten()
        .find(|e| e.file_name().to_string_lossy().starts_with("Upgrade18-"))
        .unwrap()
        .path();
    assert_eq!(
        std::fs::read_to_string(backup.join("rune.toml")).unwrap(),
        old,
        "the file as it was stays in the backup"
    );
}

// ── Every older format reaches this build ─────────────────────────────────────
// Owner, 2026-10-16: a release is for the person to reach. Every format an
// install can still be at is upgraded here, and one damaged item never stops
// the upgrade of the rest of the home.

/// Bring the home at `store` to format `to` exactly as the chain of steps
/// does — a home as every format between left it.
fn advance(store: &Path, to: u32) {
    let State::Older(from) = state(store, store).unwrap() else {
        panic!("the fixture is already current");
    };
    for next in from + 1..=to {
        let backup = store.join("backups").join(format!("Fixture{next}"));
        std::fs::create_dir_all(&backup).unwrap();
        let mut plan = Plan {
            marker: marked(store).unwrap(),
            ..Default::default()
        };
        run_step(next, store, store, &backup, &mut plan).unwrap();
        write_marker(store, next).unwrap();
    }
}

#[test]
fn every_older_format_reaches_this_build() {
    for from in 1..VERSION {
        let home = old_home(&format!("every-{from}"));
        let store = &home.store;
        if from > 1 {
            advance(store, from);
        }
        assert_eq!(state(store, store).unwrap(), State::Older(from));
        check(store, store).unwrap_or_else(|e| panic!("format {from} check: {e}"));
        let mut left = Vec::new();
        prepare(store, store, &[], &mut |p| {
            if let Progress::LeftAsItWas(items) = p {
                left.extend(items);
            }
        })
        .unwrap_or_else(|e| panic!("format {from}: {e}"));
        assert_eq!(
            state(store, store).unwrap(),
            State::Current,
            "format {from}"
        );
        assert!(left.is_empty(), "format {from} left {left:?}");
        crate::settings::configuration::Configuration::load(store)
            .unwrap_or_else(|e| panic!("format {from} configuration: {e}"));
        crate::store::Db::open(&store.join("rune.db"), std::time::Duration::from_secs(5))
            .unwrap_or_else(|e| panic!("format {from} database: {e}"));
        let app = crate::project::find(store, &home.project).expect("its folder");
        assert!(
            app.join("sessions/1/session.jsonl").is_file(),
            "format {from}"
        );
        // Done once: a second start changes nothing.
        let before = tree(store);
        prepare(store, store, &[], &mut |_| {}).unwrap();
        assert_eq!(tree(store), before, "format {from}");
    }
}

/// Sessions as a power cut, a permission slip and a dead write left them.
#[test]
fn one_damaged_session_never_stops_the_upgrade_of_the_rest() {
    let home = old_home("damaged-sessions");
    let store = &home.store;
    advance(store, 12);
    let app = crate::project::find(store, &home.project).unwrap();
    let sessions = app.join("sessions");
    let good = sessions.join("1/session.jsonl");
    std::fs::write(&good, "{\"id\":1,\"role\":\"user\",\"content\":\"hi\"}\n").unwrap();
    let torn = sessions.join("3/session.jsonl");
    std::fs::create_dir_all(torn.parent().unwrap()).unwrap();
    let mut torn_bytes = b"{\"id\":1,\"role\":\"user\",\"content\":\"kept\"}\n".to_vec();
    torn_bytes.extend_from_slice(b"\xff\xfe\x00\x00 torn\n");
    std::fs::write(&torn, &torn_bytes).unwrap();
    let locked = sessions.join("4/session.jsonl");
    std::fs::create_dir_all(locked.parent().unwrap()).unwrap();
    std::fs::write(&locked, "{\"id\":1,\"role\":\"user\",\"content\":\"x\"}\n").unwrap();
    let locked_away = Restricted::new(&locked, 0o000).unwrap();
    let other = crate::project::find(store, &home.other)
        .unwrap()
        .join("sessions");
    let other_away = Restricted::new(&other, 0o000).unwrap();
    {
        let db = rusqlite::Connection::open(store.join("rune.db")).unwrap();
        db.execute(
            "INSERT INTO promotions (token, session_id, journal) VALUES ('good', 1, '{\"message\":{\"id\":5,\"role\":\"user\"}}'), ('torn', 1, '{\"message\":')",
            [],
        )
        .unwrap();
    }
    let mut left = Vec::new();
    let result = prepare(store, store, &[], &mut |p| {
        if let Progress::LeftAsItWas(items) = p {
            left.extend(items);
        }
    });
    drop((locked_away, other_away));
    result.unwrap();
    assert_eq!(state(store, store).unwrap(), State::Current);
    let row: serde_json::Value =
        serde_json::from_str(std::fs::read_to_string(&good).unwrap().trim()).unwrap();
    assert_eq!(row["work"]["source"], 1, "the rest is upgraded");
    let torn_after = std::fs::read(&torn).unwrap();
    assert!(
        torn_after.ends_with(b"\xff\xfe\x00\x00 torn\n"),
        "a torn line is kept byte for byte"
    );
    let first = torn_after.split(|b| *b == b'\n').next().unwrap();
    let first: serde_json::Value = serde_json::from_slice(first).unwrap();
    assert_eq!(first["content"], "kept");
    assert_eq!(first["work"]["source"], 1, "its readable rows upgraded");
    assert_eq!(
        std::fs::read_to_string(&locked).unwrap(),
        "{\"id\":1,\"role\":\"user\",\"content\":\"x\"}\n",
        "an unreadable session is left as it was"
    );
    let named = |path: &Path| left.iter().any(|(p, _)| p == path);
    assert!(named(&locked), "{left:?}");
    // The folder nobody may read is named, or what is in it. Windows lets
    // every user open a path beneath a folder it may not list (bypass
    // traverse checking): there the sessions in it are read, upgraded with
    // the rest, and nothing about them is left to name.
    assert!(
        named(&other)
            || left.iter().any(|(p, _)| p.starts_with(&other))
            || crate::platform::OS == crate::platform::Os::Windows,
        "{left:?}"
    );
    assert!(named(&store.join("rune.db")), "{left:?}");
    let db = rusqlite::Connection::open(store.join("rune.db")).unwrap();
    let good_journal: String = db
        .query_row(
            "SELECT journal FROM promotions WHERE token='good'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(good_journal.contains("\"work\""), "{good_journal}");
    let torn_journal: String = db
        .query_row(
            "SELECT journal FROM promotions WHERE token='torn'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(torn_journal, "{\"message\":");
}

#[test]
fn a_drafts_registry_that_does_not_read_never_stops_the_upgrade() {
    let store = home_0_2_9("damaged-drafts");
    std::fs::write(store.join("drafts.json"), "{\"clock\": 4").unwrap();
    let mut left = Vec::new();
    prepare(&store, &store, &[], &mut |p| {
        if let Progress::LeftAsItWas(items) = p {
            left.extend(items);
        }
    })
    .unwrap();
    assert_eq!(state(&store, &store).unwrap(), State::Current);
    assert_eq!(
        std::fs::read_to_string(store.join("drafts.json")).unwrap(),
        "{\"clock\": 4"
    );
    assert!(
        left.iter().any(|(p, _)| p == &store.join("drafts.json")),
        "{left:?}"
    );
}

/// A format 1 folder that still holds something after its contents moved is
/// set aside with everything else format 2 has no place for.
#[test]
fn a_format_1_folder_left_holding_something_is_set_aside_not_a_stop() {
    let home = old_home("leftover");
    let store = &home.store;
    let scratch = store.join("scratch");
    std::fs::write(scratch.join("left-by-hand.txt"), "keep me").unwrap();
    // Unlistable: nothing in it can be moved one by one.
    let unlistable = Restricted::new(&scratch, 0o300).unwrap();
    let result = prepare(store, store, &[], &mut |_| {});
    let kept = std::fs::read_dir(store.join("backups"))
        .unwrap()
        .flatten()
        .flat_map(|backup| walk_files(&backup.path()))
        .find(|path| path.ends_with("left-by-hand.txt"));
    if let Some(path) = &kept {
        crate::platform::set_file_mode(path.parent().unwrap(), 0o700).unwrap();
    }
    drop(unlistable);
    result.unwrap();
    assert_eq!(state(store, store).unwrap(), State::Current);
    assert!(!scratch.exists());
    assert_eq!(
        std::fs::read_to_string(kept.expect("set aside")).unwrap(),
        "keep me"
    );
}

fn walk_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let _ = crate::platform::set_file_mode(dir, 0o700);
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk_files(&path));
        } else {
            out.push(path);
        }
    }
    out
}

/// Owner, 2026-10-16: the shell's default became `auto`. A home whose file
/// still carries `/bin/bash` as a default an older build wrote follows the new
/// default; a file a format 16 build saved holds choices only, so its
/// `/bin/bash` was chosen and stays.
#[test]
fn the_old_shell_default_follows_the_new_one_but_a_chosen_shell_stays() {
    // Homes of these formats were written only by Linux builds (Rune ran on
    // Linux alone before 0.7.0): `/bin/bash` names no drive on Windows, where
    // no such home exists.
    if crate::platform::OS == crate::platform::Os::Windows {
        return;
    }
    let written = temp("shell-written-default");
    write_marker(&written, 15).unwrap();
    std::fs::write(
        written.join("rune.toml"),
        "[terminal]\nshell = \"/bin/bash\"\ncommand_timeout = \"90s\"\n",
    )
    .unwrap();
    prepare(&written, &written, &[], &mut |_| {}).unwrap();
    let config = crate::settings::configuration::Configuration::load(&written).unwrap();
    assert_eq!(config.text("terminal.shell"), "auto");
    assert_eq!(config.duration("terminal.command_timeout").as_secs(), 90);

    let chosen = temp("shell-chosen");
    write_marker(&chosen, 19).unwrap();
    std::fs::write(
        chosen.join("rune.toml"),
        "[terminal]\nshell = \"/bin/bash\"\n",
    )
    .unwrap();
    prepare(&chosen, &chosen, &[], &mut |_| {}).unwrap();
    let config = crate::settings::configuration::Configuration::load(&chosen).unwrap();
    assert_eq!(config.text("terminal.shell"), "/bin/bash");
}

/// Owner, 2026-10-16: a credential store that cannot be opened stopped the
/// format 17 upgrade (it asks the store for a Serper key). Whether one was
/// there is unknown, so nothing is guessed: no connection is made, the person
/// is told, and the store is left exactly as it was.
#[test]
fn a_format_16_home_whose_store_cannot_be_opened_upgrades_and_is_told_about_serper() {
    let home = temp("v17-store-unavailable");
    write_marker(&home, 16).unwrap();
    crate::settings::secrets::write_store(&home, r#"{"serper":"sk-serper-0001"}"#);
    std::fs::remove_file(home.join("master.key")).unwrap();
    let vault = std::fs::read(home.join("secrets.vault")).unwrap();
    let mut left = Vec::new();
    prepare(&home, &home, &[], &mut |p| {
        if let Progress::LeftAsItWas(items) = p {
            left.extend(items);
        }
    })
    .unwrap();
    assert_eq!(state(&home, &home).unwrap(), State::Current);
    let (path, why) = left
        .iter()
        .find(|(p, _)| p.ends_with("secrets.vault"))
        .expect("the person is told");
    assert!(path.starts_with(&home));
    assert!(why.contains("Serper"), "{why}");
    assert_eq!(std::fs::read(home.join("secrets.vault")).unwrap(), vault);
    assert!(!home.join("master.key").exists());
    let config = std::fs::read_to_string(home.join("rune.toml")).unwrap_or_default();
    assert!(!config.contains("serper"), "{config}");
}
