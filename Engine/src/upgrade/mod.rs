//! Bringing an engine home written by an older Rune up to this build, before
//! anything reads it.
//!
//! # Why a release can change the data under a running Rune
//!
//! A release is activated live: a session host or a window that sees a new
//! build installed hands itself over to it, without the person relaunching
//! anything (`Cli/src/host.rs`, `Cli/src/ui`). The build that takes over is
//! the one that knows what changed, so it is the one that upgrades the home —
//! here, as its very first step, before configuration, database or sessions
//! are opened. An older build never has to know what a newer one needs.
//!
//! # One version for the home
//!
//! `version.json` beside the database names the home's format ([`VERSION`]).
//! Each step takes the home from one version to the next; the steps run in
//! order and each runs once. The database keeps its own schema version, and
//! its migration runs inside the same upgrade, so no release changes the
//! data without passing through here.
//!
//! # Nobody may be using what moves
//!
//! An upgrade waits until every process of the previous build has let go of
//! the home — no session lease and no session host lock is held — and holds
//! `run/Upgrade.lock` while it works, so a process of this build starting
//! meanwhile waits for it rather than reading a home halfway between formats.
//! The previous build's processes are already on their way out: the release
//! they saw installed is what makes each of them hand over when idle.
//!
//! # Nothing is lost, and a failed upgrade is undone
//!
//! An upgrade deletes nothing: what the new format no longer has is moved
//! into `backups/`, and every change is written down before it is made
//! (`backups/Upgrade<N>-<time>/plan.json`). A step that fails is undone in
//! reverse; an upgrade cut short by a crash is undone the next time any
//! build starts (`run/Upgrade.active`). The home is then exactly what the
//! previous build left, and the frontend returns to that build.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

mod defaults;
mod v10;
mod v11;
mod v12;
mod v13;
mod v14;
mod v15;
mod v16;
mod v17;
mod v18;
mod v19;
mod v2;
mod v20;
mod v3;
mod v4;
mod v5;
mod v6;
mod v7;
mod v8;
mod v9;

/// The home format this build reads.
pub const VERSION: u32 = 20;

const MARKER: &str = "version.json";
const LOCK: &str = "run/Upgrade.lock";
const ACTIVE: &str = "run/Upgrade.active";

#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    version: u32,
}

/// Where a home stands against this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Written in this build's format (or nothing written yet).
    Current,
    /// Written by an older build: an upgrade from this version is due.
    Older(u32),
    /// Written by a newer build than this one.
    Newer(u32),
}

/// What a waiting upgrade reports, so a person is told why nothing happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Sessions still open in a previous build.
    Waiting(Vec<i64>),
    /// The upgrade is running.
    Upgrading { from: u32, to: u32 },
    /// The upgrade finished.
    Done { from: u32, to: u32 },
    /// What the upgrade left as it was — each with why — because it could
    /// not be read or changed; everything else was upgraded.
    LeftAsItWas(Vec<(PathBuf, String)>),
}

/// The format of the home at `store` (the database's directory), whose
/// configuration lives in `settings`.
///
/// A home without a marker was written before the marker existed (format 1,
/// told apart by its shape), or by the engine alone — which never writes
/// anything a later format would not find — or not at all.
pub fn state(store: &Path, settings: &Path) -> Result<State> {
    let version = match marked(store)? {
        Some(version) => version,
        None if v2::written_by_version_1(store) || v2::configured_by_version_1(settings) => 1,
        // Written by the engine alone, which writes nothing a later format
        // would not find, or not written at all.
        None => VERSION,
    };
    Ok(match version.cmp(&VERSION) {
        std::cmp::Ordering::Less => State::Older(version),
        std::cmp::Ordering::Equal => State::Current,
        std::cmp::Ordering::Greater => State::Newer(version),
    })
}

/// Whether the files under `store` are still in an older format. The engine
/// refuses to start on such a home: its frontend upgrades it first.
pub fn pending(store: &Path) -> Result<bool> {
    if store.join(ACTIVE).exists() {
        return Ok(true);
    }
    Ok(match marked(store)? {
        Some(version) => version < VERSION,
        None => v2::written_by_version_1(store),
    })
}

fn marked(store: &Path) -> Result<Option<u32>> {
    match std::fs::read(store.join(MARKER)) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice::<Marker>(&bytes)
                .map_err(|e| Error::invalid(format!("{}: {e}", store.join(MARKER).display())))?
                .version,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Bring the home up to this build, waiting for every process of a previous
/// build to let go of it first. `store` is the database's directory and
/// `settings` the directory holding `rune.toml` — the same directory in an
/// install, two in a test.
///
/// Returns once the home is current. On failure the home is back exactly as
/// it was, and the error says what stopped the upgrade.
///
/// `own` names the sessions whose ownership lock this process already holds:
/// a host that `exec`d this build keeps its sessions' locks across the exec
/// (`session_<id>.Host.lock`, handed on so a session never lapses), and those
/// are this build's, not the previous one's. Waiting for them would wait for
/// this very process — and for every other host that `exec`d this build and
/// queues for the upgrade behind it, each still holding its own sessions'
/// locks: three hosts taking 0.2.8 over at once waited for one another for
/// good (2026-10-09). So, before it queues, this process leaves a note on
/// each lock it carries ([`Carrying`]), and the barrier passes over a lock
/// whose note names a process still alive.
pub fn prepare(
    store: &Path,
    settings: &Path,
    own: &[i64],
    report: &mut dyn FnMut(Progress),
) -> Result<()> {
    let result = bring_up(store, settings, own, report);
    match &result {
        Ok(()) => Refusal::clear(store),
        // Only a home this build still has to upgrade is one the build
        // before goes on serving — and one this build should not be handed
        // again at once.
        Err(error) if matches!(state(store, settings), Ok(State::Older(_))) => {
            Refusal::note(store, error)
        }
        Err(_) => {}
    }
    result
}

fn bring_up(
    store: &Path,
    settings: &Path,
    own: &[i64],
    report: &mut dyn FnMut(Progress),
) -> Result<()> {
    if !store.join(ACTIVE).exists() {
        match state(store, settings)? {
            State::Current => return mark_fresh(store),
            State::Newer(version) => return Err(newer(version)),
            State::Older(_) => {}
        }
    }
    std::fs::create_dir_all(store.join("run"))?;
    let _carrying = Carrying::note(store, own)?;
    let _lock = hold(&store.join(LOCK))?;
    // Another process of this build may have finished while this one waited.
    if let Some(unfinished) = active(store)? {
        undo(store, settings, &unfinished)?;
    }
    let from = match state(store, settings)? {
        State::Current => return mark_fresh(store),
        State::Newer(version) => return Err(newer(version)),
        State::Older(version) => version,
    };
    wait_for_previous_build(store, own, report)?;
    preflight(store, settings)?;
    report(Progress::Upgrading { from, to: VERSION });
    let mut version = from;
    let mut left = Vec::new();
    while version < VERSION {
        let next = version + 1;
        let backup = store
            .join("backups")
            .join(format!("Upgrade{next}-{}", crate::store::now_ms()));
        std::fs::create_dir_all(&backup).map_err(|e| at(&backup, e))?;
        // The plan names the marker it starts from before the upgrade is
        // marked active, so an undo — even after a crash this early — puts
        // back exactly the version the home had.
        let mut plan = Plan {
            marker: marked(store)?,
            ..Plan::default()
        };
        plan.save(&backup)?;
        crate::settings::secrets::atomic_write(
            &store.join(ACTIVE),
            backup.to_string_lossy().as_bytes(),
        )?;
        let outcome = run_step(next, store, settings, &backup, &mut plan)
            .and_then(|()| write_marker(store, next));
        if let Err(error) = outcome {
            plan.save(&backup)?;
            undo(store, settings, &backup)?;
            let message =
                format!("upgrading Rune's data to format {next} failed and was undone: {error}");
            // What went wrong stays what it was (a full disk is still one), so
            // the person is told what to do about it.
            return Err(match error {
                Error::Io(e) => Error::Io(std::io::Error::new(e.kind(), message)),
                _ => Error::invalid(message),
            });
        }
        plan.save(&backup)?;
        std::fs::remove_file(store.join(ACTIVE))?;
        left.extend(plan.skipped);
        version = next;
    }
    if !left.is_empty() {
        report(Progress::LeftAsItWas(left));
    }
    report(Progress::Done { from, to: VERSION });
    Ok(())
}

/// Take the home from format `next - 1` to `next`, every change written into
/// `plan` before it is made.
fn run_step(
    next: u32,
    store: &Path,
    settings: &Path,
    backup: &Path,
    plan: &mut Plan,
) -> Result<()> {
    match next {
        2 => v2::apply(store, settings, backup, plan),
        3 => v3::apply(store, settings, backup, plan),
        4 => v4::apply(store, settings, backup, plan),
        5 => v5::apply(store, backup, plan),
        6 => v6::apply(store, backup, plan),
        7 => v7::apply(store, backup, plan),
        8 => v8::apply(settings, backup, plan),
        9 => v9::apply(store, backup, plan),
        10 => v10::apply(store, backup, plan),
        11 => v11::apply(settings, backup, plan),
        12 => v12::apply(settings, backup, plan),
        13 => v13::apply(store, settings, backup, plan),
        14 => v14::apply(settings, backup, plan),
        15 => v15::apply(settings, backup, plan),
        16 => v16::apply(store, settings, backup, plan),
        17 => v17::apply(settings, backup, plan),
        18 => v18::apply(settings, backup, plan),
        19 => v19::apply(settings, backup, plan),
        20 => v20::apply(settings, backup, plan),
        _ => Err(Error::Internal(format!(
            "no upgrade step to version {next}"
        ))),
    }
}

/// Whether a process is upgrading the home at `store` now: it holds the
/// upgrade lock, which nothing takes but an upgrade ([`prepare`], and only
/// for a home in an older format). A host taking a newer build over serves
/// again only once that is done — and the upgrade waits for every session of
/// the previous build to close, one still answering a turn included.
pub fn underway(store: &Path) -> bool {
    matches!(
        crate::lock::exclusive(&store.join(LOCK), crate::lock::Wait::No),
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock
    )
}

/// Whether this build could take over the home at `store` without being
/// refused — checked without changing anything, so a running build can ask
/// it of a newly installed one before handing over.
pub fn check(store: &Path, settings: &Path) -> Result<()> {
    if let Some(refusal) = Refusal::read(store).filter(Refusal::stands) {
        return Err(Error::invalid(format!(
            "bringing Rune's data up to this build failed {} time(s), last: {}; it is tried again in {} s",
            refusal.attempts,
            refusal.reason,
            refusal.wait().as_secs().max(1)
        )));
    }
    match state(store, settings)? {
        State::Current => {}
        State::Newer(version) => return Err(newer(version)),
        State::Older(version) => {
            if version < 2 {
                v2::check(store, settings)?;
            }
            if version < 3 {
                v3::check(settings, version)?;
            }
            if version < 4 {
                v4::check(settings, version)?;
            }
            // Validates the whole chain up to this format: `validate` judges
            // every step's text as format 11 leaves it.
            v8::check(settings, version)?;
            return Ok(());
        }
    }
    crate::settings::configuration::Configuration::load(settings)?;
    Ok(())
}

/// Rewrite `rune.toml` in `settings` through `upgrade` — the one way a step
/// changes the configuration: the new text must load in this build before
/// anything is written, and the file as it was is saved in the backup and
/// named in the plan first, so an undo puts it back. A file the step leaves
/// as it is, or no file at all, is not touched.
fn rewrite_configuration(
    settings: &Path,
    backup: &Path,
    plan: &mut Plan,
    upgrade: impl Fn(&str) -> Result<String>,
) -> Result<()> {
    let path = settings.join(crate::settings::configuration::FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let upgraded = upgrade(&text)?;
    if upgraded == text {
        return Ok(());
    }
    validate(&upgraded)?;
    let saved = backup.join(crate::settings::configuration::FILE);
    std::fs::copy(&path, &saved)?;
    plan.config = Some(saved);
    plan.save(backup)?;
    crate::settings::secrets::atomic_write(&path, upgraded.as_bytes())
}

/// An upgraded configuration must load in this build, or the upgrade stops
/// before anything is written. A step before the last is judged as the whole
/// upgrade will leave it: the keys formats 11, 14 and 17 retire go, and the
/// routes format 18 keeps are kept, by the end of the same upgrade, so a step
/// that only carries one along is not refused for it — nor is a home asked
/// `-upgrade-check` before the steps that put it right have run.
fn validate(text: &str) -> Result<()> {
    let text = &v19::config(&v18::seed(&v17::retire(&v14::retire(&v11::retire(
        text,
    )?)?)?)?)?;
    let value: serde_json::Value = toml_edit::de::from_str(text)
        .map_err(|e| Error::invalid(format!("{}: {e}", crate::settings::configuration::FILE)))?;
    crate::settings::configuration::Configuration::from_values(value).map(|_| ())
}

fn newer(version: u32) -> Error {
    Error::invalid(format!(
        "this Rune home was written by a newer Rune (format {version}); this build reads format {VERSION}"
    ))
}

/// The folders in `dir`; a listing that cannot be read is noted and passed.
fn folders(dir: &Path, plan: &mut Plan) -> Vec<PathBuf> {
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            let mut out: Vec<_> = entries
                .flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .map(|entry| entry.path())
                .collect();
            out.sort();
            out
        }
        Err(error) => {
            plan.skip(dir, error);
            Vec::new()
        }
    }
}

/// Rewrite the record files `names` of every session under `store` through
/// `change` — the one way a step changes session records. The file as it was
/// goes into the backup and the plan first, so an undo puts it back. A file
/// or folder that cannot be read, or a file this user may not write, is left
/// as it was and noted: one damaged session never stops the upgrade of the
/// rest. Only a failure to keep the backup copy stops it.
fn rewrite_sessions(
    store: &Path,
    backup: &Path,
    plan: &mut Plan,
    names: &[&str],
    change: impl Fn(&str, &[u8]) -> Vec<u8>,
) -> Result<()> {
    let projects = store.join("projects");
    if !projects.is_dir() {
        return Ok(());
    }
    for project in folders(&projects, plan) {
        let sessions = project.join("sessions");
        if !sessions.is_dir() {
            continue;
        }
        for session in folders(&sessions, plan) {
            for name in names {
                let path = session.join(name);
                if !path.is_file() {
                    continue;
                }
                let old = match std::fs::read(&path) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        plan.skip(&path, error);
                        continue;
                    }
                };
                let next = change(name, &old);
                if next == old {
                    continue;
                }
                let saved = backup
                    .join("history")
                    .join(path.strip_prefix(store).expect("found under the home"));
                if let Some(parent) = saved.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                crate::settings::secrets::atomic_write(&saved, &old)?;
                plan.files.push((path.clone(), saved));
                plan.save(backup)?;
                match crate::settings::secrets::atomic_write(&path, &next) {
                    Ok(()) => {}
                    Err(Error::Io(error))
                        if error.kind() == std::io::ErrorKind::PermissionDenied =>
                    {
                        plan.files.pop();
                        plan.skip(&path, error);
                        plan.save(backup)?;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(())
}

/// A record file with each line `change` alters written anew; every other
/// line — one that is not UTF-8 or not JSON (a write a power cut ended)
/// included — kept byte for byte.
fn rewrite_lines(bytes: &[u8], mut change: impl FnMut(&mut serde_json::Value) -> bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        let content = line.strip_suffix(b"\n").unwrap_or(line);
        let changed = std::str::from_utf8(content)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
            .and_then(|mut row| change(&mut row).then_some(row))
            .and_then(|row| serde_json::to_vec(&row).ok());
        match changed {
            Some(row) => {
                out.extend_from_slice(&row);
                if line.ends_with(b"\n") {
                    out.push(b'\n');
                }
            }
            None => out.extend_from_slice(line),
        }
    }
    out
}

/// Room an upgrade keeps beyond the database copy it may take: the copies of
/// the configuration and session files it rewrites, and the new forms.
const HEADROOM: u64 = 64 << 20;

/// What an upgrade needs before it changes anything: leave to write where it
/// works, and room for the copies it keeps. Found wanting, nothing has been
/// touched — the build before keeps the home, and the upgrade is tried again.
fn preflight(store: &Path, settings: &Path) -> Result<()> {
    for dir in [store, settings] {
        std::fs::create_dir_all(dir).map_err(|e| at(dir, e))?;
        let probe = dir.join(".upgrade-probe");
        match crate::settings::secrets::atomic_write(&probe, b"probe") {
            Err(Error::Io(e)) => return Err(at(dir, e)),
            other => other?,
        }
        std::fs::remove_file(&probe).map_err(|e| at(&probe, e))?;
    }
    let size = |path: PathBuf| std::fs::metadata(path).map_or(0, |m| m.len());
    let need = size(store.join("rune.db"))
        + size(store.join("rune.db-wal"))
        + size(settings.join(crate::settings::configuration::FILE))
        + HEADROOM;
    let free = crate::platform::free_space(store)?;
    if free < need {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::StorageFull,
            format!(
                "updating Rune's data needs about {} free in {}, and {} is",
                crate::media::human(need),
                store.display(),
                crate::media::human(free)
            ),
        )));
    }
    Ok(())
}

/// An I/O failure naming where it happened, its kind kept.
fn at(path: &Path, error: std::io::Error) -> Error {
    Error::Io(std::io::Error::new(
        error.kind(),
        format!("{}: {error}", path.display()),
    ))
}

/// That this build could not bring the home up to it, kept in the home: every
/// window and host of the build before asks this build before handing itself
/// over (`check`), and waits — longer after each failure — instead of each
/// failing the same way at once. A person starting Rune always tries again.
#[derive(Debug, Serialize, Deserialize)]
struct Refusal {
    build: String,
    attempts: u32,
    at: i64,
    reason: String,
}

const REFUSED: &str = "run/Upgrade.refused";
/// How long a handover waits after the n-th failure in a row.
const RETRY_AFTER: [u64; 4] = [60, 300, 900, 3600];

impl Refusal {
    fn read(store: &Path) -> Option<Refusal> {
        serde_json::from_slice(&std::fs::read(store.join(REFUSED)).ok()?).ok()
    }

    fn note(store: &Path, error: &Error) {
        let attempts = Refusal::read(store)
            .filter(|r| r.build == crate::VERSION)
            .map_or(1, |r| r.attempts.saturating_add(1));
        let refusal = Refusal {
            build: crate::VERSION.into(),
            attempts,
            at: crate::store::now_ms(),
            reason: error.to_string(),
        };
        if let Ok(bytes) = serde_json::to_vec(&refusal) {
            let _ = crate::settings::secrets::atomic_write(&store.join(REFUSED), &bytes);
        }
    }

    fn clear(store: &Path) {
        let _ = std::fs::remove_file(store.join(REFUSED));
    }

    /// Still in force: it names this build and its wait is not over.
    fn stands(&self) -> bool {
        self.build == crate::VERSION && !self.wait().is_zero()
    }

    fn wait(&self) -> std::time::Duration {
        let index = (self.attempts.max(1) as usize - 1).min(RETRY_AFTER.len() - 1);
        let until = self.at + RETRY_AFTER[index] as i64 * 1000;
        std::time::Duration::from_millis((until - crate::store::now_ms()).max(0) as u64)
    }
}

/// An untouched home is this build's from the start.
fn mark_fresh(store: &Path) -> Result<()> {
    if store.join(MARKER).exists() {
        return Ok(());
    }
    std::fs::create_dir_all(store)?;
    write_marker(store, VERSION)
}

fn write_marker(store: &Path, version: u32) -> Result<()> {
    crate::settings::secrets::atomic_write(
        &store.join(MARKER),
        &serde_json::to_vec(&Marker { version })?,
    )
}

/// Take `path` exclusively, however long another process holds it — an
/// upgrade in another process is worth waiting for.
fn hold(path: &Path) -> Result<std::fs::File> {
    loop {
        match crate::lock::exclusive(path, crate::lock::Wait::No) {
            Ok(file) => return Ok(file),
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Wait until no session is open anywhere: every lease and every host lock
/// free, except the host locks a process of this build carries across its
/// exec ([`Carrying`]). The processes holding the rest are a previous build's
/// — this build's processes wait on the upgrade lock before they open
/// anything.
fn wait_for_previous_build(
    store: &Path,
    own: &[i64],
    report: &mut dyn FnMut(Progress),
) -> Result<()> {
    let mut told: Option<Vec<i64>> = None;
    loop {
        let held = sessions_held(store, own)?;
        if held.is_empty() {
            return Ok(());
        }
        if told.as_ref() != Some(&held) {
            report(Progress::Waiting(held.clone()));
            told = Some(held);
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// Sessions whose lease or host lock a process of the previous build holds.
fn sessions_held(store: &Path, own: &[i64]) -> Result<Vec<i64>> {
    let Ok(entries) = std::fs::read_dir(store.join("run")) else {
        return Ok(Vec::new());
    };
    let mut held = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(id) = name
            .strip_prefix("session_")
            .and_then(|rest| rest.strip_suffix(".lock"))
            .map(|rest| rest.trim_end_matches(".Host"))
            .and_then(|id| id.parse::<i64>().ok())
        else {
            continue;
        };
        if name.ends_with(".Host.lock") && (own.contains(&id) || Carrying::carried(store, id)) {
            continue;
        }
        match crate::lock::exclusive(&entry.path(), crate::lock::Wait::No) {
            Ok(_) => {}
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if !held.contains(&id) {
                    held.push(id);
                }
            }
            Err(e) => return Err(e),
        }
    }
    held.sort_unstable();
    Ok(held)
}

/// The ownership locks this process carried across its exec, noted for the
/// barrier of whichever process of this build runs the upgrade
/// (`run/session_<id>.carried`: this process's id and when it began, so a
/// number the kernel hands out again is not taken for it). The notes go when
/// the upgrade is over for this process, however it ended.
struct Carrying {
    notes: Vec<PathBuf>,
}

#[derive(serde::Serialize, serde::Deserialize, PartialEq)]
struct Carrier {
    pid: u32,
    started: u64,
}

impl Carrying {
    fn note(store: &Path, own: &[i64]) -> Result<Carrying> {
        let mut carrying = Carrying { notes: Vec::new() };
        let pid = std::process::id();
        let Some(started) = crate::platform::process_started(pid) else {
            return Ok(carrying);
        };
        for id in own {
            let note = crate::store::run_files::carried(store, *id);
            crate::settings::secrets::atomic_write(
                &note,
                &serde_json::to_vec(&Carrier { pid, started })?,
            )?;
            carrying.notes.push(note);
        }
        Ok(carrying)
    }

    /// Whether session `id`'s ownership lock is carried by a process of this
    /// build that is alive — the one the note names, not another that came to
    /// have its number.
    fn carried(store: &Path, id: i64) -> bool {
        std::fs::read(crate::store::run_files::carried(store, id))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Carrier>(&bytes).ok())
            .is_some_and(|carrier| {
                crate::platform::process_started(carrier.pid) == Some(carrier.started)
            })
    }
}

impl Drop for Carrying {
    fn drop(&mut self) {
        for note in &self.notes {
            let _ = std::fs::remove_file(note);
        }
    }
}

/// The backup folder of an upgrade that did not finish, if one did not.
fn active(store: &Path) -> Result<Option<PathBuf>> {
    match std::fs::read_to_string(store.join(ACTIVE)) {
        Ok(text) => Ok(Some(PathBuf::from(text.trim()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Everything one upgrade step changed, written down before it is changed.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Plan {
    /// Renames, in the order they were made.
    pub moves: Vec<(PathBuf, PathBuf)>,
    /// Folders this step created; removed again on undo.
    pub created: Vec<PathBuf>,
    /// Folders this step removed once they were empty; created again on undo.
    pub emptied: Vec<PathBuf>,
    /// `rune.toml` as it was, saved in the backup folder, when the step
    /// rewrote it.
    pub config: Option<PathBuf>,
    /// The database copy taken before its schema changed, when it did.
    pub database: Option<PathBuf>,
    /// Files this step rewrote in place, each with its copy as it was, saved
    /// in the backup folder; put back on undo.
    #[serde(default)]
    pub files: Vec<(PathBuf, PathBuf)>,
    /// What this step left as it was because it could not read or change
    /// it — one damaged item never stops the rest (owner, 2026-10-16). Told
    /// to the person; the item is unchanged, so nothing needs undoing.
    #[serde(default)]
    pub skipped: Vec<(PathBuf, String)>,
    /// The home's version marker before this step; `None` when it had none
    /// (format 1). An undo writes it back, so a home whose step failed reads
    /// as the format it still is.
    #[serde(default)]
    pub marker: Option<u32>,
}

const PLAN: &str = "plan.json";

impl Plan {
    /// Leave `path` as it was, saying why.
    pub(crate) fn skip(&mut self, path: &Path, why: impl std::fmt::Display) {
        self.skipped.push((path.to_path_buf(), why.to_string()));
    }

    fn save(&self, backup: &Path) -> Result<()> {
        crate::settings::secrets::atomic_write(
            &backup.join(PLAN),
            &serde_json::to_vec_pretty(self)?,
        )
    }

    fn load(backup: &Path) -> Result<Plan> {
        match std::fs::read(backup.join(PLAN)) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Plan::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Make the planned renames, in order. The plan is saved before the first
    /// one, so a crash part way leaves a plan that undoes exactly what was
    /// done: undo reverses only a rename whose target exists and whose source
    /// does not.
    pub fn make_moves(&self, backup: &Path) -> Result<()> {
        self.save(backup)?;
        for (from, to) in &self.moves {
            if to.exists() {
                return Err(Error::invalid(format!(
                    "{} already exists; {} was not moved onto it",
                    to.display(),
                    from.display()
                )));
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(from, to)?;
        }
        Ok(())
    }
}

/// Undo the upgrade whose backup folder is `backup`: every rename reversed,
/// the folders it created removed, the ones it emptied restored, the
/// configuration and database put back, and the version marker as it was.
fn undo(store: &Path, settings: &Path, backup: &Path) -> Result<()> {
    let plan = Plan::load(backup)?;
    if let Some(saved) = plan.database.as_ref().filter(|saved| saved.is_file()) {
        restore_database(store, saved)?;
    }
    for (from, to) in plan.moves.iter().rev() {
        if to.exists() && !from.exists() {
            if let Some(parent) = from.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(to, from)?;
        }
    }
    for folder in plan.created.iter().rev() {
        remove_created(folder)?;
    }
    for folder in &plan.emptied {
        std::fs::create_dir_all(folder)?;
    }
    if let Some(saved) = &plan.config {
        std::fs::copy(saved, settings.join(crate::settings::configuration::FILE))?;
    }
    for (live, saved) in plan.files.iter().rev() {
        if saved.is_file() {
            crate::settings::secrets::atomic_write(live, &std::fs::read(saved)?)?;
        }
    }
    match plan.marker {
        Some(version) => write_marker(store, version)?,
        None => {
            let _ = std::fs::remove_file(store.join(MARKER));
        }
    }
    if store.join(ACTIVE).exists() {
        std::fs::remove_file(store.join(ACTIVE))?;
    }
    Ok(())
}

/// Remove a project folder this upgrade created — only when nothing but its
/// identity and empty folders is left in it. Anything else stays: undo never
/// deletes a file it did not make.
fn remove_created(folder: &Path) -> Result<()> {
    fn empty_but_identity(dir: &Path, top: bool) -> bool {
        std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().all(|entry| {
                let path = entry.path();
                if path.is_dir() {
                    empty_but_identity(&path, false)
                } else {
                    top && entry.file_name() == "project.json"
                }
            })
        })
    }
    if folder.is_dir() && empty_but_identity(folder, true) {
        crate::project::remove(folder)?;
    }
    Ok(())
}

/// Put the database back as it was before its schema migration. Only ever
/// done under the upgrade lock with no session open, so nothing holds it.
fn restore_database(store: &Path, saved: &Path) -> Result<()> {
    let db = store.join("rune.db");
    let staged = store.join("rune.db.restore");
    std::fs::copy(saved, &staged)?;
    for suffix in ["-wal", "-shm"] {
        let side = store.join(format!("rune.db{suffix}"));
        if side.exists() {
            std::fs::remove_file(side)?;
        }
    }
    std::fs::rename(staged, db)?;
    Ok(())
}

#[cfg(test)]
mod product_tests;
#[cfg(test)]
mod tests;
