//! The agent's memory — what it knows about a project that outlives a session.
//!
//! The design follows the industry standard for agent memory: a **directory** the
//! agent owns, an **index** file it curates itself, and that index — and nothing
//! else — handed to the model in the context a session opens with. No
//! embeddings, no graph, no vector store, and no expansion of the files the index
//! happens to point at.
//!
//! # The index is the whole injection
//!
//! `MEMORY.md` holds one line per note; the note itself lives in its own file
//! beside it. Only the index reaches the model. A note reaches it when the model
//! opens it with `inspect_file` on its `@memory/…` path — the moment it decided
//! the note mattered — which is the rule the mainstream agent-memory standard
//! follows, and the reason a single 15 KB note can never be charged to every
//! request for the rest of the project's life.
//!
//! # Read when a session opens, not while it runs
//!
//! The index is read when a session opens and again when its history is
//! compacted, never between: a note another session saves meanwhile reaches
//! this one at its next opening, as it does in Claude Code. A note this session
//! saves is already known to the model that wrote it.
//!
//! # No memory tools, deliberately
//!
//! Memory is managed by the model itself through the existing file tools —
//! `inspect_file`, `find_files`, `search_text`, `create_file` and `patch_file`
//! already read and write files. A `memory_write` tool would be a second
//! function doing an existing function's job.
//!
//! # Where it lives, and what may never come out of it
//!
//! `memory/` inside the project's own folder under the engine's home (see
//! [`crate::project`]), so two projects never share notes.
//!
//! It sits under the engine's home, which the kernel sandbox does **not** grant:
//! a shell command the agent runs cannot see it. It is reachable only through
//! the engine's own file tools, via the jail's annex (see [`crate::jail`]).
//! That is the narrowest door that still makes the feature work.
//!
//! **Nothing the engine enforces may ever be read from here.** Memory is text
//! the agent writes, so memory is prompt-injection territory: no permission, no
//! path allow-list, no limit, no configuration. It is prose for the model and
//! nothing else, and it reaches the model only through
//! [`crate::frame::block`], which keeps it from forging Rune's tags.
//!
//! # Shared by every session of the project
//!
//! The index is the one file several sessions — in several processes — edit,
//! and a change to it is read, then written: two sessions changing it at once
//! would each write over the other's line. So a change to a memory file is
//! held against every other session's for its whole read and write
//! ([`Memory::hold`]), and the write replaces the file whole
//! ([`Hold::write`]): a session opening while another writes reads the old
//! index or the new one, never half of it.
//!
//! # One budget, in bytes
//!
//! The index is held to `memory.recall_size` and nothing else, read at the
//! moment it is applied. A second bound — a line count, say — would be a second
//! knob turning the same constraint.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::media::human;

/// How the model addresses its memory. Not a real path: the jail translates it,
/// so the model never types a machine-specific absolute path and a project file
/// can never be mistaken for a note.
pub const ALIAS: &str = "@memory";

/// The index the agent curates. One line per note, **newest first** — which is
/// what makes the truncation below keep the freshest entries.
pub const INDEX_FILE: &str = "MEMORY.md";

/// What an over-budget index is told to do about it. One sentence, one place:
/// the note beside a cut recall and the notice after a write that overflowed
/// are both built from it, so they cannot drift apart.
const COMPACT_ADVICE: &str =
    "Keep one line per entry, newest at the top, move detail into topic files, and \
     prune or merge older entries from the bottom. A topic file is never shown on \
     its own: read it with inspect_file when you need it.";

/// One project's memory directory.
#[derive(Debug, Clone)]
pub struct Memory {
    dir: PathBuf,
}

/// One change to a file, held from the read it starts with to the write it
/// ends with. See [`Memory::hold`].
#[derive(Debug)]
pub struct Hold {
    /// The memory's canonical directory and its lock, when the file is one of
    /// the memory's.
    memory: Option<(PathBuf, std::fs::File)>,
}

impl Hold {
    /// Write `bytes` to `path`. A memory file is replaced whole or not at all,
    /// because other sessions read the index while this one writes it; any
    /// other file is written in place, so it keeps its inode, its mode and
    /// every link to it.
    pub fn write(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        match &self.memory {
            Some((dir, _lock)) if path.starts_with(dir) => {
                crate::settings::secrets::atomic_write(path, bytes)
            }
            Some((dir, _lock)) => Err(crate::error::Error::invalid(format!(
                "{} is not in the memory held at {}",
                path.display(),
                dir.display()
            ))),
            None => Ok(std::fs::write(path, bytes)?),
        }
    }
}

/// The index as the model is shown it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recall {
    /// The index, or a line saying there is none yet.
    pub index: String,
    /// Set when the index outgrew its budget and its last lines were left out.
    pub cut: Option<String>,
}

impl Memory {
    pub fn new(dir: PathBuf) -> Memory {
        Memory { dir }
    }

    /// Create the directory and hand back its canonical path.
    ///
    /// Canonical because the jail compares canonicalised prefixes: an annex that
    /// still contained a symlink would let a path pass the lexical check and land
    /// somewhere else entirely.
    pub fn ensure(&self) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)?;
        Ok(crate::platform::canonical(&self.dir)?)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX_FILE)
    }

    /// Hold `path` for one change, taken before the file is read for it.
    ///
    /// A memory file is held against every other session of the project, in
    /// every process, until the [`Hold`] is dropped; any other file is held by
    /// nothing here. The lock sits beside the directory rather than in it, so
    /// the model never sees it among its notes.
    pub fn hold(&self, path: &Path) -> Result<Hold> {
        let Ok(dir) = crate::platform::canonical(&self.dir) else {
            return Ok(Hold { memory: None });
        };
        if !path.starts_with(&dir) {
            return Ok(Hold { memory: None });
        }
        let lock =
            crate::lock::exclusive(&self.dir.with_extension("lock"), crate::lock::Wait::Yes)?;
        Ok(Hold {
            memory: Some((dir, lock)),
        })
    }

    /// The index, held to `budget` bytes. Files it names are not opened here;
    /// the model opens the one it wants.
    ///
    /// Past the budget the tail is cut at a line boundary and [`Recall::cut`]
    /// says so, because the index is newest-first and a line the model cannot
    /// see is worse than one it knows is missing.
    ///
    /// Missing file, unreadable file and empty file all mean the same thing to
    /// the model — there is nothing remembered yet — so they answer the same way
    /// rather than surfacing an error it cannot act on.
    pub fn recall(&self, budget: usize) -> Recall {
        let fresh = || {
            Recall {
            index: format!(
                "(no memories saved yet — create {ALIAS}/{INDEX_FILE} when you learn something worth keeping)"
            ),
            cut: None,
        }
        };
        let Some((text, total)) = self.read_index(budget) else {
            return fresh();
        };
        if text.trim().is_empty() {
            return fresh();
        }
        let index = cap_index(text.trim_end(), budget);
        Recall {
            cut: (total > budget).then(|| {
                format!(
                    "{} Only its first {} are shown here.",
                    verdict(total, budget).unwrap_or_default(),
                    crate::tools::plural(index.lines().count(), "line")
                )
            }),
            index,
        }
    }

    /// The index as it is saved now, as a digest read in pieces: whether it
    /// changed since the model was handed it, whatever budget cuts what it is
    /// handed. Empty when there is no index to read.
    pub fn index_digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let Ok(mut file) = std::fs::File::open(self.index_path()) else {
            return String::new();
        };
        let mut hasher = Sha256::new();
        let mut piece = [0u8; 8192];
        loop {
            match file.read(&mut piece) {
                Ok(0) => break,
                Ok(n) => hasher.update(&piece[..n]),
                Err(_) => return String::new(),
            }
        }
        format!("{:x}", hasher.finalize())
    }

    /// What an index that has outgrown its budget is told, or `None` while it
    /// still fits.
    pub fn over_budget_at(index: &Path, budget: usize) -> Option<String> {
        let bytes = std::fs::metadata(index).ok()?.len() as usize;
        verdict(bytes, budget)
    }

    /// Read at most one byte past the budget, so the cap can be judged honestly
    /// without ever pulling an arbitrarily large file into memory. Reports the
    /// file's real size alongside, because "over by one byte" is a worse answer
    /// than "over by 40 KB" for a model deciding what to cut.
    ///
    /// Size and bytes come from one open file: the index is replaced whole
    /// when it changes, and a size read by name could belong to the index
    /// before the one being read.
    fn read_index(&self, budget: usize) -> Option<(String, usize)> {
        let file = std::fs::File::open(self.index_path()).ok()?;
        let total = file.metadata().ok()?.len() as usize;
        let mut bytes = Vec::new();
        file.take(budget as u64 + 1).read_to_end(&mut bytes).ok()?;
        Some((String::from_utf8_lossy(&bytes).to_string(), total))
    }
}

/// Whether `shown` — a path as the jail displays it — is the memory index.
pub fn is_index(shown: &str) -> bool {
    shown == format!("{ALIAS}/{INDEX_FILE}")
}

/// The single overrun verdict.
fn verdict(bytes: usize, budget: usize) -> Option<String> {
    (bytes > budget).then(|| {
        format!(
            "{INDEX_FILE} is {} over its {} budget, so its last lines no longer reach \
             the context a session opens with. {COMPACT_ADVICE}",
            human((bytes - budget) as u64),
            human(budget as u64)
        )
    })
}

/// Cut the index at a line boundary, never mid-line: half an entry is worse than
/// no entry, because the model cannot tell which half it is holding.
fn cap_index(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let mut kept: Vec<&str> = Vec::new();
    let mut used = 0usize;
    for line in text.lines() {
        if used + line.len() + 1 > budget {
            break;
        }
        used += line.len() + 1;
        kept.push(line);
    }
    kept.join("\n").trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped default lives in `rune.toml`; this is what a test means by
    /// "a full index", not a second copy of the configuration.
    const CAP: usize = 20 * 1024;

    fn at(name: &str) -> (Memory, PathBuf) {
        let dir = std::env::temp_dir().join("rune-test").join(format!(
            "runemem-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        (Memory::new(dir.clone()), dir)
    }

    fn scratch(name: &str) -> Memory {
        at(name).0
    }

    /// Two sessions of one project changing the index at once — each reads
    /// it, changes its own line, writes it back, exactly as `patch_file` does —
    /// lose nothing: every change is held from its read to its write.
    #[test]
    fn two_sessions_changing_the_index_at_once_lose_nothing() {
        let (first, dir) = at("race");
        first.ensure().unwrap();
        let second = Memory::new(dir);
        let lines: String = (0..50).map(|i| format!("a{i}\nb{i}\n")).collect();
        std::fs::write(first.index_path(), lines).unwrap();
        let change = |memory: Memory, from: char, to: char| {
            std::thread::spawn(move || {
                let index = memory.ensure().unwrap().join(INDEX_FILE);
                for i in 0..50 {
                    let held = memory.hold(&index).unwrap();
                    let text = std::fs::read_to_string(&index).unwrap();
                    let changed = text.replacen(&format!("{from}{i}\n"), &format!("{to}{i}\n"), 1);
                    held.write(&index, changed.as_bytes()).unwrap();
                }
            })
        };
        let a = change(first.clone(), 'a', 'A');
        let b = change(second, 'b', 'B');
        a.join().unwrap();
        b.join().unwrap();
        let text = std::fs::read_to_string(first.index_path()).unwrap();
        for i in 0..50 {
            assert!(text.contains(&format!("A{i}\n")), "lost A{i}:\n{text}");
            assert!(text.contains(&format!("B{i}\n")), "lost B{i}:\n{text}");
        }
    }

    /// A session opening while another rewrites the index reads the old index
    /// or the new one — never an empty or half-written file.
    #[test]
    fn the_index_is_never_read_half_written() {
        let (memory, _) = at("torn");
        let index = memory.ensure().unwrap().join(INDEX_FILE);
        let old = "- old note\n".repeat(2000);
        let new = "- new note\n".repeat(2000);
        std::fs::write(&index, &old).unwrap();
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let (index, done) = (index.clone(), done.clone());
            let (old, new) = (old.clone(), new.clone());
            std::thread::spawn(move || {
                let mut reads = 0u32;
                while !done.load(std::sync::atomic::Ordering::Relaxed) || reads == 0 {
                    let seen = std::fs::read_to_string(&index).unwrap();
                    assert!(seen == old || seen == new, "read {} bytes", seen.len());
                    reads += 1;
                }
            })
        };
        for round in 0..200 {
            let held = memory.hold(&index).unwrap();
            let text = if round % 2 == 0 { &new } else { &old };
            held.write(&index, text.as_bytes()).unwrap();
        }
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        reader.join().unwrap();
    }

    /// Only the memory's own files are held, and a hold writes nowhere else;
    /// the lock sits beside the notes, never among them.
    #[test]
    fn only_the_memorys_files_are_held() {
        let (memory, dir) = at("held");
        let canonical = memory.ensure().unwrap();
        let outside = dir.with_file_name(format!("runemem-{}-outside.txt", std::process::id()));
        // Two holds on a file elsewhere at once: neither waits on the other.
        let first = memory.hold(&outside).unwrap();
        let second = memory.hold(&outside).unwrap();
        second.write(&outside, b"in place").unwrap();
        drop((first, second));
        assert_eq!(std::fs::read(&outside).unwrap(), b"in place");

        let held = memory.hold(&canonical.join(INDEX_FILE)).unwrap();
        assert!(held.write(&outside, b"escaped").is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"in place");
        held.write(&canonical.join(INDEX_FILE), b"- a note\n")
            .unwrap();
        let listed: Vec<_> = std::fs::read_dir(&canonical)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(listed, vec![std::ffi::OsString::from(INDEX_FILE)]);
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn ensure_creates_the_directory_and_canonicalises_it() {
        let (m, dir) = at("ensure");
        let canon = m.ensure().unwrap();
        assert!(canon.is_dir());
        assert_eq!(canon, crate::platform::canonical(&dir).unwrap());
    }

    #[test]
    fn an_empty_memory_still_tells_the_model_the_facility_exists() {
        let m = scratch("bootstrap");
        m.ensure().unwrap();
        let r = m.recall(CAP);
        assert!(r.index.contains(ALIAS), "recall must name the alias: {r:?}");
        assert!(r.index.contains(INDEX_FILE));
        assert_eq!(r.cut, None);
    }

    #[test]
    fn a_whitespace_only_index_reads_as_empty() {
        let m = scratch("blank");
        m.ensure().unwrap();
        std::fs::write(m.index_path(), "   \n\n\t\n").unwrap();
        assert!(m.recall(CAP).index.contains("no memories saved yet"));
    }

    #[test]
    fn recall_returns_the_index_verbatim_when_it_is_small() {
        let m = scratch("small");
        m.ensure().unwrap();
        std::fs::write(m.index_path(), "- [Build](build.md) — cargo, not npm\n").unwrap();
        assert_eq!(
            m.recall(CAP),
            Recall {
                index: "- [Build](build.md) — cargo, not npm".into(),
                cut: None
            }
        );
    }

    /// The regression this whole module is built around: a note the index points
    /// at is a file on disk, not prompt text. It is opened by the model when the
    /// model decides it matters, and never before.
    #[test]
    fn a_note_the_index_points_at_is_not_injected() {
        let m = scratch("notinlined");
        m.ensure().unwrap();
        std::fs::create_dir_all(m.dir.join("projects")).unwrap();
        std::fs::write(
            m.dir.join("projects/deploy.md"),
            "The staging password is hunter2 and nobody knows it.\n",
        )
        .unwrap();
        std::fs::write(
            m.index_path(),
            "- [Deploy](projects/deploy.md) — staging runs on 3200\n",
        )
        .unwrap();

        let r = m.recall(CAP).index;
        assert!(r.contains("- [Deploy](projects/deploy.md) — staging runs on 3200"));
        assert!(
            !r.contains("hunter2"),
            "the note's own text reached the prompt: {r}"
        );
    }

    #[test]
    fn a_bare_path_reference_is_not_inlined_either() {
        let m = scratch("barepath");
        m.ensure().unwrap();
        std::fs::write(m.dir.join("notes.md"), "SECRET NOTE BODY\n").unwrap();
        std::fs::write(m.index_path(), "- @memory/notes.md — pointer\n").unwrap();
        let r = m.recall(CAP).index;
        assert!(r.contains("@memory/notes.md"), "{r}");
        assert!(!r.contains("SECRET NOTE BODY"), "{r}");
    }

    #[test]
    fn an_oversized_index_is_cut_at_a_line_boundary_and_says_so() {
        let m = scratch("big");
        m.ensure().unwrap();
        let line = "- [note](n.md) — ".to_string() + &"x".repeat(60) + "\n";
        let mut text = String::new();
        while text.len() < CAP * 2 {
            text.push_str(&line);
        }
        std::fs::write(m.index_path(), &text).unwrap();

        let r = m.recall(CAP);
        let cut = r.cut.expect("truncation must be announced");
        assert!(cut.contains("over its 20.0 KB budget"), "{cut}");
        assert!(cut.contains(COMPACT_ADVICE), "{cut}");
        assert!(
            cut.contains(&format!("first {} lines", r.index.lines().count())),
            "{cut}"
        );
        for l in r.index.lines() {
            assert_eq!(l, line.trim_end(), "a line was cut in half");
        }
        assert!(r.index.len() <= CAP, "the cap did not hold");
    }

    #[test]
    fn truncation_keeps_the_newest_entries_which_are_at_the_top() {
        let m = scratch("newest");
        m.ensure().unwrap();
        let mut text = String::from("- NEWEST\n");
        while text.len() < CAP * 2 {
            text.push_str("- filler filler filler filler filler filler filler\n");
        }
        text.push_str("- OLDEST\n");
        std::fs::write(m.index_path(), &text).unwrap();

        let r = m.recall(CAP).index;
        assert!(r.contains("- NEWEST"));
        assert!(!r.contains("- OLDEST"));
    }

    /// The budget is the caller's value at the moment of the call: a budget
    /// raised in `rune.toml` applies to the next recall, not to the next session.
    #[test]
    fn the_budget_is_read_at_each_recall() {
        let m = scratch("livecap");
        m.ensure().unwrap();
        std::fs::write(m.index_path(), "- a\n- b\n- c\n").unwrap();
        assert!(m.recall(4).cut.is_some());
        assert!(m.recall(CAP).cut.is_none());
    }

    #[test]
    fn ordinary_angle_brackets_in_a_note_survive() {
        let m = scratch("prose");
        m.ensure().unwrap();
        std::fs::write(
            m.index_path(),
            "- [Tmpl](t.md) — use Vec<T> and a < b comparison\n",
        )
        .unwrap();
        let r = m.recall(CAP).index;
        assert!(r.contains("Vec<T>"), "{r}");
        assert!(r.contains("a < b comparison"), "{r}");
    }

    #[test]
    fn a_write_that_fits_the_budget_produces_no_notice() {
        let m = scratch("okwrite");
        m.ensure().unwrap();
        std::fs::write(m.index_path(), "- [A](a.md) — fine\n").unwrap();
        assert_eq!(Memory::over_budget_at(&m.index_path(), CAP), None);
    }

    #[test]
    fn a_write_past_the_byte_cap_names_the_overage_in_its_own_units() {
        let m = scratch("overbyte");
        m.ensure().unwrap();
        std::fs::write(m.index_path(), "x".repeat(4096)).unwrap();
        let notice = Memory::over_budget_at(&m.index_path(), 1024).unwrap();
        assert!(notice.contains("3.0 KB over its 1.0 KB budget"), "{notice}");
    }

    #[test]
    fn only_the_index_path_is_the_index() {
        assert!(is_index("@memory/MEMORY.md"));
        assert!(!is_index("@memory/projects/MEMORY.md"));
        assert!(!is_index("MEMORY.md"));
    }
}
