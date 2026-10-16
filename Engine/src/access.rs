//! Who may speak to a session host.
//!
//! A session host listens on a Unix socket under the home's `run/`. That
//! socket carries every command a window may send — switch the mode to Full,
//! answer a permission card, change a setting — and the session's whole event
//! stream: the conversation, every tool's output. Two things that guard the
//! rest of Rune cannot guard it:
//!
//! - **The kernel sandbox.** Landlock mediates opening files, not `connect()`
//!   on a pathname socket. Verified, not assumed (2026-10-07, ABI 6): a
//!   confined command that was refused a listing of `run/` still connected to
//!   a socket in it and delivered `session.mode` → `full`. A command the agent
//!   runs is a process of the same user, so nothing else stood in its way.
//! - **File permissions.** A socket's mode comes from the umask of whoever
//!   bound it, which Rune does not choose (owner rule 8).
//!
//! So a connection is admitted on two facts, both settled before it hears or
//! says anything:
//!
//! 1. The peer is a process of this user — the kernel's word
//!    ([`crate::platform::LocalStream::peer_is_same_user`]), whatever the
//!    socket's mode is.
//! 2. Its first line carries the host's key: [`KEY_BYTES`] random bytes the host
//!    issues into a 0600 file beside its socket each time it starts
//!    ([`HostKey::issue`]). A confined command cannot read `run/` — the
//!    sandbox never grants Rune's home on its own — so it cannot learn the key.
//!    A process that can read it could already rewrite `rune.toml` and the
//!    database: the socket grants nothing that access did not.
//!
//! The key is never derivable from anything a confined process can see: not
//! the socket's name (`/proc/net/unix` lists every bound path to anyone who
//! can read `/proc`, which builds need), not a pid, not a time.
//!
//! A host of the build before keys does not know the hello: it answers it as
//! a command it cannot read (`ok: false` with its older `protocol`), and has
//! already admitted the connection — so a window may go on talking to it
//! ([`Answer::Older`]) until that host hands over to this build when idle.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use rand::RngCore;
use serde_json::{json, Value};

use crate::error::{Error, Result};
use crate::platform::LocalStream;
use crate::proto::Event;

/// How much randomness a key carries.
pub const KEY_BYTES: usize = 32;

/// The command name of the hello. Not a [`crate::Command`]: it belongs to the
/// socket, not to the engine, and never reaches one.
pub const HELLO: &str = "session.hello";

/// The request id of the hello. Every window numbers its own requests from 1
/// upward under its pid, so 0 is never a command's.
pub const HELLO_ID: u64 = 0;

/// How long a window waits for a host to answer its hello, and a host for a
/// connection's first line. A live host answers in microseconds; this only
/// bounds one that is wedged, or a connection that never speaks.
pub const PATIENCE: std::time::Duration = std::time::Duration::from_secs(5);

/// Where the key of the host listening at `socket` lives.
pub fn key_path(socket: &Path) -> PathBuf {
    socket.with_extension("key")
}

/// The key a host issued, held by that host.
pub struct HostKey {
    hex: String,
    path: PathBuf,
}

impl HostKey {
    /// Issue a fresh key for the host about to bind `socket`, replacing any
    /// key a previous host left. Called while holding the host's ownership
    /// lock and before binding, so a window that reaches the new socket can
    /// already read its key.
    pub fn issue(socket: &Path) -> Result<HostKey> {
        let mut bytes = [0u8; KEY_BYTES];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        let hex = hex(&bytes);
        let path = key_path(socket);
        crate::settings::secrets::atomic_write(&path, hex.as_bytes())?;
        Ok(HostKey { hex, path })
    }

    /// Whether `presented` is this key. Compared in constant time: a peer
    /// learns nothing about the key from how long a wrong one took.
    pub fn admits(&self, presented: &str) -> bool {
        let (a, b) = (self.hex.as_bytes(), presented.as_bytes());
        a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
    }

    /// Remove the key file. Called by a host going away while it still holds
    /// its ownership lock — no successor can have issued its own key yet.
    pub fn withdraw(&self) {
        if read_key(&self.path).is_ok_and(|held| self.admits(&held)) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// What a connection's first line says.
pub enum Presented {
    /// A hello, with the key it carries — none when the window found no key
    /// file, which a host of this build always refuses.
    Hello(Option<String>),
    /// Anything else: a window of the build before keys, or not a window.
    Other,
}

/// Read a connection's first line.
pub fn presented(line: &str) -> Presented {
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Presented::Other;
    };
    match value["cmd"].as_str() {
        Some(HELLO) => Presented::Hello(value["key"].as_str().map(str::to_string)),
        _ => Presented::Other,
    }
}

/// The host's answers to a hello, as lines on the socket. Plain acks of the
/// protocol every build reads.
pub fn admitted_line() -> String {
    ack(true, None, json!({ "protocol": crate::proto::PROTOCOL }))
}

/// A hello whose key is not this host's — a key issued by the host before
/// this one, or a guess.
pub fn refused_line() -> String {
    ack(
        false,
        Some("This session host did not recognise the window's key."),
        json!({ "protocol": crate::proto::PROTOCOL, "auth": false }),
    )
}

/// A hello that arrived while this host is handing over to its successor.
pub fn retiring_line() -> String {
    ack(
        false,
        Some("The session host is activating an update."),
        json!({ "protocol": crate::proto::PROTOCOL, "retry": true }),
    )
}

fn ack(ok: bool, error: Option<&str>, data: Value) -> String {
    serde_json::to_string(&Event::Ack {
        id: HELLO_ID,
        ok,
        error: error.map(str::to_string),
        data: Some(data),
    })
    .expect("an ack serialises")
}

/// How a host answered a window's hello.
#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    /// The connection is admitted.
    Admitted,
    /// A host of the build before keys, which admits every connection of this
    /// user and answered the hello as a command it does not know.
    Older,
    /// The key was not the host's: it was replaced between reading and
    /// connecting. Read it again.
    Refused,
    /// The host is handing over; its successor is the one to reach.
    Retiring,
}

/// A connection the host answered, with whatever the host said before the
/// answer (a host of the build before keys streams the session's events from
/// the moment it accepts). The reader keeps the bytes it buffered past the
/// answer, so it is the one to go on reading with.
pub struct Introduction {
    pub answer: Answer,
    pub early: Vec<String>,
    pub reader: BufReader<LocalStream>,
}

/// Introduce `stream`, connected to the host listening at `socket`: check the
/// host is this user's, present the key, and read until the host answers.
pub fn introduce(stream: &LocalStream, socket: &Path) -> Result<Introduction> {
    if !stream.peer_is_same_user() {
        return Err(Error::Denied(
            "the process listening on the session socket belongs to another user".into(),
        ));
    }
    // No key file beside a socket that answers means a host of the build
    // before keys (this build issues its key before it binds): the hello goes
    // without one, which such a host answers as a command it does not know
    // and a host of this build refuses.
    let hello = match read_key(&key_path(socket)) {
        Ok(key) => json!({ "cmd": HELLO, "id": HELLO_ID, "key": key }),
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            json!({ "cmd": HELLO, "id": HELLO_ID })
        }
        Err(e) => return Err(e),
    };
    let mut writer = stream.try_clone()?;
    writer.write_all(format!("{hello}\n").as_bytes())?;
    writer.flush()?;
    stream.set_read_timeout(Some(PATIENCE))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut early = Vec::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the session host closed the connection before answering",
            )
            .into());
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        let Ok(Event::Ack {
            id: HELLO_ID,
            ok,
            data,
            ..
        }) = serde_json::from_str::<Event>(&line)
        else {
            early.push(line);
            continue;
        };
        let data = data.unwrap_or(Value::Null);
        let answer = if ok {
            Answer::Admitted
        } else if data["auth"] == false {
            Answer::Refused
        } else if data["retry"] == true {
            Answer::Retiring
        } else if data["protocol"]
            .as_u64()
            .is_some_and(|theirs| theirs < u64::from(crate::proto::PROTOCOL))
        {
            Answer::Older
        } else {
            return Err(Error::Invalid(format!(
                "the session host answered the window's hello with {line}"
            )));
        };
        stream.set_read_timeout(None)?;
        return Ok(Introduction {
            answer,
            early,
            reader,
        });
    }
}

/// The key in the file at `path`: exactly `2 × KEY_BYTES` hex digits.
fn read_key(path: &Path) -> Result<String> {
    let mut text = String::new();
    std::fs::File::open(path)
        .map_err(|e| -> Error {
            match e.kind() {
                std::io::ErrorKind::NotFound => std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "the session host has not issued its key",
                )
                .into(),
                _ => e.into(),
            }
        })?
        .take(4 * KEY_BYTES as u64)
        .read_to_string(&mut text)?;
    let key = text.trim();
    if key.len() != 2 * KEY_BYTES || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Invalid(format!(
            "the session key at {} is malformed",
            path.display()
        )));
    }
    Ok(key.to_string())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-access-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A key is private to its user, new on every issue, and admits only itself.
    #[test]
    fn a_key_is_private_fresh_and_admits_only_itself() {
        let dir = home("issue");
        let socket = dir.join("session_1.sock");
        let first = HostKey::issue(&socket).unwrap();
        assert!(
            crate::platform::is_private(&key_path(&socket)).unwrap(),
            "the key file is the user's alone"
        );
        let held = read_key(&key_path(&socket)).unwrap();
        assert!(first.admits(&held));
        assert!(!first.admits(&held[1..]), "a shorter key");
        let mut wrong = held.clone().into_bytes();
        wrong[10] = if wrong[10] == b'a' { b'b' } else { b'a' };
        assert!(!first.admits(std::str::from_utf8(&wrong).unwrap()));
        let second = HostKey::issue(&socket).unwrap();
        assert!(!first.admits(&read_key(&key_path(&socket)).unwrap()));
        assert!(second.admits(&read_key(&key_path(&socket)).unwrap()));
        // The host before does not take its successor's key away.
        first.withdraw();
        assert!(key_path(&socket).exists());
        second.withdraw();
        assert!(!key_path(&socket).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_hello_is_told_from_everything_else() {
        assert!(matches!(
            presented(r#"{"cmd":"session.hello","id":0,"key":"ab"}"#),
            Presented::Hello(Some(k)) if k == "ab"
        ));
        assert!(matches!(
            presented(r#"{"cmd":"session.hello","id":0}"#),
            Presented::Hello(None)
        ));
        for other in [
            r#"{"cmd":"session.resume","id":5,"session":1}"#,
            "not json",
            "",
        ] {
            assert!(matches!(presented(other), Presented::Other), "{other}");
        }
    }

    /// Every answer a host can give reads back as the answer it is, and what a
    /// host of the build before keys says to an unknown command reads as
    /// `Older` — never as admitted.
    #[test]
    fn a_hosts_answer_is_read_for_what_it_says() {
        let dir = home("answers");
        let socket = dir.join("session_2.sock");
        let _key = HostKey::issue(&socket).unwrap();
        let older = serde_json::to_string(&Event::Ack {
            id: HELLO_ID,
            ok: false,
            error: Some("This session host is a different Rune build".into()),
            data: Some(json!({ "protocol": crate::proto::PROTOCOL - 1 })),
        })
        .unwrap();
        let early = r#"{"t":"log","session":2}"#.to_string();
        for (lines, expected) in [
            (vec![admitted_line()], Answer::Admitted),
            (vec![refused_line()], Answer::Refused),
            (vec![retiring_line()], Answer::Retiring),
            (vec![early.clone(), older], Answer::Older),
        ] {
            let (ours, theirs) = LocalStream::pair().unwrap();
            let mut host = theirs.try_clone().unwrap();
            let answer = std::thread::spawn(move || {
                let mut hello = String::new();
                BufReader::new(theirs).read_line(&mut hello).unwrap();
                for line in lines {
                    host.write_all(format!("{line}\n").as_bytes()).unwrap();
                }
                hello
            });
            let intro = introduce(&ours, &socket).unwrap();
            let hello = answer.join().unwrap();
            assert!(matches!(presented(hello.trim()), Presented::Hello(Some(_))));
            if expected == Answer::Older {
                assert_eq!(intro.early, vec![early.clone()], "what came first is kept");
            }
            assert_eq!(intro.answer, expected);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without a key file the hello carries none: a host of the build before
    /// keys answers it as an unknown command and is talked to; a host of this
    /// build refuses it. Found with real processes: a window that would not
    /// say hello without a key could not reach a host of the build before.
    #[test]
    fn without_a_key_file_the_hello_carries_none() {
        let dir = home("nokey");
        let socket = dir.join("session_3.sock");
        let older = serde_json::to_string(&Event::Ack {
            id: HELLO_ID,
            ok: false,
            error: Some("This session host is a different Rune build".into()),
            data: Some(json!({ "protocol": crate::proto::PROTOCOL - 1 })),
        })
        .unwrap();
        for (answer, expected) in [(older, Answer::Older), (refused_line(), Answer::Refused)] {
            let (ours, theirs) = LocalStream::pair().unwrap();
            let mut host = theirs.try_clone().unwrap();
            let heard = std::thread::spawn(move || {
                let mut hello = String::new();
                BufReader::new(theirs).read_line(&mut hello).unwrap();
                host.write_all(format!("{answer}\n").as_bytes()).unwrap();
                hello
            });
            assert_eq!(introduce(&ours, &socket).unwrap().answer, expected);
            assert!(matches!(
                presented(heard.join().unwrap().trim()),
                Presented::Hello(None)
            ));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
