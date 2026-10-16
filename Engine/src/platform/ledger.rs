//! The record of what a confinement leaves on disk, so a person can take
//! all of it back with one command (`rune -sandbox reset`; owner,
//! 2026-10-17).
//!
//! Landlock and Seatbelt hold a confinement in the kernel and leave nothing
//! behind. Windows decides who reaches a file by the file's own ACL, so a
//! confined command is let into its project by an entry written on the
//! project folder — and an entry written is one a person may want gone. Each
//! is recorded here before it is written, so an entry Rune made is never one
//! it forgot: one line per path and access, `write<TAB>path` or
//! `read<TAB>path`, in a file of this user's alone in Rune's home, which no
//! confined command reaches. Every reader and writer holds the file's lock,
//! so a session recording an entry and a reset taking them back never lose
//! one between them.
//!
//! What a ledger holds and how it is kept is the same on every OS; only the
//! taking back is the OS's (`forget_grants`).

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// One recorded entry: a path, and whether it was granted writing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RecordedGrant {
    pub path: PathBuf,
    pub write: bool,
}

impl RecordedGrant {
    /// Its line, or `None` for a path no line holds whole: one that is not
    /// Unicode, or that holds a line break or a tab (no Windows name does).
    fn line(&self) -> Option<String> {
        let path = self.path.to_str()?;
        if path.is_empty() || path.contains(['\n', '\r', '\t']) {
            return None;
        }
        let access = if self.write { "write" } else { "read" };
        Some(format!("{access}\t{path}"))
    }

    fn parse(line: &str) -> Option<RecordedGrant> {
        let (access, path) = line.split_once('\t')?;
        let write = match access {
            "write" => true,
            "read" => false,
            _ => return None,
        };
        (!path.is_empty()).then(|| RecordedGrant {
            path: PathBuf::from(path),
            write,
        })
    }
}

/// What taking entries back did ([`forget_grants`](super::forget_grants)).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Forgotten {
    /// Entries taken off their paths.
    pub removed: usize,
    /// Entries already gone: their path no longer exists, or holds none.
    pub absent: usize,
    /// Entries that could not be taken back, with why — still recorded, for
    /// another try.
    pub kept: Vec<(RecordedGrant, String)>,
}

/// What the OS found taking one entry back.
#[cfg_attr(not(windows), allow(dead_code))]
pub(in crate::platform) enum Taken {
    Removed,
    Absent,
}

/// Every entry `text` records, each once, in the order first recorded; a line
/// that is not an entry is let go.
fn entries(text: &str) -> Vec<RecordedGrant> {
    let mut seen = HashSet::new();
    text.lines()
        .filter_map(RecordedGrant::parse)
        .filter(|grant| seen.insert(grant.clone()))
        .collect()
}

/// Every entry the ledger at `path` records. A ledger that does not exist
/// records nothing.
pub fn recorded_grants(path: &Path) -> io::Result<Vec<RecordedGrant>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut held = Ledger::hold(path)?;
    Ok(entries(&held.text()?))
}

/// A ledger held: no other process records in it or takes from it until it
/// is let go.
pub(in crate::platform) struct Ledger {
    file: File,
    listed: Option<HashSet<RecordedGrant>>,
}

impl Ledger {
    /// The ledger at `path` — made, this user's alone, when missing — held.
    pub(in crate::platform) fn hold(path: &Path) -> io::Result<Ledger> {
        let file = super::open_private(path, super::Private::Lock)?;
        file.lock()?;
        Ok(Ledger { file, listed: None })
    }

    fn text(&mut self) -> io::Result<String> {
        let mut bytes = Vec::new();
        self.file.seek(SeekFrom::Start(0))?;
        self.file.read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Record `grant` — on disk, before the entry is written — unless it is
    /// recorded already.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(in crate::platform) fn note(&mut self, grant: &RecordedGrant) -> io::Result<()> {
        if self.listed.is_none() {
            let listed = entries(&self.text()?).into_iter().collect();
            self.listed = Some(listed);
        }
        if self.listed.as_ref().is_some_and(|l| l.contains(grant)) {
            return Ok(());
        }
        let line = grant.line().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} has a name the record of Rune's grants cannot hold",
                    grant.path.display()
                ),
            )
        })?;
        // A last line a crash cut short is ended first, so the new one is a
        // line of its own.
        let end = self.file.seek(SeekFrom::End(0))?;
        let mut last = [0u8; 1];
        if end > 0 {
            self.file.seek(SeekFrom::End(-1))?;
            self.file.read_exact(&mut last)?;
            self.file.seek(SeekFrom::End(0))?;
        }
        let separator = if end > 0 && last[0] != b'\n' {
            "\n"
        } else {
            ""
        };
        self.file
            .write_all(format!("{separator}{line}\n").as_bytes())?;
        self.file.sync_data()?;
        if let Some(listed) = self.listed.as_mut() {
            listed.insert(grant.clone());
        }
        Ok(())
    }

    /// Take back every entry recorded with `take`, keeping recorded only what
    /// could not be taken back.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(in crate::platform) fn forget(
        &mut self,
        take: impl Fn(&RecordedGrant) -> io::Result<Taken>,
    ) -> io::Result<Forgotten> {
        let mut forgotten = Forgotten::default();
        let mut kept = String::new();
        for grant in entries(&self.text()?) {
            match take(&grant) {
                Ok(Taken::Removed) => forgotten.removed += 1,
                Ok(Taken::Absent) => forgotten.absent += 1,
                Err(error) if error.kind() == io::ErrorKind::NotFound => forgotten.absent += 1,
                Err(error) => {
                    if let Some(line) = grant.line() {
                        kept.push_str(&line);
                        kept.push('\n');
                    }
                    forgotten.kept.push((grant, error.to_string()));
                }
            }
        }
        self.file.set_len(0)?;
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(kept.as_bytes())?;
        self.file.sync_data()?;
        self.listed = None;
        Ok(forgotten)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(path: &str, write: bool) -> RecordedGrant {
        RecordedGrant {
            path: PathBuf::from(path),
            write,
        }
    }

    fn ledger_in(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-ledger-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("sandbox-grants.txt")
    }

    /// An entry is recorded once, whatever number of times it is noted, and
    /// read back as it was noted; a ledger that does not exist records
    /// nothing, and asking makes none.
    #[test]
    fn an_entry_is_recorded_once_and_read_back() {
        let path = ledger_in("once");
        assert_eq!(recorded_grants(&path).unwrap(), []);
        assert!(!path.exists(), "reading makes no ledger");
        let project = grant(r"C:\Users\me\work", true);
        let cargo = grant(r"C:\Users\me\.cargo", false);
        {
            let mut held = Ledger::hold(&path).unwrap();
            held.note(&project).unwrap();
            held.note(&cargo).unwrap();
            held.note(&project).unwrap();
        }
        Ledger::hold(&path).unwrap().note(&cargo).unwrap();
        assert_eq!(recorded_grants(&path).unwrap(), [project, cargo]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "write\tC:\\Users\\me\\work\nread\tC:\\Users\\me\\.cargo\n"
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A last line a crash cut short stays a line of its own, and lines that
    /// are no entry are let go when read.
    #[test]
    fn a_line_cut_short_never_joins_the_next() {
        let path = ledger_in("cut");
        std::fs::write(&path, "write\tC:\\one\nnot an entry\nread\tC:\\tw").unwrap();
        Ledger::hold(&path)
            .unwrap()
            .note(&grant(r"C:\three", true))
            .unwrap();
        assert_eq!(
            recorded_grants(&path).unwrap(),
            [
                grant(r"C:\one", true),
                grant(r"C:\tw", false),
                grant(r"C:\three", true)
            ]
        );
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// A reset takes back what it can, counts what was gone already, and
    /// keeps recorded — with why — only what it could not take back.
    #[test]
    fn a_reset_keeps_recorded_only_what_stayed() {
        let path = ledger_in("reset");
        {
            let mut held = Ledger::hold(&path).unwrap();
            for entry in [
                grant(r"C:\taken", true),
                grant(r"C:\gone", true),
                grant(r"C:\locked", false),
                grant(r"C:\empty", false),
            ] {
                held.note(&entry).unwrap();
            }
        }
        let forgotten = Ledger::hold(&path)
            .unwrap()
            .forget(|entry| match entry.path.to_str().unwrap() {
                r"C:\taken" => Ok(Taken::Removed),
                r"C:\gone" => Err(io::Error::from(io::ErrorKind::NotFound)),
                r"C:\locked" => Err(io::Error::from(io::ErrorKind::PermissionDenied)),
                _ => Ok(Taken::Absent),
            })
            .unwrap();
        assert_eq!(forgotten.removed, 1);
        assert_eq!(forgotten.absent, 2);
        assert_eq!(forgotten.kept.len(), 1);
        assert_eq!(forgotten.kept[0].0, grant(r"C:\locked", false));
        assert_eq!(
            recorded_grants(&path).unwrap(),
            [grant(r"C:\locked", false)]
        );
        // A second reset finds nothing left to take but what stayed.
        let again = Ledger::hold(&path)
            .unwrap()
            .forget(|_| Ok(Taken::Removed))
            .unwrap();
        assert_eq!((again.removed, again.kept.len()), (1, 0));
        assert_eq!(recorded_grants(&path).unwrap(), []);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    /// No line can hold a path that is not one line: it is refused, never
    /// written cut in two.
    #[test]
    fn a_path_no_line_holds_is_refused() {
        let path = ledger_in("refused");
        let mut held = Ledger::hold(&path).unwrap();
        let error = held.note(&grant("C:\\a\nwrite\tC:\\", true)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        drop(held);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}
