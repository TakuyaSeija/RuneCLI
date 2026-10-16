//! Content-addressed blob store.
//!
//! A blob is named by the SHA-256 of its bytes, so storing the same content
//! twice costs nothing and a name can never disagree with what it points at.
//!
//! **Deliberately outside the project directory.** The obvious home would be
//! `<project>/.rune/objects`, and it is where the design doc first put it — but
//! the agent runs shell commands inside the project, and `rm -rf .rune` would
//! then destroy the only undo history the user has. Keeping blobs in the engine's
//! own data directory puts them outside every writable root the sandbox grants.

use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};

pub struct Objects {
    /// The project's `objects/` folder (see `crate::project`).
    root: PathBuf,
}

impl Objects {
    pub fn new(root: PathBuf) -> Objects {
        Objects { root }
    }

    pub fn root(&self) -> PathBuf {
        self.root.clone()
    }

    /// Store `bytes`, returning the hash that names them. Storing content that is
    /// already present is a no-op.
    pub fn put(&self, bytes: &[u8]) -> Result<String> {
        let hash = hash_of(bytes);
        let path = self.path_of(&hash);
        if path.exists() {
            return Ok(hash);
        }
        let dir = path
            .parent()
            .ok_or_else(|| Error::invalid("bad object path"))?;
        std::fs::create_dir_all(dir)?;

        crate::settings::secrets::atomic_write(&path, bytes)?;
        Ok(hash)
    }

    pub fn get(&self, hash: &str) -> Result<Vec<u8>> {
        if !is_hash(hash) {
            return Err(Error::invalid("Checkpoint object identity is invalid."));
        }
        let bytes = match std::fs::read(self.path_of(hash)) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::NotFound(format!("object {hash}")))
            }
            Err(error) => return Err(error.into()),
        };
        if hash_of(&bytes) != hash {
            return Err(Error::invalid(
                "Checkpoint object failed content verification.",
            ));
        }
        Ok(bytes)
    }

    pub fn has(&self, hash: &str) -> bool {
        is_hash(hash) && self.path_of(hash).is_file()
    }

    /// `ab/cdef…` — one level of fan-out keeps directories small enough for any
    /// filesystem without making the tree deep.
    fn path_of(&self, hash: &str) -> PathBuf {
        let (head, tail) = hash.split_at(2.min(hash.len()));
        self.root.join(head).join(tail)
    }
}

/// A name the store could have given: hexadecimal, two to sixty-four digits.
fn is_hash(hash: &str) -> bool {
    (2..=64).contains(&hash.len()) && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// What a sweep removed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Swept {
    pub blobs: usize,
    pub bytes: u64,
}

impl Objects {
    /// Delete every stored blob that `keep` does not name.
    ///
    /// Without this the store grows for the life of the machine: every version of
    /// every file the agent ever touched, kept forever. The caller is responsible
    /// for `keep` covering **all** sessions of the project — a partial set here
    /// deletes somebody's undo history.
    pub fn sweep(&self, keep: &std::collections::HashSet<String>) -> Result<Swept> {
        let mut swept = Swept::default();
        let dirs = match std::fs::read_dir(self.root()) {
            Ok(d) => d,
            // Nothing stored yet is not a failure to clean.
            Err(_) => return Ok(swept),
        };
        for shard in dirs.flatten() {
            if !shard.path().is_dir() {
                continue;
            }
            let head = shard.file_name().to_string_lossy().to_string();
            let files = match std::fs::read_dir(shard.path()) {
                Ok(f) => f,
                Err(_) => continue,
            };
            for entry in files.flatten() {
                let tail = entry.file_name().to_string_lossy().to_string();
                // Skip half-written blobs from a crashed run; they carry no hash
                // anyone could reference.
                if tail.starts_with('.') {
                    let _ = std::fs::remove_file(entry.path());
                    continue;
                }
                let hash = format!("{head}{tail}");
                if keep.contains(&hash) {
                    continue;
                }
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                if std::fs::remove_file(entry.path()).is_ok() {
                    swept.blobs += 1;
                    swept.bytes += size;
                }
            }
            let _ = std::fs::remove_dir(shard.path()); // only succeeds when empty
        }
        Ok(swept)
    }
}

pub fn hash_of(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex(&h.finalize())
}

/// [`hash_of`] the file at `path`, read in pieces: an attachment of any size
/// is fingerprinted without being held in memory whole.
pub fn hash_file(path: &std::path::Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => h.update(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(hex(&h.finalize()))
}

fn hex(digest: &[u8]) -> String {
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> Objects {
        let dir = std::env::temp_dir().join("rune-test").join(format!(
            "runeobj-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Objects::new(dir)
    }

    /// A blob whose bytes no longer match its name is refused, never handed
    /// back as the file's earlier content.
    #[test]
    fn a_corrupted_blob_is_refused() {
        let o = store("corrupt");
        let hash = o.put(b"written once").unwrap();
        std::fs::write(o.path_of(&hash), b"corrupted").unwrap();
        assert!(o.get(&hash).is_err());
    }

    #[test]
    fn put_then_get_round_trips() {
        let o = store("roundtrip");
        let h = o.put(b"hello world").unwrap();
        assert_eq!(o.get(&h).unwrap(), b"hello world");
        assert!(o.has(&h));
    }

    #[test]
    fn identical_content_is_stored_once() {
        let o = store("dedupe");
        let a = o.put(b"same").unwrap();
        let b = o.put(b"same").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn different_content_gets_different_names() {
        let o = store("distinct");
        assert_ne!(o.put(b"a").unwrap(), o.put(b"b").unwrap());
    }

    #[test]
    fn a_missing_object_is_not_found_not_a_panic() {
        let o = store("missing");
        assert!(!o.has("deadbeef"));
        assert!(matches!(o.get("deadbeef"), Err(Error::NotFound(_))));
    }

    #[test]
    fn empty_content_is_storable() {
        let o = store("empty");
        let h = o.put(b"").unwrap();
        assert_eq!(o.get(&h).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn hash_matches_the_known_sha256_of_abc() {
        assert_eq!(
            hash_of(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn sweep_removes_only_what_nothing_references() {
        use std::collections::HashSet;
        let o = store("sweep");
        let keep_hash = o.put(b"still referenced").unwrap();
        let drop_hash = o.put(b"orphaned").unwrap();

        let keep: HashSet<String> = [keep_hash.clone()].into_iter().collect();
        let swept = o.sweep(&keep).unwrap();

        assert_eq!(swept.blobs, 1);
        assert!(swept.bytes > 0);
        assert!(o.has(&keep_hash));
        assert!(!o.has(&drop_hash));
    }

    #[test]
    fn sweeping_an_empty_store_is_not_an_error() {
        use std::collections::HashSet;
        let o = store("sweepempty");
        assert_eq!(o.sweep(&HashSet::new()).unwrap(), Swept::default());
    }

    #[test]
    fn sweep_clears_half_written_leftovers() {
        use std::collections::HashSet;
        let o = store("sweeptmp");
        let h = o.put(b"content").unwrap();
        let shard = o.root().join(&h[..2]);
        std::fs::write(shard.join(".crashed.tmp99"), b"junk").unwrap();

        o.sweep(&[h.clone()].into_iter().collect::<HashSet<String>>())
            .unwrap();
        assert!(!shard.join(".crashed.tmp99").exists());
        assert!(o.has(&h));
    }

    #[test]
    fn no_temporary_files_are_left_behind() {
        let o = store("clean");
        o.put(b"content").unwrap();
        let mut leftovers = 0;
        for entry in walkdir(&o.root) {
            if entry.file_name().to_string_lossy().contains(".tmp") {
                leftovers += 1;
            }
        }
        assert_eq!(leftovers, 0);
    }

    fn walkdir(root: &std::path::Path) -> Vec<std::fs::DirEntry> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.flatten() {
                    if e.path().is_dir() {
                        stack.push(e.path());
                    } else {
                        out.push(e);
                    }
                }
            }
        }
        out
    }
}
