//! API keys: where they live, and the rules that keep them there.
//!
//! `~/.rune/secrets.json`, mode `0600`, one flat object of `name → key`, where
//! a key may also be a **list** of keys:
//!
//! ```json
//! { "mimo": "sk-one", "openrouter": ["sk-or-a", "sk-or-b"], "serper": ["a", "b"] }
//! ```
//!
//! # The ring
//!
//! Position 0 is the key in use. When one is rate-limited, revoked or out of
//! credit it is **demoted to the back and everything behind it moves up one** —
//! `1,2,3,4` becomes `2,3,4,1` — and the new order is written back, so the next
//! launch starts on a key that works instead of rediscovering the limit.
//!
//! This belongs here, to *secrets*, and not to the model provider, because it is
//! not a fact about models: a web-search key runs out exactly the same way. One
//! ring, one [`Secrets::demote`], and every caller that asks for a key by name
//! gets rotation for free without knowing the word.
//!
//! # Four decisions, each with a reason
//!
//! 1. **Separate from `rune.toml`.** That file refuses to start the engine on
//!    an unknown key, which is right for settings the user believes are in force
//!    and wrong for credentials — a stray entry there must not be able to stop
//!    Rune from running.
//! 2. **Never in the project, never in the binary.** The source is public; a key
//!    compiled in would be a key published. And `~/.rune` is not a write root in
//!    the sandbox, so a command the agent runs cannot read this file at all.
//! 3. **`0600`, enforced on every write and checked on read.** A key readable by
//!    the machine's other users is a leaked key, whoever wrote it.
//! 4. **Registered for redaction on load.** From the moment a key is known, it
//!    can no longer appear in any error message — see [`crate::net::redact`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::net::redact;

/// The encrypted-at-rest credential store: binary blobs, never plaintext JSON. The
/// master key is in master.key (0600), not beside this file.
pub const VAULT_FILE: &str = "secrets.vault";

/// Serialises every read-modify-write of the store across processes.
const LOCK_FILE: &str = "secrets.lock";

/// How a name's keys are written on disk: one, or several.
///
/// Untagged so the file a user already has keeps working untouched, and so a
/// single key is written back as a plain string rather than a one-element array
/// nobody asked for.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum SecretItem {
    Named { name: String, key: String },
    Plain(String),
}

impl SecretItem {
    pub fn value(&self) -> &str {
        match self {
            Self::Named { key, .. } => key,
            Self::Plain(key) => key,
        }
    }

    pub fn key_name(&self) -> Option<&str> {
        match self {
            Self::Named { name, .. } => Some(name),
            Self::Plain(_) => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum Entry {
    One(SecretItem),
    Many(Vec<SecretItem>),
}

/// One credential and what is currently known about its health.
///
/// Health is runtime-only and never written down: a rate limit that expired
/// while Rune was closed has expired, and a file that remembered it would send
/// the user to their second key for no reason.
#[derive(Debug, Clone)]
struct Key {
    name: Option<String>,
    value: String,
    /// When this key may be used again. `None` means now.
    resting_until: Option<Instant>,
    /// Refused as invalid. A revoked key does not heal by waiting, so it is not
    /// tried again until the user edits the file or restarts.
    dead: bool,
}

impl Key {
    fn ready(&self, now: Instant) -> bool {
        !self.dead && self.resting_until.is_none_or(|t| t <= now)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KeyInfo {
    pub position: usize,
    pub name: String,
    pub preview: String,
    pub ready: bool,
}

/// Generates a safe preview showing the first 8 characters and masking the rest.
pub fn mask_preview(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 6 {
        "•".repeat(chars.len())
    } else {
        let show = chars.len().min(8);
        let head: String = chars[..show].iter().collect();
        format!("{head}••••••••")
    }
}

/// What the credential store on disk is, as last read — the one source every
/// surface tells a key's state from (owner, 2026-10-16: what Rune shows and
/// what it uses is what is on disk; a key whose file is gone is never shown,
/// or used, as if it were there).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Store {
    /// No store file in this home: nothing saved yet, or it was removed.
    #[default]
    Absent,
    /// It opened; its keys are the ones held.
    Ready,
    /// It is there and cannot be opened. Never repaired or overwritten by
    /// Rune: restoring the cause (a master key) can bring every key back.
    Unavailable(Unavailable),
}

/// Why a credential store that is there cannot be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unavailable {
    /// The store is there; its master key (`master.key`) is not.
    KeyMissing,
    /// `master.key` is not a key: the wrong size.
    KeyDamaged,
    /// The store does not open with this master key: it came from another
    /// machine or backup than the key, or it was damaged.
    Mismatch,
    /// Shorter than any store Rune writes: cut off.
    Truncated,
    /// It opened, and what it holds does not read.
    Content,
    /// The store or its key could not be read at all.
    Unreadable(std::io::ErrorKind),
}

impl Unavailable {
    /// A stable word for the frontend and tests.
    pub fn code(&self) -> &'static str {
        match self {
            Self::KeyMissing => "key_missing",
            Self::KeyDamaged => "key_damaged",
            Self::Mismatch => "mismatch",
            Self::Truncated => "truncated",
            Self::Content => "content",
            Self::Unreadable(_) => "unreadable",
        }
    }

    /// What the person is told: what is wrong and what brings the keys back.
    pub fn message(&self) -> String {
        match self {
            Self::KeyMissing => "Rune's credential store is there but its master.key is missing — restore master.key from a backup, or start a new credential store".into(),
            Self::KeyDamaged => "Rune's master.key is damaged — restore it from a backup, or start a new credential store".into(),
            Self::Mismatch => "Rune's credential store does not open with this master.key — the two came from different machines or backups, or the store is damaged; restore the matching pair, or start a new credential store".into(),
            Self::Truncated => "Rune's credential store is cut off — restore secrets.vault from a backup, or start a new credential store".into(),
            Self::Content => "Rune's credential store opened but what it holds does not read — restore secrets.vault from a backup, or start a new credential store".into(),
            Self::Unreadable(kind) => format!("Rune's credential store could not be read ({kind}) — check the permissions of its home folder"),
        }
    }
}

/// What one key a connection names is, as every surface tells it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyState {
    /// Held: how many keys, and how many are usable now.
    Present { keys: usize, ready: usize },
    /// The store holds no key under this name.
    NotInStore,
    /// There is no credential store file at all.
    StoreAbsent,
    /// The store cannot be opened.
    Unavailable(Unavailable),
}

impl KeyState {
    /// `present`, `missing` or `unavailable`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Present { .. } => "present",
            Self::NotInStore | Self::StoreAbsent => "missing",
            Self::Unavailable(_) => "unavailable",
        }
    }

    /// One line about the key named `name`, for every surface.
    pub fn note(&self, name: &str) -> String {
        match self {
            Self::Present { keys, ready } => match (keys, keys - ready) {
                (1, 0) => "1 key".into(),
                (n, 0) => format!("{n} keys"),
                (n, resting) => format!("{n} keys · {resting} resting"),
            },
            Self::NotInStore => format!("Key missing: Rune's credential store holds no key for `{name}`"),
            Self::StoreAbsent => format!("Key missing: Rune's credential store (secrets.vault) is not in its home, so `{name}` has no key"),
            Self::Unavailable(why) => format!("Key unavailable: {}", why.message()),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Secrets {
    keys: BTreeMap<String, Vec<Key>>,
    /// The engine home the store lives in. Empty only for the in-memory test
    /// double, which has nowhere to be saved.
    home: PathBuf,
    /// At-rest encryption: the master key lives in master.key (0600). `None`
    /// until a key exists — one is made only when the first key is saved
    /// into a home with no store.
    vault: Option<super::vault::Vault>,
    /// What the store on disk was when this was read.
    store: Store,
}

impl Secrets {
    /// Load from `home/secrets.vault`. A missing store is no keys; a store
    /// that cannot be opened is [`Store::Unavailable`] with its cause — never
    /// no keys, and never a reason for Rune not to start (owner, 2026-10-16:
    /// a damaged store used to refuse every start, Settings included). The
    /// store is left exactly as it is: nothing here writes.
    pub fn load(home: &Path) -> Result<Secrets> {
        let unavailable = |why| Secrets {
            home: home.to_path_buf(),
            store: Store::Unavailable(why),
            ..Default::default()
        };
        let vault = match super::vault::Vault::open_existing(home) {
            Ok(vault) => vault,
            Err(Error::Io(e)) => return Ok(unavailable(Unavailable::Unreadable(e.kind()))),
            Err(_) => return Ok(unavailable(Unavailable::KeyDamaged)),
        };
        let blob = match std::fs::read(home.join(VAULT_FILE)) {
            Ok(blob) => blob,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Secrets {
                    keys: BTreeMap::new(),
                    home: home.to_path_buf(),
                    vault,
                    store: Store::Absent,
                });
            }
            Err(e) => return Ok(unavailable(Unavailable::Unreadable(e.kind()))),
        };
        let Some(vault) = vault else {
            return Ok(unavailable(Unavailable::KeyMissing));
        };
        let entries = match decode(&vault, &blob) {
            Ok(entries) => entries,
            Err(why) => return Ok(unavailable(why)),
        };
        let mut keys = BTreeMap::new();
        for (name, entry) in entries {
            let items = match entry {
                Entry::One(v) => vec![v],
                Entry::Many(v) => v,
            };
            // Every key in the ring, not only the active one: a key sitting at
            // position 3 is still a key that must never reach an error string.
            for item in &items {
                redact::register(item.value());
            }
            keys.insert(name, items.into_iter().map(new_key).collect());
        }
        let s = Secrets {
            keys,
            home: home.to_path_buf(),
            vault: Some(vault),
            store: Store::Ready,
        };
        s.tighten()?;
        Ok(s)
    }

    /// What the store on disk was when this was read.
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// What the key named `name` is — the one answer every surface gives.
    pub fn key_state(&self, name: &str) -> KeyState {
        match &self.store {
            Store::Unavailable(why) => KeyState::Unavailable(why.clone()),
            _ if self.keys.contains_key(name) => {
                let (keys, ready) = self.health(name);
                KeyState::Present { keys, ready }
            }
            Store::Absent => KeyState::StoreAbsent,
            Store::Ready => KeyState::NotInStore,
        }
    }

    /// The key to use for `name`, or the one sentence that says why there is
    /// none — the same sentence Settings shows.
    pub fn require(&self, name: &str) -> Result<&str> {
        self.get(name)
            .ok_or_else(|| Error::invalid(self.key_state(name).note(name)))
    }

    /// Refuse a change while the store cannot be opened: writing would
    /// replace keys a restored master key could still bring back.
    fn writable(&self) -> Result<()> {
        match &self.store {
            Store::Unavailable(why) => Err(Error::invalid(format!(
                "{}. Nothing was changed.",
                why.message()
            ))),
            _ => Ok(()),
        }
    }

    /// Save `next` as the whole store; the store is this process's from now.
    fn write(&mut self, next: &BTreeMap<String, Vec<SecretItem>>) -> Result<()> {
        self.writable()?;
        let current = self.snapshot();
        self.save(&current, next)?;
        self.store = Store::Ready;
        Ok(())
    }

    /// Whether this store holds no key at all.
    ///
    /// The one question [`crate::settings::Settings::reload_secrets`] asks of
    /// a store it did not build: a re-read that came back empty means the read
    /// failed, not that every key was deleted.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The key to use for `name` right now — the front of the ring.
    ///
    /// A resting key is skipped while any other is ready. When *every* key is
    /// resting the soonest one is handed back anyway, because "no key
    /// configured" would be a lie and the caller bounds its own attempts.
    pub fn get(&self, name: &str) -> Option<&str> {
        let ring = self.keys.get(name)?;
        let now = Instant::now();
        ring.iter()
            .find(|k| k.ready(now))
            .or_else(|| {
                ring.iter()
                    .filter(|k| !k.dead)
                    .min_by_key(|k| k.resting_until)
            })
            .map(|k| k.value.as_str())
            .filter(|v| !v.is_empty())
    }

    /// How many keys `name` holds, and how many of those are usable right now.
    /// The settings panel reports this; nothing else needs the values.
    pub fn health(&self, name: &str) -> (usize, usize) {
        let now = Instant::now();
        match self.keys.get(name) {
            Some(ring) => (ring.len(), ring.iter().filter(|k| k.ready(now)).count()),
            None => (0, 0),
        }
    }

    /// Move the key that just failed to the back of its ring, and rest it.
    ///
    /// `1,2,3,4` with 1 failing becomes `2,3,4,1`, exactly. Nothing happens when
    /// the ring holds one key — there is nowhere to move it to, and rewriting
    /// the file to say so would be noise.
    /// Persisting the new order is **not** part of the rotation, and a failure
    /// to write must not be reported as one. By the time the file is touched the
    /// ring has already moved in memory, which is the whole of what the caller
    /// asked for; the file only saves the *next* launch from starting on a key it
    /// would immediately have to rotate away from again. Returning the write's
    /// error here made a read-only home turn a rate limit the ring had already
    /// recovered from into a hard failure — the user's second key sat unused
    /// while the turn died with a disk error. Health is runtime-only in any case,
    /// so nothing is lost but a head start.
    pub fn demote(&mut self, name: &str, value: &str, rest: Duration, dead: bool) {
        let original = self.snapshot();
        let Some(ring) = self.keys.get_mut(name) else {
            return;
        };
        let Some(at) = ring.iter().position(|k| k.value == value) else {
            return;
        };
        ring[at].dead |= dead;
        ring[at].resting_until = Some(Instant::now() + rest);
        if ring.len() < 2 {
            return;
        }
        let failed = ring.remove(at);
        ring.push(failed);
        let _ = self.save(&original, &self.snapshot());
    }

    /// Add an unnamed key to the back of `name`'s ring, keeping the ones already there.
    pub fn append(&mut self, name: &str, key: &str) -> Result<()> {
        self.append_named(name, None, key)
    }

    /// Add a key with an optional humanist name to the back of `name`'s ring.
    pub fn append_named(&mut self, name: &str, key_name: Option<&str>, key: &str) -> Result<()> {
        let key = key.trim();
        if key.is_empty() {
            return Err(Error::invalid("an API key cannot be empty"));
        }
        let mut next = self.snapshot();
        let ring = next.entry(name.to_string()).or_default();
        if ring.iter().any(|item| item.value() == key) {
            return Err(Error::invalid(
                "that key is already in this provider's ring",
            ));
        }
        let clean_name = key_name
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from);
        let item = match &clean_name {
            Some(n) => SecretItem::Named {
                name: n.clone(),
                key: key.to_string(),
            },
            None => SecretItem::Plain(key.to_string()),
        };
        ring.push(item);
        self.write(&next)?;
        redact::register(key);
        self.keys.entry(name.to_string()).or_default().push(Key {
            name: clean_name,
            value: key.to_string(),
            resting_until: None,
            dead: false,
        });
        Ok(())
    }

    /// Drop one key by its position in the ring, counting from 1 as the panel
    /// shows it. `Ok(false)` when there is no such position.
    pub fn remove_at(&mut self, name: &str, position: usize) -> Result<bool> {
        self.writable()?;
        let Some(ring) = self.keys.get(name) else {
            return Ok(false);
        };
        if position == 0 || position > ring.len() {
            return Ok(false);
        }
        let mut next = self.snapshot();
        let values = next.get_mut(name).expect("the ring was just read");
        values.remove(position - 1);
        if values.is_empty() {
            next.remove(name);
        }
        self.write(&next)?;
        let ring = self.keys.get_mut(name).expect("the ring was just read");
        ring.remove(position - 1);
        if ring.is_empty() {
            self.keys.remove(name);
        }
        Ok(true)
    }

    pub fn names(&self) -> Vec<&str> {
        self.keys.keys().map(|s| s.as_str()).collect()
    }

    /// The ring's values, in order, for the one caller that must see them all:
    /// the panel that masks them. Never logged, never sent anywhere.
    pub fn ring(&self, name: &str) -> Vec<&str> {
        match self.keys.get(name) {
            Some(ring) => ring.iter().map(|k| k.value.as_str()).collect(),
            None => Vec::new(),
        }
    }

    /// Return all secret items with their names and raw values for internal backup/restore.
    pub fn raw_items(&self, name: &str) -> Vec<SecretItem> {
        match self.keys.get(name) {
            Some(ring) => ring
                .iter()
                .map(|k| match &k.name {
                    Some(n) => SecretItem::Named {
                        name: n.clone(),
                        key: k.value.clone(),
                    },
                    None => SecretItem::Plain(k.value.clone()),
                })
                .collect(),
            None => Vec::new(),
        }
    }

    /// Key metadata for UI presentation: includes humanist names, safe previews, and health.
    pub fn keys_metadata(&self, name: &str) -> Vec<KeyInfo> {
        let now = Instant::now();
        match self.keys.get(name) {
            Some(ring) => ring
                .iter()
                .enumerate()
                .map(|(i, k)| {
                    let name = k
                        .name
                        .as_deref()
                        .filter(|s| !s.trim().is_empty())
                        .map(String::from)
                        .unwrap_or_else(|| format!("Key {}", i + 1));
                    KeyInfo {
                        position: i + 1,
                        name,
                        preview: mask_preview(&k.value),
                        ready: k.ready(now),
                    }
                })
                .collect(),
            None => Vec::new(),
        }
    }

    fn snapshot(&self) -> BTreeMap<String, Vec<SecretItem>> {
        self.keys
            .iter()
            .map(|(n, ring)| {
                (
                    n.clone(),
                    ring.iter()
                        .map(|k| match &k.name {
                            Some(name) => SecretItem::Named {
                                name: name.clone(),
                                key: k.value.clone(),
                            },
                            None => SecretItem::Plain(k.value.clone()),
                        })
                        .collect(),
                )
            })
            .collect()
    }

    /// Store a key and write the file back at `0600`.
    ///
    /// The file is written **before** memory changes. A write that fails — a
    /// full disk, a read-only home — must leave the process believing exactly
    /// what is on disk: a key that lives only in memory works until the next
    /// launch and then vanishes, which is the hardest kind of bug to be told
    /// about.
    pub fn set(&mut self, name: &str, key: &str) -> Result<()> {
        let key = key.trim();
        if key.is_empty() {
            return Err(Error::invalid("an API key cannot be empty"));
        }
        self.restore(name, vec![SecretItem::Plain(key.to_string())])
    }

    /// Replace `name`'s whole ring, in order. An empty list removes the name.
    pub fn restore(&mut self, name: &str, values: Vec<SecretItem>) -> Result<()> {
        let mut next = self.snapshot();
        if values.is_empty() {
            next.remove(name);
        } else {
            next.insert(name.to_string(), values.clone());
        }
        self.write(&next)?;
        for v in &values {
            redact::register(v.value());
        }
        match values.is_empty() {
            true => {
                self.keys.remove(name);
            }
            false => {
                self.keys
                    .insert(name.to_string(), values.into_iter().map(new_key).collect());
            }
        }
        Ok(())
    }

    /// Put a key in memory without writing a file. Only for tests: the real
    /// path is [`Secrets::set`], which persists at `0600`.
    #[cfg(test)]
    pub fn keys_for_test(&mut self, name: &str, key: &str) {
        self.keys.insert(
            name.to_string(),
            vec![new_key(SecretItem::Plain(key.to_string()))],
        );
    }

    /// Remove every entry whose name starts with `prefix`, in one save, and
    /// say how many went.
    pub fn remove_prefixed(&mut self, prefix: &str) -> Result<usize> {
        self.writable()?;
        let names: Vec<String> = self
            .keys
            .keys()
            .filter(|name| name.starts_with(prefix))
            .cloned()
            .collect();
        if names.is_empty() {
            return Ok(0);
        }
        let mut next = self.snapshot();
        for name in &names {
            next.remove(name);
        }
        self.write(&next)?;
        for name in &names {
            self.keys.remove(name);
        }
        Ok(names.len())
    }

    pub fn remove(&mut self, name: &str) -> Result<bool> {
        self.writable()?;
        if !self.keys.contains_key(name) {
            return Ok(false);
        }
        let mut next = self.snapshot();
        next.remove(name);
        self.write(&next)?;
        self.keys.remove(name);
        Ok(true)
    }

    fn save(
        &self,
        original: &BTreeMap<String, Vec<SecretItem>>,
        keys: &BTreeMap<String, Vec<SecretItem>>,
    ) -> Result<()> {
        if self.home.as_os_str().is_empty() {
            return Err(Error::invalid("this configuration has nowhere to be saved"));
        }
        std::fs::create_dir_all(&self.home)?;
        let store = self.home.join(VAULT_FILE);
        let _lock = crate::lock::exclusive(&self.home.join(LOCK_FILE), crate::lock::Wait::Yes)?;
        let on_disk = match std::fs::read(&store) {
            Ok(blob) => Some(blob),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        // Only ever the sealed form. A master key is made only for a home
        // with no store: one beside a store that lost its key could never
        // open it, and would hide what is wrong.
        let vault = match (&self.vault, &on_disk) {
            (Some(vault), _) => vault.clone(),
            (None, None) => super::vault::Vault::open_or_create(&self.home)?,
            (None, Some(_)) => {
                super::vault::Vault::open_existing(&self.home)?.ok_or_else(|| {
                    Error::invalid(format!(
                        "{}. Nothing was changed.",
                        Unavailable::KeyMissing.message()
                    ))
                })?
            }
        };
        // The store as it is now, for the external-edit check below. One
        // that no longer opens is never written over.
        let mut latest: BTreeMap<String, Vec<SecretItem>> = match on_disk {
            Some(blob) => decode(&vault, &blob)
                .map_err(|why| Error::invalid(format!("{}. Nothing was changed.", why.message())))?
                .into_iter()
                .map(|(name, entry)| {
                    (
                        name,
                        match entry {
                            Entry::One(value) => vec![value],
                            Entry::Many(values) => values,
                        },
                    )
                })
                .collect(),
            None => BTreeMap::new(),
        };
        let changed: std::collections::BTreeSet<_> = original
            .keys()
            .chain(keys.keys())
            .filter(|name| original.get(*name) != keys.get(*name))
            .cloned()
            .collect();
        for name in changed {
            let mut previous = original.get(&name).cloned().unwrap_or_default();
            previous.sort_by(|a, b| a.value().cmp(b.value()));
            let mut current = latest.get(&name).cloned().unwrap_or_default();
            current.sort_by(|a, b| a.value().cmp(b.value()));
            if previous != current {
                return Err(Error::invalid(
                    "Credential changed externally; reload before updating it",
                ));
            }
            match keys.get(&name) {
                Some(values) => {
                    latest.insert(name, values.clone());
                }
                None => {
                    latest.remove(&name);
                }
            }
        }
        // A lone key is written as a plain string, so a file that never used a
        // ring is never rewritten into a shape its owner did not choose.
        let on_disk: BTreeMap<&str, Entry> = latest
            .iter()
            .map(|(n, ring)| {
                let entry = match ring.as_slice() {
                    [one] => Entry::One(one.clone()),
                    many => Entry::Many(many.to_vec()),
                };
                (n.as_str(), entry)
            })
            .collect();
        let text = serde_json::to_string_pretty(&on_disk)?;
        // The encrypted blob, never the plaintext. The master key stays in
        // master.key (0600); only this sealed form touches disk.
        atomic_write(&store, &vault.seal(text.as_bytes())?)?;
        Ok(())
    }

    /// Repair a file that is readable by anyone else, and say nothing when it is
    /// already right. A user who edited it by hand should not have to know this
    /// rule to be protected by it.
    fn tighten(&self) -> Result<()> {
        match crate::platform::restrict_to_owner(&self.home.join(VAULT_FILE)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => Ok(other?),
        }
    }
}

fn new_key(item: SecretItem) -> Key {
    match item {
        SecretItem::Named { name, key } => Key {
            name: Some(name),
            value: key,
            resting_until: None,
            dead: false,
        },
        SecretItem::Plain(value) => Key {
            name: None,
            value,
            resting_until: None,
            dead: false,
        },
    }
}

/// Set the credential store and its master key aside — together, so the pair
/// can still be put back — into `backups/Credentials-<time>/`, and start with
/// no keys. Only ever the person's explicit choice, for a store that cannot be
/// brought back; Rune never does it by itself. Where the pair went.
pub fn start_new_store(home: &Path) -> Result<PathBuf> {
    let _lock = crate::lock::exclusive(&home.join(LOCK_FILE), crate::lock::Wait::Yes)?;
    let aside = home
        .join("backups")
        .join(format!("Credentials-{}", crate::store::now_ms()));
    std::fs::create_dir_all(&aside)?;
    crate::platform::restrict_to_owner(&aside)?;
    for name in [VAULT_FILE, "master.key"] {
        match std::fs::rename(home.join(name), aside.join(name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    Ok(aside)
}

/// What a sealed store holds, or why it cannot be read.
fn decode(
    vault: &super::vault::Vault,
    blob: &[u8],
) -> std::result::Result<BTreeMap<String, Entry>, Unavailable> {
    if blob.len() < super::vault::SEALED_MIN {
        return Err(Unavailable::Truncated);
    }
    let plain = vault.open_sealed(blob).map_err(|_| Unavailable::Mismatch)?;
    let text = String::from_utf8(plain).map_err(|_| Unavailable::Content)?;
    serde_json::from_str(&text).map_err(|_| Unavailable::Content)
}

/// Shared atomic private writer for credentials and user configuration.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}.{n}", std::process::id()));
    let result = write_private(&tmp, bytes).and_then(|_| Ok(crate::platform::replace(&tmp, path)?));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// A new file only this user may read, `bytes` on disk before it is used.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut f = crate::platform::open_private(path, crate::platform::Private::New)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// Write `json` as the whole credential store in `home`, sealed exactly as
/// the engine seals it — how a test sets up a store of any shape.
#[cfg(test)]
pub(crate) fn write_store(home: &Path, json: &str) {
    let vault = super::vault::Vault::open_or_create(home).unwrap();
    atomic_write(
        &home.join(VAULT_FILE),
        &vault.seal(json.as_bytes()).unwrap(),
    )
    .unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    // A tag per test: these run in parallel in one process, and a shared
    // directory means one test deletes another's file mid-run.
    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-secrets-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Credentials come only from the sealed store: a plaintext file beside it
    /// is never read, and a store that cannot be opened stops the load rather
    /// than reading as no keys at all.
    #[test]
    fn only_the_sealed_store_is_a_source_of_keys() {
        let h = home("plaintext");
        std::fs::write(h.join("secrets.json"), r#"{"p":"sk-plaintext-0000001"}"#).unwrap();
        assert_eq!(Secrets::load(&h).unwrap().get("p"), None);
        std::fs::write(h.join(VAULT_FILE), b"not a sealed store at all").unwrap();
        let s = Secrets::load(&h).unwrap();
        assert!(
            matches!(s.store(), Store::Unavailable(_)),
            "never read as no keys"
        );
        assert!(s.require("p").is_err());
    }

    #[test]
    fn a_missing_file_is_an_empty_set_not_a_failure() {
        let s = Secrets::load(&home("missing")).unwrap();
        assert!(s.names().is_empty());
    }

    #[test]
    fn round_trips_and_writes_owner_only() {
        let h = home("rt");
        let mut s = Secrets::load(&h).unwrap();
        s.set("mimo", "sk-round-trip-key-000001").unwrap();
        let again = Secrets::load(&h).unwrap();
        assert_eq!(again.get("mimo"), Some("sk-round-trip-key-000001"));
        // The encrypted store is the credential file now; it must be owner-only.
        assert!(
            crate::platform::is_private(&h.join(VAULT_FILE)).unwrap(),
            "secrets must never be group- or world-readable"
        );
    }

    #[test]
    fn a_write_that_cannot_happen_changes_nothing_in_memory_either() {
        // An in-memory configuration has nowhere to save to, which is the
        // cheapest way to reach the failure path a full disk would produce.
        let mut s = Secrets::default();
        assert!(s.set("mimo", "sk-would-be-lost-00001").is_err());
        assert_eq!(
            s.get("mimo"),
            None,
            "a key that was never written must not appear stored"
        );
    }

    #[test]
    fn a_loose_file_is_tightened_on_load() {
        let h = home("loose");
        write_store(&h, r#"{"x":"sk-loose-key-1234567"}"#);
        let store = h.join(VAULT_FILE);
        crate::platform::fixture::share_with_others(&store).unwrap();
        assert!(!crate::platform::is_private(&store).unwrap());
        Secrets::load(&h).unwrap();
        assert!(crate::platform::is_private(&store).unwrap());
    }

    #[test]
    fn a_loaded_key_can_no_longer_appear_in_an_error() {
        let h = home("scrub");
        write_store(&h, r#"{"x":"sk-must-be-scrubbed-99"}"#);
        Secrets::load(&h).unwrap();
        let msg = crate::error::Error::Provider("leaked sk-must-be-scrubbed-99".into()).to_string();
        assert!(!msg.contains("sk-must-be-scrubbed-99"));
    }

    #[test]
    fn empty_and_missing_keys_read_as_absent() {
        let h = home("empty");
        write_store(&h, r#"{"blank":""}"#);
        let s = Secrets::load(&h).unwrap();
        assert_eq!(s.get("blank"), None);
        assert_eq!(s.get("nope"), None);
    }

    /// The owner's ring, exactly: the key that failed goes to the back and
    /// everything behind it moves up one.
    #[test]
    fn a_failed_key_goes_to_the_back_and_the_rest_move_up() {
        let h = home("ring");
        write_store(&h, r#"{"p":["k1","k2","k3","k4"]}"#);
        let mut s = Secrets::load(&h).unwrap();
        assert_eq!(s.get("p"), Some("k1"));

        s.demote("p", "k1", Duration::from_secs(60), false);
        assert_eq!(s.ring("p"), vec!["k2", "k3", "k4", "k1"]);
        assert_eq!(s.get("p"), Some("k2"));

        // And it survived the file, so the next launch does not start on the
        // key that was just rate-limited.
        assert_eq!(
            Secrets::load(&h).unwrap().ring("p"),
            vec!["k2", "k3", "k4", "k1"]
        );
    }

    #[test]
    fn a_resting_key_is_skipped_and_a_refused_one_is_not_offered_at_all() {
        let h = home("rest");
        write_store(&h, r#"{"p":["k1","k2"]}"#);
        let mut s = Secrets::load(&h).unwrap();

        s.demote("p", "k1", Duration::from_secs(600), false);
        assert_eq!(
            s.get("p"),
            Some("k2"),
            "a resting key must not be handed out"
        );
        assert_eq!(s.health("p"), (2, 1));

        s.demote("p", "k2", Duration::from_secs(600), true);
        // k2 is refused outright; k1 is only resting, so it is still the better
        // answer than claiming nothing is configured.
        assert_eq!(s.get("p"), Some("k1"));
        assert_eq!(s.health("p"), (2, 0));
    }

    /// Health is not written down: a limit that expired while Rune was closed
    /// has expired.
    #[test]
    fn resting_is_forgotten_across_a_reload() {
        let h = home("forget");
        write_store(&h, r#"{"p":["k1","k2"]}"#);
        let mut s = Secrets::load(&h).unwrap();
        s.demote("p", "k1", Duration::from_secs(600), true);
        assert_eq!(Secrets::load(&h).unwrap().health("p"), (2, 2));
    }

    /// One key has nowhere to rotate to, and rewriting the file to say so would
    /// churn a `0600` file for nothing.
    #[test]
    fn a_single_key_ring_is_left_alone_and_stays_a_plain_string() {
        let h = home("single");
        let mut s = Secrets::load(&h).unwrap();
        s.set("p", "only-key-000001").unwrap();
        s.demote("p", "only-key-000001", Duration::from_secs(60), false);
        // The on-disk form is the encrypted vault; decrypt it to check the shape is
        // preserved (a lone key stays a plain string, not an array).
        let blob = std::fs::read(h.join(VAULT_FILE)).unwrap();
        let text = String::from_utf8(
            super::super::vault::Vault::open_existing(&h)
                .unwrap()
                .unwrap()
                .open_sealed(&blob)
                .unwrap(),
        )
        .unwrap();
        assert!(
            text.contains(r#""p": "only-key-000001""#),
            "a lone key must not become an array: {text}"
        );
        assert_eq!(
            s.get("p"),
            Some("only-key-000001"),
            "with nowhere else to go it is still the key"
        );
    }

    #[test]
    fn every_key_in_a_ring_is_scrubbed_from_errors_not_only_the_active_one() {
        let h = home("scrub-ring");
        write_store(&h, r#"{"p":["sk-front-9999911","sk-buried-9999922"]}"#);
        Secrets::load(&h).unwrap();
        let msg = crate::error::Error::Provider("leaked sk-buried-9999922".into()).to_string();
        assert!(
            !msg.contains("sk-buried-9999922"),
            "a key at the back of the ring leaked: {msg}"
        );
    }

    #[test]
    fn keys_can_be_appended_and_dropped_by_position() {
        let h = home("append");
        let mut s = Secrets::load(&h).unwrap();
        s.set("p", "aaa-111111111111").unwrap();
        s.append("p", "bbb-222222222222").unwrap();
        s.append("p", "ccc-333333333333").unwrap();
        assert_eq!(
            s.ring("p"),
            vec!["aaa-111111111111", "bbb-222222222222", "ccc-333333333333"]
        );
        assert!(
            s.append("p", "bbb-222222222222").is_err(),
            "the same key twice is a mistake, not a ring"
        );

        assert!(s.remove_at("p", 2).unwrap());
        assert_eq!(s.ring("p"), vec!["aaa-111111111111", "ccc-333333333333"]);
        assert!(!s.remove_at("p", 9).unwrap());
        assert_eq!(Secrets::load(&h).unwrap().ring("p").len(), 2);
    }

    /// Owner, 2026-10-16: a damaged store refused every start, and a lost
    /// master.key was silently replaced by a new one that could never open the
    /// store. Every cause is now named, nothing is written, Rune goes on.
    #[test]
    fn every_store_that_cannot_be_opened_names_its_cause_and_is_left_as_it_was() {
        let ready = || {
            let h = home(&format!("state-{}", rand::random::<u32>()));
            Secrets::load(&h)
                .unwrap()
                .set("p", "sk-state-key-000001")
                .unwrap();
            h
        };
        type Damage = (&'static str, Box<dyn Fn(&Path)>, Unavailable);
        let cases: Vec<Damage> = vec![
            (
                "key missing",
                Box::new(|h: &Path| std::fs::remove_file(h.join("master.key")).unwrap()),
                Unavailable::KeyMissing,
            ),
            (
                "key damaged",
                Box::new(|h: &Path| std::fs::write(h.join("master.key"), b"short").unwrap()),
                Unavailable::KeyDamaged,
            ),
            (
                "another key",
                Box::new(|h: &Path| std::fs::write(h.join("master.key"), [9u8; 32]).unwrap()),
                Unavailable::Mismatch,
            ),
            (
                "cut off",
                Box::new(|h: &Path| std::fs::write(h.join(VAULT_FILE), b"tiny").unwrap()),
                Unavailable::Truncated,
            ),
            (
                "unreadable",
                Box::new(|h: &Path| {
                    crate::platform::set_file_mode(&h.join(VAULT_FILE), 0o000).unwrap()
                }),
                Unavailable::Unreadable(std::io::ErrorKind::PermissionDenied),
            ),
        ];
        for (what, damage, cause) in cases {
            let h = ready();
            let read = |path: &Path| std::fs::read(path).ok();
            let undamaged = read(&h.join(VAULT_FILE));
            damage(&h);
            // An unreadable store reads as nothing here: what it holds is the
            // store as it was before its permissions changed.
            let before = (
                read(&h.join(VAULT_FILE)).or(undamaged),
                read(&h.join("master.key")),
            );
            let mut s = Secrets::load(&h).unwrap();
            assert_eq!(s.store(), &Store::Unavailable(cause.clone()), "{what}");
            assert_eq!(s.get("p"), None, "{what}");
            let state = s.key_state("p");
            assert_eq!(state.code(), "unavailable", "{what}");
            assert!(state.note("p").starts_with("Key unavailable"), "{what}");
            // No change goes through, and nothing on disk moves.
            assert!(s.set("p", "sk-other-key-00002").is_err(), "{what}");
            assert!(s.append("q", "sk-other-key-00003").is_err(), "{what}");
            assert!(s.remove("p").is_err(), "{what}");
            assert!(s.remove_at("p", 1).is_err(), "{what}");
            assert!(s.remove_prefixed("p").is_err(), "{what}");
            let _ = crate::platform::set_file_mode(&h.join(VAULT_FILE), 0o600);
            assert_eq!(
                (read(&h.join(VAULT_FILE)), read(&h.join("master.key"))),
                before,
                "{what}"
            );
        }
    }

    #[test]
    fn a_key_is_present_missing_or_its_store_absent_never_shown_otherwise() {
        let h = home("key-states");
        let s = Secrets::load(&h).unwrap();
        assert_eq!(s.store(), &Store::Absent);
        assert_eq!(s.key_state("p"), KeyState::StoreAbsent);
        assert!(s.key_state("p").note("p").contains("secrets.vault"));
        let mut s = Secrets::load(&h).unwrap();
        s.set("p", "sk-present-key-0001").unwrap();
        assert_eq!(s.store(), &Store::Ready);
        assert_eq!(s.key_state("p"), KeyState::Present { keys: 1, ready: 1 });
        assert_eq!(s.key_state("q"), KeyState::NotInStore);
        assert!(s.key_state("q").note("q").starts_with("Key missing"));
        assert!(s.require("q").is_err());
        // The store file goes: a fresh read says so — no copy is kept.
        std::fs::remove_file(h.join(VAULT_FILE)).unwrap();
        let s = Secrets::load(&h).unwrap();
        assert_eq!(s.get("p"), None);
        assert_eq!(s.key_state("p"), KeyState::StoreAbsent);
    }

    #[test]
    fn a_master_key_is_made_only_for_a_home_without_a_store() {
        let h = home("key-made");
        let s = Secrets::load(&h).unwrap();
        assert!(!h.join("master.key").exists(), "reading makes nothing");
        drop(s);
        let mut s = Secrets::load(&h).unwrap();
        s.set("p", "sk-first-key-00001").unwrap();
        assert!(
            h.join("master.key").is_file(),
            "the first key saved makes it"
        );
        std::fs::remove_file(h.join("master.key")).unwrap();
        let mut s = Secrets::load(&h).unwrap();
        assert!(s.set("q", "sk-second-key-0002").is_err());
        assert!(
            !h.join("master.key").exists(),
            "never a new key beside a store it cannot open"
        );
    }

    #[test]
    fn a_new_store_sets_the_old_pair_aside_together_and_starts_empty() {
        let h = home("new-store");
        Secrets::load(&h)
            .unwrap()
            .set("p", "sk-old-key-000001")
            .unwrap();
        let old_vault = std::fs::read(h.join(VAULT_FILE)).unwrap();
        let old_key = std::fs::read(h.join("master.key")).unwrap();
        std::fs::remove_file(h.join("master.key")).unwrap();
        std::fs::write(h.join("master.key"), [3u8; 32]).unwrap();
        assert!(matches!(
            Secrets::load(&h).unwrap().store(),
            Store::Unavailable(_)
        ));
        let aside = start_new_store(&h).unwrap();
        assert!(aside.starts_with(h.join("backups")));
        assert_eq!(std::fs::read(aside.join(VAULT_FILE)).unwrap(), old_vault);
        assert_eq!(std::fs::read(aside.join("master.key")).unwrap(), [3u8; 32]);
        assert_ne!(old_key, vec![3u8; 32]);
        let mut s = Secrets::load(&h).unwrap();
        assert_eq!(s.store(), &Store::Absent);
        s.set("p", "sk-new-key-000001").unwrap();
        assert_eq!(
            Secrets::load(&h).unwrap().get("p"),
            Some("sk-new-key-000001")
        );
    }

    #[test]
    fn a_broken_file_says_which_file_rather_than_starting_empty() {
        let h = home("broken");
        write_store(&h, "{not json");
        let s = Secrets::load(&h).unwrap();
        assert_eq!(s.store(), &Store::Unavailable(Unavailable::Content));
        let e = s.key_state("p").note("p");
        assert!(
            e.contains(VAULT_FILE),
            "the message must name the store: {e}"
        );
    }
    #[test]
    fn independent_credential_writers_preserve_other_keys_and_detect_conflicts() {
        let root = home("concurrent-writers");
        let mut first = Secrets::load(&root).unwrap();
        let mut second = Secrets::load(&root).unwrap();
        first.set("first", "fixture-first-secret").unwrap();
        second.set("second", "fixture-second-secret").unwrap();
        let saved = Secrets::load(&root).unwrap();
        assert_eq!(saved.get("first"), Some("fixture-first-secret"));
        assert_eq!(saved.get("second"), Some("fixture-second-secret"));
        let mut stale = Secrets::load(&root).unwrap();
        first.set("first", "fixture-replaced-secret").unwrap();
        assert!(stale.set("first", "fixture-stale-secret").is_err());
    }

    #[test]
    fn named_keys_and_previews_round_trip_and_preserve_plain_format() {
        let h = home("named-keys");
        let mut s = Secrets::load(&h).unwrap();
        s.append_named("provider", Some("Work Key"), "sk-or-v1-abcdef1234567890")
            .unwrap();
        s.append_named("provider", None, "sk-or-v1-second-plain-key")
            .unwrap();

        let meta = s.keys_metadata("provider");
        assert_eq!(meta.len(), 2);
        assert_eq!(meta[0].name, "Work Key");
        assert_eq!(meta[0].preview, "sk-or-v1••••••••");
        assert_eq!(meta[1].name, "Key 2");
        assert_eq!(meta[1].preview, "sk-or-v1••••••••");

        let reloaded = Secrets::load(&h).unwrap();
        let reloaded_meta = reloaded.keys_metadata("provider");
        assert_eq!(reloaded_meta.len(), 2);
        assert_eq!(reloaded_meta[0].name, "Work Key");
        assert_eq!(reloaded_meta[1].name, "Key 2");
        assert_eq!(reloaded.get("provider"), Some("sk-or-v1-abcdef1234567890"));
    }
}
