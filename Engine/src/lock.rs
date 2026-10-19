//! Locks between Rune's processes on the files of its home.
//!
//! Every hold another process must see — a session's lease and its host's
//! ownership, the configuration, the credential store, the drafts registry,
//! a project's memory, the help in use, an upgrade — is one of these: a lock
//! on a whole file, owned by the open file. A crash frees it with the process;
//! an `exec` keeps it when the file is handed on (`platform::hand_on`), which
//! is how a session's host changes build without its session ever being free.
//! The standard library's file locks (`flock` on Linux — the lock every
//! earlier build took, so two builds side by side during a handover exclude
//! each other).
//!
//! A lock is a [`Lock`], never a bare file: a child of this process must not
//! hold it while it starts (`platform::keep_from_children`), and only the
//! type knows to take that note back before the file closes.

use std::fs::{File, TryLockError};
use std::path::Path;
use std::time::Duration;

use crate::error::{Error, Result};

/// Whether taking a lock may wait for its holder to let go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// Answer at once: a lock held elsewhere is an `Io` error of kind
    /// `WouldBlock`, which callers read as "in use".
    No,
    /// Wait for it — at most [`PATIENCE`], then an error of kind `TimedOut`.
    Yes,
}

/// How long [`Wait::Yes`] waits. Contention is normally a few milliseconds
/// (another window saving); a slow disk gets this long before a wait becomes
/// an error.
pub const PATIENCE: Duration = Duration::from_secs(10);

/// A lock held: the open file it belongs to, kept from every child this
/// process starts. Let go when dropped.
#[derive(Debug)]
pub struct Lock {
    // Declared first, so dropped first: the note goes before the descriptor
    // it names closes, and a child never closes a number reused meanwhile.
    _kept: crate::platform::KeptFromChildren,
    file: File,
}

impl Lock {
    fn noted(file: File) -> Lock {
        Lock {
            _kept: crate::platform::keep_from_children(&file),
            file,
        }
    }

    /// A lock the build before this one handed on across its `exec`.
    pub fn adopt(file: File) -> Lock {
        Lock::noted(file)
    }

    /// The file, for handing on across an `exec` (`platform::hand_on`), no
    /// longer kept from children: done last before the `exec`.
    pub fn into_file(self) -> File {
        // The note goes with the destructuring, before the file is returned.
        let Lock { file, .. } = self;
        file
    }
}

impl std::ops::Deref for Lock {
    type Target = File;

    fn deref(&self) -> &File {
        &self.file
    }
}

/// `path` held alone: no other holder, shared or exclusive.
pub fn exclusive(path: &Path, wait: Wait) -> Result<Lock> {
    take(path, wait, Kind::Exclusive, PATIENCE)
}

/// `path` held beside other shared holders, never beside an exclusive one.
pub fn shared(path: &Path, wait: Wait) -> Result<Lock> {
    take(path, wait, Kind::Shared, PATIENCE)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Shared,
    Exclusive,
}

fn try_lock(file: &File, kind: Kind) -> std::result::Result<(), TryLockError> {
    match kind {
        Kind::Shared => file.try_lock_shared(),
        Kind::Exclusive => file.try_lock(),
    }
}

fn take(path: &Path, wait: Wait, kind: Kind, patience: Duration) -> Result<Lock> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = Lock::noted(crate::platform::open_private(
        path,
        crate::platform::Private::Lock,
    )?);
    match try_lock(&file.file, kind) {
        Ok(()) => return Ok(file),
        Err(TryLockError::Error(error)) => return Err(error.into()),
        Err(TryLockError::WouldBlock) if wait == Wait::No => {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!("{} is held by another process", path.display()),
            )))
        }
        Err(TryLockError::WouldBlock) => {}
    }
    // The wait is the kernel's, woken by the release itself: a waiter that
    // polled on its own clock lost every release to a holder that took the
    // lock straight back, and timed out
    // (`a_wait_for_a_lock_is_not_starved_by_one_that_keeps_taking_it_back`).
    //
    // The lock belongs to the open file `waiter` shares with `file`. Given up
    // on, the waiter's copy is its last one: the thread takes the lock when it
    // is released, finds no one to hand it to, and closing the copy frees it
    // again.
    let waiter = Lock::noted(file.file.try_clone()?);
    let (taken, answer) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("lock-wait".into())
        .spawn(move || {
            let outcome = loop {
                let held = match kind {
                    Kind::Shared => waiter.file.lock_shared(),
                    Kind::Exclusive => waiter.file.lock(),
                };
                match held {
                    Ok(()) => break Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) => break Err(error),
                }
            };
            let _ = taken.send(outcome);
        })?;
    match answer.recv_timeout(patience) {
        Ok(Ok(())) => Ok(file),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("timed out waiting for lock on {}", path.display()),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A tag per test: these run in parallel in one process, and a shared
    // directory means one test deletes another's file mid-run.
    fn home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-lock-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A wait for a lock is answered while another process keeps taking it
    /// back: two sessions writing their memory index in turn lost the race on
    /// every release under load and timed out
    /// (`memory::two_sessions_changing_the_index_at_once_lose_nothing`,
    /// 2026-10-15).
    #[test]
    fn a_wait_for_a_lock_is_not_starved_by_one_that_keeps_taking_it_back() {
        let path = home("starved").join("held.lock");
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let holder = {
            let (path, stop) = (path.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let held = exclusive(&path, Wait::Yes).unwrap();
                    std::thread::sleep(Duration::from_millis(5));
                    drop(held);
                }
            })
        };
        std::thread::sleep(Duration::from_millis(50));
        let start = std::time::Instant::now();
        let got = take(&path, Wait::Yes, Kind::Exclusive, Duration::from_secs(3));
        let waited = start.elapsed();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let ok = got.is_ok();
        drop(got);
        holder.join().unwrap();
        assert!(ok, "starved for {waited:?}");
        assert!(waited < Duration::from_secs(1), "waited {waited:?}");
    }

    /// A wait that is given up on frees what it would have taken: the lock is
    /// free again once its holder lets go, for a waiter that came later.
    #[test]
    fn a_wait_given_up_on_leaves_the_lock_free_once_its_holder_lets_go() {
        let path = home("given-up").join("held.lock");
        let held = exclusive(&path, Wait::Yes).unwrap();
        let start = std::time::Instant::now();
        let error = take(
            &path,
            Wait::Yes,
            Kind::Exclusive,
            Duration::from_millis(200),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("timed out waiting for lock"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(held);
        // The abandoned waiter takes the lock on release and lets it go.
        let again = take(&path, Wait::Yes, Kind::Exclusive, Duration::from_secs(3));
        assert!(again.is_ok(), "{:?}", again.err());
        drop(again);
        // An abandoned waiter holds it only between taking and closing it.
        assert!(
            take(&path, Wait::Yes, Kind::Exclusive, Duration::from_secs(1)).is_ok(),
            "nothing is left holding it"
        );
    }

    /// Shared holders stand together; an exclusive one stands alone, and
    /// "in use" is told by its kind, never by its words.
    #[test]
    fn shared_holders_stand_together_and_exclusive_alone() {
        let path = home("kinds").join("lease.lock");
        let first = shared(&path, Wait::No).unwrap();
        let second = shared(&path, Wait::No).unwrap();
        let busy = exclusive(&path, Wait::No).unwrap_err();
        assert!(
            matches!(&busy, Error::Io(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "{busy}"
        );
        drop((first, second));
        let alone = exclusive(&path, Wait::No).unwrap();
        assert!(matches!(
            shared(&path, Wait::No),
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock
        ));
        drop(alone);
        assert!(
            crate::platform::is_private(&path).unwrap(),
            "a lock file is this user's alone"
        );
    }
}
