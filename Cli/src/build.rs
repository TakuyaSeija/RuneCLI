//! Which build of Rune runs: whether an installed build can take over, the
//! home brought up to the build that starts, and the way back when it cannot.
//!
//! A release is activated live. A window or a session host that sees a new
//! build installed hands itself over to it when idle (`host.rs`, the window's
//! handover), after asking it — [`ready`] — whether it could take the home
//! over. The build that starts upgrades the home before anything reads it
//! ([`adopt`], `runeengine::upgrade`). An upgrade that fails leaves the home
//! as it was; a process another build handed over goes back to that build —
//! the program it handed on, not a file kept beside the installed one — so a
//! session never loses its host, and the installed build is tried again later.
//! Rune never replaces the installed build itself: whoever installed it (a
//! person, a package manager) owns that file, on every machine.

use std::path::{Path, PathBuf};

use runeengine::upgrade::{self, Progress, State};

/// Where the home lives: the database's directory (sessions, projects, run
/// locks) and the directory holding `rune.toml` — one directory in an
/// install, two when a test points `-db` elsewhere.
fn homes(db: Option<&Path>) -> Result<(PathBuf, PathBuf), String> {
    let settings = runeengine::store::Db::home_dir().map_err(|e| e.to_string())?;
    let store = match db.and_then(Path::parent) {
        Some(parent) => parent.to_path_buf(),
        None => settings.clone(),
    };
    Ok((store, settings))
}

/// Bring the home up to this build before anything reads it, or — when the
/// upgrade fails and was undone — go back to the previous build.
pub(crate) fn adopt(db: Option<&Path>) -> Result<(), String> {
    let (store, settings) = homes(db)?;
    if let State::Newer(version) = upgrade::state(&store, &settings).map_err(|e| e.to_string())? {
        // A newer build's home is refused as it is; going back would not help.
        return Err(format!(
            "this Rune home was written by a newer Rune (format {version}); this build reads format {}",
            upgrade::VERSION
        ));
    }
    // A window keeps its terminal quiet unless there is something to wait
    // for; a session host's stderr is its log, which keeps the whole story.
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stderr());
    let mut report = |progress: Progress| match progress {
        Progress::Waiting(sessions) => eprintln!(
            "rune: updating Rune's data — waiting for {} still open in the previous build to close",
            describe(&sessions)
        ),
        Progress::Upgrading { from, to } if !interactive => {
            eprintln!("rune: upgrading Rune's data from format {from} to {to}")
        }
        Progress::Done { from, to } if !interactive => {
            eprintln!("rune: Rune's data is now format {to} (was {from})")
        }
        // Always said: something of the person's could not be updated.
        Progress::LeftAsItWas(items) => {
            for (path, why) in items {
                eprintln!(
                    "rune: left as it was (it could not be updated): {} — {why}",
                    path.display()
                );
            }
        }
        _ => {}
    };
    match upgrade::prepare(
        &store,
        &settings,
        &crate::handover::owned_sessions(),
        &mut report,
    ) {
        Ok(()) => Ok(()),
        Err(error) => {
            // A home still in an older format is one the build that handed
            // this process over reads: it goes on serving, and asks this
            // build again later (`upgrade::check` holds it back meanwhile).
            let older = matches!(upgrade::state(&store, &settings), Ok(State::Older(_)));
            if let Some(program) = crate::handover::way_home().filter(|_| older) {
                return Err(go_back(program, &error));
            }
            Err(failure(&error, &store))
        }
    }
}

/// What a person is told when this build cannot take their data over: what
/// stopped it, that nothing of theirs changed, and what to do — never a path
/// of a developer's install.
fn failure(error: &runeengine::Error, store: &Path) -> String {
    use std::io::ErrorKind;
    let hint = match error {
        runeengine::Error::Io(e) if e.kind() == ErrorKind::StorageFull => {
            format!(
                "free some disk space where Rune keeps its data ({})",
                store.display()
            )
        }
        runeengine::Error::Io(e)
            if matches!(
                e.kind(),
                ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
            ) =>
        {
            format!("Rune needs to be able to write to {}", store.display())
        }
        _ => "if it happens again, keep this message for a report".into(),
    };
    format!(
        "Rune's data could not be updated for this version: {error}. Nothing of yours was changed. \
         Start Rune again once that is fixed — {hint}."
    )
}

/// Go back to the build that handed this process over, with the same
/// arguments and environment — so it reads its own handover record again.
/// Returns only when that could not start.
fn go_back(program: runeengine::platform::Handed, why: &runeengine::Error) -> String {
    let program = match runeengine::platform::program_at(program) {
        Ok(program) => program,
        Err(error) => return format!("{why}; the previous build could not be resumed: {error}"),
    };
    eprintln!("rune: {why}. Staying on the previous build for now; this one is tried again later.");
    let mut args = std::env::args_os();
    let name = args.next();
    let mut command = runeengine::platform::program_command(&program);
    command.args(args);
    let error = runeengine::platform::replace_process(&mut command, name.as_deref());
    format!("{why}; the previous build could not be resumed: {error}")
}

fn describe(sessions: &[i64]) -> String {
    match sessions {
        [one] => format!("session #{one}"),
        many => format!(
            "sessions {}",
            many.iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// What `-upgrade-check` says, after "ready", of a build that takes over the
/// sessions' ownership locks a host hands on across its exec
/// (`handover::Owner`). A build that does not say it is handed none: it would
/// find its own session locked by the descriptor it inherited.
pub(crate) const ADOPTS_OWNERS: &str = "adopts-owners";

/// `-upgrade-check`: whether this build could take the home over, decided
/// without changing anything. Exit status 0 when it could; on stdout, "ready"
/// and what it takes over from the build before it.
pub(crate) fn check(db: Option<&Path>) -> Result<(), String> {
    let (store, settings) = homes(db)?;
    upgrade::check(&store, &settings).map_err(|e| e.to_string())?;
    println!("ready");
    println!("{ADOPTS_OWNERS}");
    // The format it brings a home to — read by the release's dry run.
    println!("format {}", upgrade::VERSION);
    Ok(())
}

/// Whether another file than the one running is installed at `exe` — a newer
/// build put there by a release. The one test a window, the session picker
/// and a host share for "a release came".
pub(crate) fn replaced(exe: &Path) -> bool {
    use runeengine::platform::{running_program, FileIdentity};
    matches!(
        (running_program(), FileIdentity::of_path(exe)),
        (Ok(running), Ok(installed)) if running != installed
    )
}

/// The installed build's path — where a release puts the next build — not
/// the file this process runs, which a release may have replaced.
// Its callers start hosts and new builds, which unit tests never do.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn installed() -> Result<PathBuf, String> {
    runeengine::platform::installed_program().map_err(|e| e.to_string())
}

/// Whether `theirs` is a later release than `ours`, both `X.Y.Z` (the
/// versioning rule). A version that does not read as one counts as later when
/// it differs at all: following a build that cannot be placed is how a window
/// catches up with one installed in place, under the same file.
pub(crate) fn later(theirs: &str, ours: &str) -> bool {
    fn parts(version: &str) -> Option<(u64, u64, u64)> {
        let mut it = version.trim().split('.').map(|p| p.parse::<u64>().ok());
        let parsed = (it.next()??, it.next()??, it.next()??);
        it.next().is_none().then_some(parsed)
    }
    match (parts(theirs), parts(ours)) {
        (Some(theirs), Some(ours)) => theirs > ours,
        _ => theirs != ours,
    }
}

/// What the build installed at a path answered `-upgrade-check`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Readiness {
    /// It starts, and could take this home over.
    pub ok: bool,
    /// It takes over the ownership locks a host hands on ([`ADOPTS_OWNERS`]).
    pub adopts_owners: bool,
}

/// Whether the build installed at `path` could take this home over: it
/// starts, and its `-upgrade-check` — which reads the configuration, the
/// database and the layout without changing them — answers yes in time.
///
/// Asked before a host or a window tears itself down for a new build, so a
/// release that cannot run is caught while the running one still serves.
/// One probe per installed build while it says yes, and one a minute
/// ([`ASK_AGAIN`]) while it says no: the window asks about every second while
/// an update is pending.
pub(crate) fn ready(path: &Path, db: Option<&Path>) -> bool {
    readiness(path, db).ok
}

/// How long a build that answered "not ready" is left before it is asked again.
const ASK_AGAIN: std::time::Duration = std::time::Duration::from_secs(60);

/// [`ready`], with what else the build said of itself.
pub(crate) fn readiness(path: &Path, db: Option<&Path>) -> Readiness {
    use runeengine::platform::FileIdentity;
    use std::io::Read;
    use std::process::Stdio;
    /// The installed file asked about, its answer, and when.
    type Verdict = (FileIdentity, Readiness, std::time::Instant);
    static VERDICT: std::sync::Mutex<Option<Verdict>> = std::sync::Mutex::new(None);
    let identity = FileIdentity::of_path(path).ok();
    if let (Some(identity), Some((seen, verdict, at))) = (
        identity,
        *VERDICT.lock().unwrap_or_else(|error| error.into_inner()),
    ) {
        // A yes holds for the build; a no is asked again a while later — a
        // full disk freed or a held-back upgrade's wait over must not leave a
        // window on the old build until it is closed.
        if seen == identity && (verdict.ok || at.elapsed() < ASK_AGAIN) {
            return verdict;
        }
    }
    let verdict = (|| {
        let mut probe = runeengine::platform::program_command(path);
        probe.arg("-upgrade-check");
        if let Some(db) = db {
            probe.arg("-db").arg(db);
        }
        let Ok(mut child) = probe.stdout(Stdio::piped()).stderr(Stdio::null()).spawn() else {
            return Readiness::default();
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    // A few short lines: the pipe held them whole.
                    let mut said = String::new();
                    if let Some(mut out) = child.stdout.take() {
                        let _ = out.read_to_string(&mut said);
                    }
                    return Readiness {
                        ok: status.success(),
                        adopts_owners: status.success()
                            && said.lines().any(|line| line.trim() == ADOPTS_OWNERS),
                    };
                }
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Readiness::default();
                }
            }
        }
    })();
    if let Some(identity) = identity {
        *VERDICT.lock().unwrap_or_else(|error| error.into_inner()) =
            Some((identity, verdict, std::time::Instant::now()));
    }
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_is_later_by_its_numbers_not_its_spelling() {
        assert!(later("0.2.2", "0.2.1"));
        assert!(later("0.10.0", "0.9.9"), "numbers, not text");
        assert!(later("1.0.0", "0.99.9"));
        assert!(!later("0.2.1", "0.2.2"), "an earlier host");
        assert!(!later("0.2.2", "0.2.2"));
        assert!(later("0.2.2-dev", "0.2.2"), "unplaceable and different");
        assert!(!later("weird", "weird"));
    }
}
