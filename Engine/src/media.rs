//! Files a person attached to a question, and what a model can be told about
//! them.
//!
//! # One canonical shape, translated once per dialect
//!
//! This is the Clode server's arrangement, ported: every attachment becomes a
//! neutral part — a MIME type and some bytes — and each provider translates that
//! into its own wire format at the last moment
//! (`Server/services/Cipher/CipherComponent/xAIProvider.js::_partsToOpenAIContent`
//! is the same function on the other side of the house). The alternative, a
//! frontend that knows what an `image_url` block looks like, puts one provider's
//! vocabulary into every layer above it and breaks on the next provider.
//!
//! # What the model is told when it cannot see the file
//!
//! The other half of the ported design, and the more important one. A model that
//! cannot take a PDF is not handed a PDF and it is **not handed silence** — it
//! gets a line saying the file exists, what kind it is, how big it is and where
//! it sits, so it can open it with a tool instead of answering a question about
//! a file it never received. A silent drop is how an assistant comes to
//! confidently describe an image nobody sent it.
//!
//! # The type comes from the bytes
//!
//! Never from the extension. A `.png` that is really a JPEG is rejected by every
//! vision endpoint there is, and the user who renamed it is not the one who will
//! understand the error. The same rule the rest of this ecosystem already
//! follows for media.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::settings::providers::Dialect;

/// The most a single file may be, in bytes, before it is described rather than
/// sent.
///
/// Provider limits vary and none of them is generous; this is below all of them
/// and above every screenshot. A larger file is not an error — it becomes a
/// notice naming its own path, which the agent can read with a tool.
pub const MAX_INLINE_BYTES: u64 = 8 * 1024 * 1024;

/// Global raw-bytes context payload cap (70 MB), ported from Cipher
/// (`config.cipherAI.maxTotalContextPayload`).
pub const GLOBAL_CONTEXT_PAYLOAD_CAP: u64 = 70 * 1024 * 1024;

/// Base64 inflation factor (4/3) + small JSON overhead, ported from Cipher
/// (`config.cipherAI.base64Factor`).
pub const BASE64_FACTOR: f64 = 1.34;

/// Maximum characters for auto-inlining a text file (60,000 characters),
/// ported from Cipher (`config.cipherAI.maxTextFileCharsForContext`).
/// Text files exceeding this limit produce `status="too_long"` and are not auto-loaded.
pub const MAX_TEXT_FILE_CHARS: usize = 60_000;

/// Provider capability data is independent of the provider's display name.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(default)]
pub struct MediaLimits {
    #[serde(alias = "request_bytes", alias = "total_bytes")]
    pub max_payload_bytes: u64,
    pub text_chars: usize,
}

/// The shipped defaults: a provider's default request size and the default
/// `attachments.max_text_chars` — each from the one place it is defined.
impl Default for MediaLimits {
    fn default() -> Self {
        Self {
            max_payload_bytes: crate::settings::configuration::parse_size_sum(
                crate::settings::providers::DEFAULT_MAX_PAYLOAD_SIZE,
            )
            .expect("the default payload size parses"),
            text_chars: crate::settings::configuration::Configuration::default()
                .number("attachments.max_text_chars") as usize,
        }
    }
}
impl MediaLimits {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}
pub fn provider_payload_budget(provider: &MediaLimits) -> u64 {
    ((provider.max_payload_bytes as f64) / BASE64_FACTOR) as u64
}

/// History attachments to drop so the request fits, oldest first.
///
/// An eviction is permanent — the caller records it — because an attachment
/// that came back later would rewrite a message the provider already cached.
/// So history only ever loses media, the oldest first, and only when the
/// payload budget or the file count says it must; the message holding the
/// current turn's media is never touched.
pub fn evictions(
    messages: &[crate::provider::Message],
    current: Option<usize>,
    total_budget: u64,
    max_history_files: usize,
) -> Vec<(usize, usize)> {
    let Some(current) = current else {
        return Vec::new();
    };
    let budget = total_budget.saturating_sub(messages[current].media.iter().map(|a| a.size).sum());
    let kept: Vec<(usize, usize, u64)> = history_media(messages, current)
        .map(|(i, j)| (i, j, messages[i].media[j].size))
        .collect();
    let mut bytes: u64 = kept.iter().map(|(_, _, size)| size).sum();
    let mut count = kept.len();
    let mut out = Vec::new();
    for (i, j, size) in kept {
        if bytes <= budget && count <= max_history_files {
            break;
        }
        out.push((i, j));
        bytes -= size;
        count -= 1;
    }
    out
}

/// The history attachments a request may inline: every one not evicted.
pub fn history_allowlist(
    messages: &[crate::provider::Message],
    current: Option<usize>,
) -> std::collections::HashSet<(usize, usize)> {
    current.map_or_else(Default::default, |current| {
        history_media(messages, current).collect()
    })
}

/// Live (not evicted) attachments before `current`, oldest first.
fn history_media(
    messages: &[crate::provider::Message],
    current: usize,
) -> impl Iterator<Item = (usize, usize)> + '_ {
    messages[..current]
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            matches!(
                m.role,
                crate::store::sessions::Role::User | crate::store::sessions::Role::Tool
            )
        })
        .flat_map(|(i, m)| {
            m.media
                .iter()
                .enumerate()
                .filter(|(_, a)| !a.evicted)
                .map(move |(j, _)| (i, j))
        })
}

/// Format integer with commas, e.g. 60000 -> "60,000".
pub fn human_num(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if i > 0 && (chars.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// One attached file, as the session stores it.
///
/// A path, not a copy (owner, 2026-10-11): what is sent is read from the
/// file, and the file's SHA-256 when it was sent is kept beside its path. A
/// later request that finds the file changed — or gone — renders the content
/// as it was sent from a copy, made only then: from the session's own copy of
/// it, or from the undo store when Rune's own tools changed it. With neither,
/// the attachment says the file changed or is gone, and the model is told so
/// (`session::context`), never handed new bytes under an old message — which
/// would also unseat every cached request after it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    /// The name the user knows it by.
    pub name: String,
    /// What it is, decided from its first bytes.
    pub mime: String,
    /// Where it is, absolute.
    pub path: String,
    pub size: u64,
    /// The SHA-256 of the file's content when the message was sent: what
    /// every later request checks the file against, and the name of the copy
    /// made once it changes. Stored as `snapshot`, the name it had while every
    /// attachment was copied — an older row's value is the same hash, of the
    /// copy it kept. `None` on rows written before either, and for a file that
    /// could not be read when sent: those are read live.
    #[serde(rename = "snapshot", default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// The file's modification time when it was sent, in nanoseconds since
    /// the epoch: with `size`, the cheap check that it is still what was sent.
    /// `None` on older rows; those are hashed again to tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<i64>,
    /// The inline text budget (`attachments.max_text_chars`) in force when the
    /// message was stored. History renders with it, so a later edit to the limit
    /// cannot re-render a message the provider already cached. `None` on rows
    /// written before it existed; those fall back to the live value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_chars: Option<usize>,
    /// Where this request reads the bytes from: the copy of what was sent,
    /// once the file no longer is; `None` reads the file itself. Set by the
    /// turn ([`Attachment::settle`]); never stored.
    #[serde(skip)]
    pub source: Option<std::path::PathBuf>,
    /// What was sent can no longer be shown: the file changed or went, and no
    /// copy of it exists. Set by the turn; never stored.
    #[serde(skip)]
    pub gone: Option<Gone>,
    /// Dropped from history to fit the provider's payload budget. Decided once
    /// by the turn and never undone (see [`evictions`]); never stored here.
    #[serde(skip)]
    pub evicted: bool,
}

/// Why an attachment's content as sent can no longer be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gone {
    /// The file is no longer at its path.
    NotFound,
    /// The file holds something else now.
    Changed,
}

/// How an attachment's file stands against what was sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// Nothing recorded to check it against, or it cannot be read to tell:
    /// read live, as it always was.
    Live,
    /// Still what was sent.
    Same,
    /// It holds something else now.
    Changed,
    /// It is no longer at its path.
    Missing,
}

/// Verifies the sent SHA independently of metadata. A file with the same size
/// and mtime can still have new content; every request checks that identity.
#[derive(Debug, Default)]
pub struct Fingerprints {
    _private: (),
}

impl Fingerprints {
    pub fn standing(&self, path: &str, _size: u64, _modified: Option<i64>, sha: &str) -> Standing {
        match std::fs::metadata(path) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => return Standing::Missing,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Standing::Missing,
            Err(_) => return Standing::Live,
        }
        match crate::checkpoint::objects::hash_file(std::path::Path::new(path)) {
            Ok(now) if now == sha => Standing::Same,
            Ok(_) => Standing::Changed,
            Err(_) => Standing::Live,
        }
    }
}

/// A file's modification time in nanoseconds since the epoch, where the
/// system keeps one.
fn modified_ns(meta: &std::fs::Metadata) -> Option<i64> {
    let since = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    i64::try_from(since.as_nanos()).ok()
}

/// Whether `sha` is a SHA-256 as Rune writes one — so a value read back from a
/// stored row can only ever name a file inside the copies' folder.
fn is_sha(sha: &str) -> bool {
    sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

impl Attachment {
    /// Read one file from disk and decide what it is.
    ///
    /// The user chose this file themselves, so the workspace jail does not apply
    /// — that jail is about what the *agent* may reach. A directory, a file that
    /// is not there, and a file too large to open are all refused here, where the
    /// person who typed the path is still watching.
    pub fn at(path: &std::path::Path) -> Result<Attachment> {
        let full = crate::platform::canonical(path)
            .map_err(|e| Error::invalid(format!("{}: {e}", path.display())))?;
        let meta = std::fs::metadata(&full)?;
        if meta.is_dir() {
            return Err(Error::invalid(format!("{} is a directory", full.display())));
        }
        let head = head_of(&full, SNIFF_BYTES)?;
        Ok(Attachment {
            name: full
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| full.display().to_string()),
            mime: sniff(&head, &full).to_string(),
            path: full.display().to_string(),
            size: meta.len(),
            ..Default::default()
        })
    }

    /// Fix what this attachment is for the rest of the session: the file's
    /// size, modification time and SHA-256 as it is sent — nothing is copied
    /// — and the text budget in force now. Every message that stores
    /// attachments — a prompt or a tool result — goes through here. A file
    /// that cannot be read stays a live reference with no hash; rendering
    /// then reports it unreadable, as it always did.
    pub fn seal(&mut self, text_chars: usize) {
        let path = std::path::Path::new(&self.path);
        // The time before the hash: a file written meanwhile then carries a
        // newer time than the one kept, and is hashed again, never trusted.
        if let Ok(meta) = std::fs::metadata(path) {
            if let Ok(sha) = crate::checkpoint::objects::hash_file(path) {
                self.size = meta.len();
                self.modified = modified_ns(&meta);
                self.sha = Some(sha);
            }
        }
        self.text_chars = Some(text_chars);
    }

    /// Decide where this request reads the attachment from (`dir` holds the
    /// session's copies, named by hash): the file while it is still what was
    /// sent; once it is not, the copy of what was sent — made now, the first
    /// time it is needed, from `original` (the undo store, which holds a
    /// file's content from before Rune's own tools changed it) — or, with no
    /// copy to be had, nothing: [`Attachment::gone`] says why. Returns how the
    /// file stands, for whoever tells the model.
    pub fn settle(
        &mut self,
        dir: &std::path::Path,
        prints: &Fingerprints,
        original: &dyn Fn(&str) -> Option<Vec<u8>>,
    ) -> Standing {
        self.source = None;
        self.gone = None;
        let Some(sha) = self.sha.clone().filter(|sha| is_sha(sha)) else {
            return Standing::Live;
        };
        let standing = prints.standing(&self.path, self.size, self.modified, &sha);
        if !matches!(standing, Standing::Changed | Standing::Missing) {
            return standing;
        }
        let copy = dir.join(&sha);
        if !copy.is_file() {
            if let Some(bytes) = original(&sha) {
                if crate::checkpoint::objects::hash_of(&bytes) == sha {
                    let _ = std::fs::create_dir_all(dir)
                        .map_err(Error::from)
                        .and_then(|()| crate::settings::secrets::atomic_write(&copy, &bytes));
                }
            }
        }
        if copy.is_file() {
            self.source = Some(copy);
        } else {
            self.gone = Some(match standing {
                Standing::Missing => Gone::NotFound,
                _ => Gone::Changed,
            });
        }
        standing
    }

    /// Where the bytes are read from: the copy of what was sent when the file
    /// no longer is, else the user's file.
    pub fn bytes_at(&self) -> &std::path::Path {
        self.source
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new(&self.path))
    }

    /// The exact sent content, checked after reading too: a rename/write between
    /// settlement and wire rendering must never put new bytes under an old row.
    pub(crate) fn read_verified(&self, max: u64) -> Result<Vec<u8>> {
        let bytes = crate::settings::configuration::read_bounded(self.bytes_at(), max)?;
        if self
            .sha
            .as_deref()
            .is_some_and(|sha| crate::checkpoint::objects::hash_of(&bytes) != sha)
        {
            return Err(Error::invalid(
                "attachment changed after it was sent; no new bytes were supplied",
            ));
        }
        Ok(bytes)
    }

    /// The bytes, base64-encoded, or the reason there are none.
    pub fn load(&self, max_payload_bytes: u64) -> std::result::Result<String, String> {
        let meta = std::fs::metadata(self.bytes_at())
            .map_err(|e| format!("{} could not be read: {e}", self.name))?;
        if meta.len() > max_payload_bytes {
            return Err(format!(
                "{} is {} — exceeds {} payload limit; read it from {} instead",
                self.name,
                human(meta.len()),
                human(max_payload_bytes),
                self.path
            ));
        }
        let bytes = self
            .read_verified(max_payload_bytes)
            .map_err(|e| format!("{} could not be read: {e}", self.name))?;
        Ok(base64(&bytes))
    }

    /// Pure informational summary when the file cannot be inlined.
    pub fn notice(&self, why: &str) -> String {
        format!(
            "File: {} ({} · {}) — {why}. It is at {}",
            self.name,
            self.mime,
            human(self.size),
            self.path
        )
    }

    /// Whether this is text the model can simply be shown.
    pub fn is_text(&self) -> bool {
        is_text_mime(&self.mime)
    }

    /// Metadata-only eligibility, shared by the composer and the wire builder.
    /// Reading the bytes remains a request-time check, never a render-time job.
    pub fn inline_reason(
        &self,
        model: Option<&crate::settings::providers::ModelInfo>,
        dialect: Dialect,
        max_payload_bytes: u64,
    ) -> Option<String> {
        if self.size > max_payload_bytes {
            return Some(format!(
                "exceeds the {} payload budget",
                human(max_payload_bytes)
            ));
        }
        if self.is_text() {
            return None;
        }
        let Some(modality) = modality_of(&self.mime) else {
            return Some("this file type cannot be sent inline".into());
        };
        if dialect == Dialect::Anthropic && matches!(modality, "audio" | "video") {
            return Some(format!("this endpoint has no {modality} input"));
        }
        if dialect == Dialect::OpenAi
            && modality == "file"
            && !model.is_some_and(|m| m.accepts("file"))
        {
            return Some(
                "this OpenAI-compatible endpoint does not support inline PDF file blocks".into(),
            );
        }
        if dialect == Dialect::Anthropic
            && modality == "image"
            && !matches!(
                self.mime.as_str(),
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            )
        {
            return Some("convert this image to PNG, JPEG, GIF or WebP".into());
        }
        match model {
            Some(m) if m.accepts(modality) => None,
            Some(m) if !m.modalities.is_empty() => {
                Some(format!("model does not accept {modality} input"))
            }
            _ => Some(format!(
                "{modality} support is not published for this model"
            )),
        }
    }
}

/// Is this a MIME type whose bytes are readable as text?
///
/// The `text/*` family, plus the application types that are text in everything
/// but their label — which is most of what a developer attaches.
pub fn decode_text(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(b"\xef\xbb\xbf") {
        return String::from_utf8(bytes[3..].to_vec()).ok();
    }
    if bytes.starts_with(b"\xff\xfe") {
        let u16s: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16(&u16s).ok();
    }
    if bytes.starts_with(b"\xfe\xff") {
        let u16s: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16(&u16s).ok();
    }
    String::from_utf8(bytes.to_vec()).ok()
}

pub fn is_text_mime(mime: &str) -> bool {
    let m = mime.to_ascii_lowercase();
    m.starts_with("text/")
        || matches!(
            m.as_str(),
            "application/json"
                | "application/xml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/x-typescript"
                | "application/x-yaml"
                | "application/yaml"
                | "application/toml"
                | "application/sql"
                | "application/x-sql"
                | "application/x-sh"
                | "application/x-python"
                | "application/x-rust"
                | "application/x-c"
                | "application/x-c++"
                | "application/x-csharp"
                | "application/x-java-source"
                | "application/rtf"
        )
        || m.ends_with("+json")
        || m.ends_with("+xml")
        || m.ends_with("+yaml")
}

/// What these bytes are.
///
/// A signature table, then a text/binary decision, and the extension only as the
/// last word on a file that is plainly text — because `main.rs` and `main.py`
/// have no signature and never will, while a JPEG always starts `FF D8 FF`.
fn sniff(head: &[u8], path: &std::path::Path) -> &'static str {
    const SIGNATURES: &[(&[u8], &str)] = &[
        (b"\xFF\xD8\xFF", "image/jpeg"),
        (b"\x89PNG\r\n\x1a\n", "image/png"),
        (b"GIF87a", "image/gif"),
        (b"GIF89a", "image/gif"),
        (b"BM", "image/bmp"),
        (b"II*\x00", "image/tiff"),
        (b"MM\x00*", "image/tiff"),
        (b"%PDF-", "application/pdf"),
        (b"OggS", "audio/ogg"),
        (b"fLaC", "audio/flac"),
        (b"\x1a\x45\xdf\xa3", "video/webm"),
        (b"FLV\x01", "video/x-flv"),
        (b"\x7fELF", "application/x-executable"),
        (b"MZ", "application/x-dosexec"),
        (b"SQLite format 3\x00", "application/vnd.sqlite3"),
        (b"ID3", "audio/mpeg"),
    ];
    for (magic, mime) in SIGNATURES {
        if head.starts_with(magic) {
            if *mime == "audio/ogg" {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    if ext.eq_ignore_ascii_case("opus") {
                        return "audio/opus";
                    }
                }
            }
            if *mime == "video/webm" {
                if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                    if ext.eq_ignore_ascii_case("mkv") {
                        return "video/x-matroska";
                    }
                    if ext.eq_ignore_ascii_case("weba") {
                        return "audio/webm";
                    }
                }
            }
            return mime;
        }
    }
    // Zip container (Office OpenXML / Zip archive / EPUB)
    if head.starts_with(b"PK\x03\x04") {
        return match path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            "odt" => "application/vnd.oasis.opendocument.text",
            "ods" => "application/vnd.oasis.opendocument.spreadsheet",
            "odp" => "application/vnd.oasis.opendocument.presentation",
            "epub" => "application/epub+zip",
            _ => "application/zip",
        };
    }
    // OLE2 Compound Document (Legacy Office: .doc, .xls, .ppt)
    if head.starts_with(b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1") {
        return match path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "doc" => "application/msword",
            "xls" => "application/vnd.ms-excel",
            "ppt" => "application/vnd.ms-powerpoint",
            _ => "application/x-ole-storage",
        };
    }
    // MP3 raw frame sync
    if head.len() >= 2 && head[0] == 0xFF && (head[1] & 0xE0) == 0xE0 {
        return "audio/mpeg";
    }
    // The container formats whose signature is not at byte zero.
    if head.len() >= 12 {
        if &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
            return "image/webp";
        }
        if &head[0..4] == b"RIFF" && &head[8..12] == b"WAVE" {
            return "audio/wav";
        }
        if &head[0..4] == b"RIFF" && &head[8..12] == b"AVI " {
            return "video/x-msvideo";
        }
        if &head[0..4] == b"FORM" && &head[8..12] == b"AIFF" {
            return "audio/aiff";
        }
        if &head[4..8] == b"ftyp" {
            let brand = &head[8..12];
            return match brand {
                b"qt  " => "video/quicktime",
                b"heic" | b"heix" => "image/heic",
                b"heif" | b"mif1" => "image/heif",
                b"avif" | b"avis" => "image/avif",
                _ => "video/mp4",
            };
        }
    }
    if looks_binary(head) {
        return match path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            // Audio
            "mp3" | "mpga" => "audio/mpeg",
            "wav" => "audio/wav",
            "m4a" => "audio/mp4",
            "aac" => "audio/aac",
            "flac" => "audio/flac",
            "ogg" | "oga" => "audio/ogg",
            "opus" => "audio/opus",
            "aiff" | "aif" => "audio/aiff",
            "wma" => "audio/x-ms-wma",
            "amr" => "audio/amr",
            "pcm" => "audio/pcm",
            "weba" => "audio/webm",
            // Video
            "mov" => "video/quicktime",
            "avi" => "video/x-msvideo",
            "mkv" => "video/x-matroska",
            "webm" => "video/webm",
            "flv" => "video/x-flv",
            "wmv" => "video/x-ms-wmv",
            "3gp" => "video/3gpp",
            "3g2" => "video/3gpp2",
            "mp4" | "m4v" => "video/mp4",
            "mpg" | "mpeg" => "video/mpeg",
            // Image
            "jpg" | "jpeg" => "image/jpeg",
            "png" => "image/png",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "bmp" => "image/bmp",
            "tiff" | "tif" => "image/tiff",
            "heic" => "image/heic",
            "heif" => "image/heif",
            "avif" => "image/avif",
            "ico" => "image/x-icon",
            // Documents
            "pdf" => "application/pdf",
            "epub" => "application/epub+zip",
            "rtf" => "application/rtf",
            _ => "application/octet-stream",
        };
    }
    // Text, and now the name may say which kind — a distinction that costs
    // nothing to be wrong about, because either way the model is shown the text.
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "json" => "application/json",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "xml" => "text/xml",
        "yaml" | "yml" => "text/x-yaml",
        "toml" => "text/x-toml",
        "js" | "mjs" | "cjs" => "text/javascript",
        "ts" | "tsx" => "text/x-typescript",
        "py" => "text/x-python",
        "rs" => "text/x-rust",
        "go" => "text/x-go",
        "c" | "h" => "text/x-c",
        "cpp" | "hpp" | "cc" | "cxx" => "text/x-c++",
        "cs" => "text/x-csharp",
        "java" => "text/x-java-source",
        "rb" => "text/x-ruby",
        "php" => "text/x-php",
        "sh" | "bash" | "zsh" => "text/x-shellscript",
        "sql" => "application/x-sql",
        "diff" | "patch" => "text/x-diff",
        "log" => "text/x-log",
        "lua" => "text/x-lua",
        "pl" => "text/x-perl",
        "kt" => "text/x-kotlin",
        "swift" => "text/x-swift",
        _ => "text/plain",
    }
}

/// How much of a file decides what it is. One window for every reader — a
/// prompt attachment, `inspect_file`, `search_text` — so a file is never text
/// to one of them and binary to another (a log whose last block a power cut
/// left as zeros was read whole by one and attached by the other).
pub const SNIFF_BYTES: usize = 8192;

/// What a file is, from its first [`SNIFF_BYTES`] and its name: the one
/// decision every tool asks. Text is whatever [`is_text_mime`] accepts.
pub fn kind_of(head: &[u8], path: &std::path::Path) -> &'static str {
    sniff(&head[..head.len().min(SNIFF_BYTES)], path)
}

/// Binary: a NUL byte among bytes that are not UTF-8.
///
/// A NUL alone is what `grep` and `git` go by, and it misreads text that holds
/// a few — a log whose tail a crash or a power cut left as zeros, a capture of
/// a program that printed one. Those are still UTF-8 throughout, which a binary
/// format almost never is past its first bytes; a character cut by the end of
/// the window is not held against it.
fn looks_binary(head: &[u8]) -> bool {
    head.contains(&0)
        && match std::str::from_utf8(head) {
            Ok(_) => false,
            Err(e) => e.error_len().is_some(),
        }
}

fn head_of(path: &std::path::Path, bytes: usize) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; bytes];
    let read = file.read(&mut buf)?;
    buf.truncate(read);
    Ok(buf)
}

/// `2411724` → `2.3 MB`. A size is read as a magnitude, never as digits.
pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// Base64, standard alphabet with padding.
///
/// Written out rather than pulled in: the whole of it is twenty lines, and a
/// dependency that ships in a public repository is a dependency somebody has to
/// audit.
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// What one attachment turns into for one model.
///
/// The decision lives here, once, and each dialect only renders it — so
/// "can this model see a PDF" is answered in one place and `openai.rs` and
/// `anthropic.rs` cannot drift apart about it.
#[derive(Debug, Clone, PartialEq)]
pub enum Inlined {
    /// Show it as text, because it is text.
    Text(String),
    /// Send the bytes. `modality` is what the endpoint calls this kind.
    Bytes {
        modality: &'static str,
        mime: String,
        data: String,
        name: String,
    },
    /// Say what it is, because it cannot be sent.
    Notice(String),
}

/// Cipher's user-turn layout: one tagged attachment at a time, then user text.
/// Both dialects only translate these canonical parts into their wire blocks.
pub fn parts(
    files: &[Attachment],
    text: &str,
    model: Option<&crate::settings::providers::ModelInfo>,
    dialect: Dialect,
) -> Vec<Inlined> {
    let empty_allowlist = std::collections::HashSet::new();
    parts_for_message(
        files,
        text,
        model,
        dialect,
        &MediaLimits::default(),
        true,
        0,
        &empty_allowlist,
    )
}

pub(crate) fn inline_envelope(file: &Attachment) -> (String, String) {
    let path = crate::frame::attribute(&file.path);
    let size = human(file.size);
    (
        format!("\n<file_attachment path=\"{path}\" size=\"{size}\">\n"),
        "\n</file_attachment>\n".into(),
    )
}

/// Cipher's user-turn layout with dynamic provider payload budget and context waterfall:
/// Attachments first with strict Cipher XML status tags, then user text.
#[allow(clippy::too_many_arguments)] // Shared existing attachment contract used by both provider dialects.
pub fn parts_for_message(
    files: &[Attachment],
    text: &str,
    model: Option<&crate::settings::providers::ModelInfo>,
    dialect: Dialect,
    provider: &MediaLimits,
    is_current: bool,
    msg_idx: usize,
    history_allowlist: &std::collections::HashSet<(usize, usize)>,
) -> Vec<Inlined> {
    let mut out = Vec::new();
    let total_budget = provider_payload_budget(provider);
    let mut consumed_bytes = 0u64;

    for (j, file) in files.iter().enumerate() {
        let path = file
            .path
            .replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        let size_str = human(file.size);

        // 1. History attachment waterfall budget check
        if !is_current && !history_allowlist.contains(&(msg_idx, j)) {
            out.push(Inlined::Text(format!(
                "\n<file_attachment path=\"{path}\" size=\"{size_str}\" status=\"skipped_context_budget\" />\n"
            )));
            continue;
        }

        // What was sent is gone and no copy of it was kept: said in the
        // same words on every request, so the message stays as cached from
        // here on.
        match file.gone {
            Some(Gone::NotFound) => {
                out.push(Inlined::Text(format!(
                    "\n<file_attachment path=\"{path}\" size=\"{size_str}\" status=\"not_found\" note=\"No file is at {path} any more.\" />\n"
                )));
                continue;
            }
            Some(Gone::Changed) => {
                out.push(Inlined::Text(format!(
                    "\n<file_attachment path=\"{path}\" size=\"{size_str}\" status=\"changed\" note=\"The file changed after it was attached and its content as sent was not kept. Read it with inspect_file.\" />\n"
                )));
                continue;
            }
            None => {}
        }

        // 2. Text files, held to the budget recorded when they were sent:
        if file.is_text() {
            let max_text_file_chars = file.text_chars.unwrap_or(provider.text_chars);
            match file.read_verified(total_budget) {
                Ok(bytes) => match decode_text(&bytes) {
                    Some(text_content) => {
                        let char_count = text_content.chars().count();
                        if char_count > max_text_file_chars {
                            let total_lines = text_content.lines().count();
                            out.push(Inlined::Text(format!(
                                "\n<file_attachment path=\"{path}\" size=\"{size_str}\" status=\"too_long\" char_limit=\"{max_text_file_chars}\">\nThis file exceeds the {} character context limit and was not auto-loaded (truncated {total_lines} lines more). You can inspect it using the inspect_file tool — either read the full content or specific line ranges as needed.\n</file_attachment>\n",
                                human_num(max_text_file_chars)
                            )));
                        } else {
                            out.push(Inlined::Text(format!(
                                "\n<file_attachment path=\"{path}\" size=\"{size_str}\">\n{text_content}\n</file_attachment>\n"
                            )));
                        }
                    }
                    None => {
                        out.push(Inlined::Text(format!(
                            "\n<file_attachment path=\"{path}\" size=\"{size_str}\" status=\"binary_not_injected\">\nThis is a binary file format that is not natively injected into your context. Use inspect_file or terminal commands to read its content.\n</file_attachment>\n"
                        )));
                    }
                },
                Err(_) => {
                    out.push(Inlined::Text(format!(
                        "\n<file_attachment path=\"{path}\" size=\"{size_str}\" status=\"unsupported_by_model\" note=\"Could not read file from disk. It is at {path}\" />\n"
                    )));
                }
            }
            continue;
        }

        // 3. Binary files (Images, Audio, Video, PDF, or non-inlinable formats):
        let modality = modality_of(&file.mime);
        let Some(modality) = modality else {
            // Non-inlinable binary format (e.g. .xlsx, .xls, .docx, .zip, .tar, .exe, .bin)
            out.push(Inlined::Text(format!(
                "\n<file_attachment path=\"{path}\" size=\"{size_str}\" status=\"binary_not_injected\">\nThis is a binary file format that is not natively injected into your context. Use inspect_file or terminal commands to read its content.\n</file_attachment>\n"
            )));
            continue;
        };

        // Check dialect & model support for modality:
        let dialect_unsupported = (dialect == Dialect::Anthropic
            && matches!(modality, "audio" | "video"))
            || (dialect == Dialect::OpenAi
                && modality == "file"
                && !model.is_some_and(|m| m.accepts("file")));
        let image_format_unsupported = modality == "image"
            && dialect == Dialect::Anthropic
            && !matches!(
                file.mime.as_str(),
                "image/png" | "image/jpeg" | "image/gif" | "image/webp"
            );
        let model_unsupported = match model {
            Some(m) => !m.accepts(modality),
            None => false,
        };

        if dialect_unsupported || image_format_unsupported || model_unsupported {
            let note = if dialect == Dialect::OpenAi && modality == "file" {
                "This OpenAI-compatible endpoint does not support inline PDF file blocks. Use the inspect_file tool to inspect its content."
            } else if dialect_unsupported {
                "The active endpoint does not support this media type. Switch to a model that supports it, or use the inspect_file tool."
            } else if image_format_unsupported {
                "Anthropic only accepts PNG, JPEG, GIF, or WebP images. Use inspect_file or convert the image."
            } else {
                "The active model cannot read this file type. Switch to a model that supports it, or use the inspect_file tool."
            };
            out.push(Inlined::Text(format!(
                "\n<file_attachment path=\"{path}\" size=\"{size_str}\" status=\"unsupported_by_model\" note=\"{note}\" />\n"
            )));
            continue;
        }

        // Check size against provider limits:
        let file_too_large = consumed_bytes + file.size > total_budget;

        if file_too_large {
            let budget_mb = (total_budget / (1024 * 1024)).max(1);
            out.push(Inlined::Text(format!(
                "\n<file_attachment path=\"{path}\" size=\"{size_str}\" status=\"too_large\">\nThis file exceeds the {budget_mb} MB payload limit for this model and was not auto-loaded. You can inspect it using the inspect_file tool.\n</file_attachment>\n"
            )));
            continue;
        }

        match decide(
            file,
            model,
            dialect,
            total_budget.saturating_sub(consumed_bytes),
        ) {
            part @ Inlined::Bytes { .. } => {
                consumed_bytes += file.size;
                let (open, close) = inline_envelope(file);
                out.push(Inlined::Text(open));
                out.push(part);
                out.push(Inlined::Text(close));
            }
            Inlined::Notice(why) | Inlined::Text(why) => {
                out.push(Inlined::Text(format!(
                    "\n<file_attachment path=\"{path}\" size=\"{size_str}\" status=\"not_injected\">\n{why}\n</file_attachment>\n"
                )));
            }
        }
    }

    if !text.trim().is_empty() {
        out.push(Inlined::Text(text.into()));
    }
    out
}

/// What kind of thing this MIME type is, in the vocabulary every endpoint shares.
pub fn modality_of(mime: &str) -> Option<&'static str> {
    if mime.starts_with("image/") {
        return Some("image");
    }
    if mime.starts_with("audio/") {
        return Some("audio");
    }
    if mime.starts_with("video/") {
        return Some("video");
    }
    (mime == "application/pdf").then_some("file")
}

/// Decide what this model may be shown of this file.
///
/// `model` is `None` when nothing is published about it, and that is read as
/// **text only** — see [`crate::settings::providers::ModelInfo::modalities`].
pub fn decide(
    a: &Attachment,
    model: Option<&crate::settings::providers::ModelInfo>,
    dialect: Dialect,
    max_inline_bytes: u64,
) -> Inlined {
    if let Some(reason) = a.inline_reason(model, dialect, max_inline_bytes) {
        return Inlined::Notice(a.notice(&reason));
    }
    // Text first, and for every model: a source file is not a modality, it is
    // just more of the message.
    if a.is_text() {
        return match a.load(max_inline_bytes) {
            Ok(data) => match decode(&data).and_then(|bytes| {
                String::from_utf8(bytes).map_err(|_| "Invalid UTF-8 content".to_string())
            }) {
                Ok(text) => Inlined::Text(format!("--- {} ---\n{text}", a.name)),
                Err(_) => Inlined::Notice(a.notice("it is not valid UTF-8 after all")),
            },
            Err(why) => Inlined::Notice(format!("[{why}]")),
        };
    }
    let modality = modality_of(&a.mime).expect("inline eligibility checked the file type");
    match a.load(max_inline_bytes) {
        Ok(data) => Inlined::Bytes {
            modality,
            mime: a.mime.clone(),
            data,
            name: a.name.clone(),
        },
        Err(why) => Inlined::Notice(format!("[{why}]")),
    }
}

/// Decode both local and external media; invalid alphabet or padding is rejected.
pub fn decode(text: &str) -> std::result::Result<Vec<u8>, String> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|_| "Invalid base64 content".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_size_and_restored_mtime_cannot_replace_an_attachment_silently() {
        let path = write("same-stamp.txt", b"before-content");
        let mut attachment = Attachment::at(&path).unwrap();
        attachment.seal(60_000);
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, b"after--content").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(
            Fingerprints::default().standing(
                &attachment.path,
                attachment.size,
                attachment.modified,
                attachment.sha.as_deref().unwrap()
            ),
            Standing::Changed
        );
        assert!(attachment.read_verified(1024).is_err());
    }

    /// The shipped `attachments.max_history_files`.
    fn max_history_files() -> usize {
        crate::settings::configuration::Configuration::default()
            .number("attachments.max_history_files") as usize
    }

    fn write(name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rune-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// The rule this ecosystem already paid to learn: what a file *is* comes
    /// from its bytes. A `.png` that is really a JPEG is refused by every vision
    /// endpoint there is, and the person who renamed it is not the one who has
    /// to read the error.
    #[test]
    fn the_type_comes_from_the_bytes_and_never_from_the_name() {
        let lying = write("screenshot.png", b"\xFF\xD8\xFF\xE0 not a png at all");
        assert_eq!(Attachment::at(&lying).unwrap().mime, "image/jpeg");

        let real = write("real.png", b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR");
        assert_eq!(Attachment::at(&real).unwrap().mime, "image/png");
    }

    /// …with one deliberate exception, and it is not really one: source code has
    /// no signature, so the name is the only thing left. Either way the model is
    /// shown the text, so being wrong here costs nothing.
    #[test]
    fn text_files_fall_back_to_the_name_and_stay_text() {
        let code = write("main.rs", b"fn main() {}\n");
        let a = Attachment::at(&code).unwrap();
        assert_eq!(a.mime, "text/x-rust");
        assert!(a.is_text());

        let data = write("data.json", b"{\"a\": 1}");
        assert_eq!(Attachment::at(&data).unwrap().mime, "application/json");
    }

    #[test]
    fn a_nul_among_bytes_that_are_not_utf8_makes_it_binary_whatever_it_is_called() {
        let blob = write("notes.txt", b"text\x00then \xff\xfe a nul");
        let a = Attachment::at(&blob).unwrap();
        assert_eq!(a.mime, "application/octet-stream");
        assert!(!a.is_text());
        // A NUL in text that is UTF-8 throughout leaves it text.
        let log = write("log.txt", b"text\x00then a nul");
        assert!(Attachment::at(&log).unwrap().is_text());
    }

    #[test]
    fn a_directory_and_a_missing_file_are_refused_where_the_user_can_see_it() {
        let dir = std::env::temp_dir().join(format!("rune-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Attachment::at(&dir).is_err());
        assert!(Attachment::at(&dir.join("nothing-here")).is_err());
    }

    /// A file the model cannot be shown is still described, and the description
    /// names the path — that is the difference between a dead end and a task.
    #[test]
    fn a_notice_names_the_file_its_size_and_where_to_find_it() {
        let f = write("report.pdf", b"%PDF-1.7 ...");
        let a = Attachment::at(&f).unwrap();
        let notice = a.notice("this model cannot read PDFs");
        assert!(notice.contains("report.pdf"));
        assert!(notice.contains("application/pdf"));
        assert!(
            notice.contains(&a.path),
            "the model is not told where the file is"
        );
        assert!(notice.contains("cannot read PDFs"));
    }

    #[test]
    fn base64_matches_the_standard_alphabet_and_pads() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn sizes_are_read_as_magnitudes() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(2_411_724), "2.3 MB");
    }

    use crate::settings::providers::ModelInfo;

    fn model(modalities: &[&str]) -> ModelInfo {
        ModelInfo {
            id: "m".into(),
            modalities: modalities.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// A file the model can take is sent. One it cannot is **described**, never
    /// dropped — a silent drop is how an assistant comes to answer confidently
    /// about an image nobody sent it.
    #[test]
    fn a_model_that_cannot_see_is_told_rather_than_left_guessing() {
        let png = write("shot.png", b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR");
        let a = Attachment::at(&png).unwrap();

        match decide(
            &a,
            Some(&model(&["image"])),
            Dialect::OpenAi,
            MAX_INLINE_BYTES,
        ) {
            Inlined::Bytes {
                modality,
                mime,
                data,
                ..
            } => {
                assert_eq!(modality, "image");
                assert_eq!(mime, "image/png");
                assert!(!data.is_empty());
            }
            other => panic!("a vision model was not shown the image: {other:?}"),
        }

        for blind in [Some(&model(&["audio"])), Some(&model(&[])), None] {
            match decide(&a, blind, Dialect::OpenAi, MAX_INLINE_BYTES) {
                Inlined::Notice(text) => {
                    assert!(text.contains("shot.png"));
                    assert!(
                        text.contains(&a.path),
                        "the notice must say where the file is"
                    );
                }
                other => panic!("a text-only model was handed bytes: {other:?}"),
            }
        }
    }

    /// Text is text for every model. It is not a modality to be gated — it is
    /// more of the message.
    #[test]
    fn a_text_file_reaches_every_model_as_its_own_contents() {
        let f = write("notes.md", b"# Title\nbody\n");
        let a = Attachment::at(&f).unwrap();
        match decide(&a, None, Dialect::OpenAi, MAX_INLINE_BYTES) {
            Inlined::Text(text) => {
                assert!(text.contains("# Title"));
                assert!(
                    text.contains("notes.md"),
                    "the model should know which file this was"
                );
            }
            other => panic!("text was not inlined: {other:?}"),
        }
    }

    #[test]
    fn a_kind_no_endpoint_takes_is_described_without_asking_the_model() {
        let z = write("bundle.zip", b"PK\x03\x04rest");
        let a = Attachment::at(&z).unwrap();
        assert!(modality_of(&a.mime).is_none());
        assert!(matches!(
            decide(
                &a,
                Some(&model(&["image", "file"])),
                Dialect::OpenAi,
                MAX_INLINE_BYTES
            ),
            Inlined::Notice(_)
        ));
    }

    #[test]
    fn base64_round_trips() {
        for case in [
            &b""[..],
            b"f",
            b"fo",
            b"foo",
            b"foobar",
            &[0u8, 255, 128, 3],
        ] {
            assert_eq!(
                decode(&base64(case)).unwrap(),
                case,
                "round trip failed for {case:?}"
            );
        }
    }

    #[test]
    fn text_exceeding_60k_chars_is_marked_too_long_with_line_count() {
        let mut large_text = String::with_capacity(70_000);
        for i in 1..=1000 {
            large_text.push_str(&format!(
                "Line {i}: This is some test content repeating to exceed limit.\n"
            ));
        }
        assert!(large_text.chars().count() > MAX_TEXT_FILE_CHARS);
        let path = write("large.txt", large_text.as_bytes());
        let a = Attachment::at(&path).unwrap();

        let empty = std::collections::HashSet::new();
        let parts = parts_for_message(
            &[a],
            "",
            None,
            Dialect::OpenAi,
            &MediaLimits {
                max_payload_bytes: 100 * 1024 * 1024,
                ..MediaLimits::default()
            },
            true,
            0,
            &empty,
        );
        assert_eq!(parts.len(), 1);
        match &parts[0] {
            Inlined::Text(xml) => {
                assert!(
                    xml.contains("status=\"too_long\""),
                    "must have status=too_long: {xml}"
                );
                assert!(
                    xml.contains("char_limit=\"60000\""),
                    "must carry char_limit: {xml}"
                );
                assert!(
                    xml.contains("truncated 1000 lines more"),
                    "must show line count: {xml}"
                );
                assert!(
                    xml.contains("inspect_file"),
                    "must advise inspect_file: {xml}"
                );
            }
            other => panic!("expected Inlined::Text, got {other:?}"),
        }
    }

    #[test]
    fn binary_too_large_uses_too_large_status_and_no_lines() {
        let path = write("small.png", b"\x89PNG\r\n\x1a\nfake image content");
        let mut a = Attachment::at(&path).unwrap();
        // Artificially set size to 25MB (exceeds Anthropic 32MB payload budget of ~23.8MB, but within Google 100MB)
        a.size = 25 * 1024 * 1024;

        let empty = std::collections::HashSet::new();
        let m = model(&["image"]);

        // On Anthropic (32MB payload cap): 25MB exceeds raw budget of ~23.8MB -> status="too_large"
        let parts_anthropic = parts_for_message(
            &[a.clone()],
            "",
            Some(&m),
            Dialect::Anthropic,
            &MediaLimits {
                max_payload_bytes: 32 * 1024 * 1024,
                ..MediaLimits::default()
            },
            true,
            0,
            &empty,
        );
        match &parts_anthropic[0] {
            Inlined::Text(xml) => {
                assert!(
                    xml.contains("status=\"too_large\""),
                    "must be status=too_large: {xml}"
                );
                assert!(
                    !xml.contains("lines"),
                    "binary file must not mention lines: {xml}"
                );
            }
            other => panic!("expected Inlined::Text, got {other:?}"),
        }

        // On Google (100MB payload cap): 25MB is well within ~74.6MB raw budget -> inlined as Bytes
        let parts_google = parts_for_message(
            &[a],
            "",
            Some(&m),
            Dialect::OpenAi,
            &MediaLimits {
                max_payload_bytes: 100 * 1024 * 1024,
                ..MediaLimits::default()
            },
            true,
            0,
            &empty,
        );
        assert_eq!(
            parts_google.len(),
            3,
            "Google allows 25MB image (open tag, bytes, close tag)"
        );
        assert!(matches!(parts_google[1], Inlined::Bytes { .. }));
    }

    #[test]
    fn uninlinable_binary_uses_binary_not_injected() {
        let path = write("sheet.xlsx", b"PK\x03\x04fake excel content");
        let a = Attachment::at(&path).unwrap();
        let empty = std::collections::HashSet::new();
        let parts = parts_for_message(
            &[a],
            "",
            None,
            Dialect::OpenAi,
            &MediaLimits {
                max_payload_bytes: 100 * 1024 * 1024,
                ..MediaLimits::default()
            },
            true,
            0,
            &empty,
        );
        match &parts[0] {
            Inlined::Text(xml) => {
                assert!(
                    xml.contains("status=\"binary_not_injected\""),
                    "must be binary_not_injected: {xml}"
                );
                assert!(
                    xml.contains("inspect_file"),
                    "must advise inspect_file: {xml}"
                );
            }
            other => panic!("expected Inlined::Text, got {other:?}"),
        }
    }

    #[test]
    fn history_attachments_waterfall_budget() {
        let budget = provider_payload_budget(&MediaLimits {
            max_payload_bytes: 32 * 1024 * 1024,
            ..MediaLimits::default()
        }); // 23.8 MB
        assert!(budget > 20 * 1024 * 1024);

        let mut m0 = crate::provider::Message::user("first");
        m0.media = vec![Attachment {
            name: "old1.png".into(),
            mime: "image/png".into(),
            path: "/path/old1.png".into(),
            size: 15 * 1024 * 1024,
            ..Default::default()
        }];

        let mut m1 = crate::provider::Message::user("second");
        m1.media = vec![Attachment {
            name: "old2.png".into(),
            mime: "image/png".into(),
            path: "/path/old2.png".into(),
            size: 10 * 1024 * 1024,
            ..Default::default()
        }];

        let mut m2 = crate::provider::Message::user("current");
        m2.media = vec![Attachment {
            name: "curr.png".into(),
            mime: "image/png".into(),
            path: "/path/curr.png".into(),
            size: 5 * 1024 * 1024,
            ..Default::default()
        }];

        let mut messages = vec![m0, m1, m2];
        // Current consumes 5MB, leaving ~18.8MB for history holding 25MB: the
        // OLDEST goes, never the newer one.
        let evicted = evictions(&messages, Some(2), budget, max_history_files());
        assert_eq!(evicted, vec![(0, 0)], "only the oldest is dropped");
        messages[0].media[0].evicted = true;
        let allowlist = history_allowlist(&messages, Some(2));
        assert!(allowlist.contains(&(1, 0)), "m1 must be in allowlist");
        assert!(!allowlist.contains(&(0, 0)), "m0 stays dropped");

        // Permanence: with a tiny current attachment the budget would fit m0
        // again, but an eviction is never undone — and nothing new is evicted.
        messages[2].media[0].size = 1;
        assert!(evictions(&messages, Some(2), budget, max_history_files()).is_empty());
        assert!(!history_allowlist(&messages, Some(2)).contains(&(0, 0)));
    }

    #[test]
    fn the_file_count_cap_drops_the_oldest_first() {
        let mut messages: Vec<crate::provider::Message> = (0..4)
            .map(|i| {
                let mut m = crate::provider::Message::user(format!("m{i}"));
                m.media = vec![Attachment {
                    size: 1,
                    ..Default::default()
                }];
                m
            })
            .collect();
        messages.push(crate::provider::Message::user("current"));
        assert_eq!(
            evictions(&messages, Some(4), u64::MAX, 2),
            vec![(0, 0), (1, 0)]
        );
    }

    /// A history attachment rendered as the model reads it.
    fn rendered(file: &Attachment) -> String {
        let mut allowlist = std::collections::HashSet::new();
        allowlist.insert((0, 0));
        parts_for_message(
            std::slice::from_ref(file),
            "",
            None,
            Dialect::OpenAi,
            &MediaLimits::default(),
            false,
            0,
            &allowlist,
        )
        .into_iter()
        .map(|part| match part {
            Inlined::Text(t) | Inlined::Notice(t) => t,
            Inlined::Bytes { .. } => String::new(),
        })
        .collect()
    }

    /// A fresh folder for a session's copies.
    fn copies(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("rune-media-{}", std::process::id()))
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn nothing_kept(_: &str) -> Option<Vec<u8>> {
        None
    }

    /// Owner, 2026-10-11: an attachment is its path and its SHA-256 — nothing
    /// is copied when it is sent, and while the file is what was sent, every
    /// request reads it from where it is.
    #[test]
    fn an_unchanged_attachment_is_read_from_its_path_and_never_copied() {
        let path = write("as_sent.txt", b"what the user attached");
        let mut file = Attachment::at(&path).unwrap();
        file.seal(1000);
        assert_eq!(
            file.sha.as_deref(),
            Some(crate::checkpoint::objects::hash_of(b"what the user attached").as_str())
        );
        assert!(file.modified.is_some());
        let dir = copies("unchanged");
        let prints = Fingerprints::default();
        for _ in 0..3 {
            assert_eq!(file.settle(&dir, &prints, &nothing_kept), Standing::Same);
            assert!(file.source.is_none() && file.gone.is_none());
            assert_eq!(file.bytes_at(), path.as_path());
        }
        assert!(!dir.exists(), "nothing is copied while nothing changed");
        assert!(rendered(&file).contains("what the user attached"));
    }

    /// Changed by Rune's own tools, the content as sent is in the undo store:
    /// it is copied out the first time the change is seen — and only then —
    /// and every request after renders it, byte for byte, so the cached
    /// message holds.
    #[test]
    fn a_changed_attachment_renders_what_was_sent_from_a_copy_made_when_seen() {
        let path = write("edited_by_rune.txt", b"version one");
        let mut file = Attachment::at(&path).unwrap();
        file.seal(1000);
        let before = rendered(&file);
        std::fs::write(&path, b"version two, longer").unwrap();
        let dir = copies("changed");
        let asked = std::cell::Cell::new(0);
        let undo_store = |sha: &str| {
            asked.set(asked.get() + 1);
            (sha == crate::checkpoint::objects::hash_of(b"version one"))
                .then(|| b"version one".to_vec())
        };
        let prints = Fingerprints::default();
        assert_eq!(file.settle(&dir, &prints, &undo_store), Standing::Changed);
        assert_eq!(std::fs::read(file.bytes_at()).unwrap(), b"version one");
        assert_eq!(rendered(&file), before, "the message as cached");
        assert_eq!(file.settle(&dir, &prints, &undo_store), Standing::Changed);
        assert_eq!(asked.get(), 1, "copied once, then read from the copy");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    /// With no copy to be had (changed by someone else, outside Rune), the
    /// attachment says it changed — the same words on every request — and
    /// never hands the model the new content under the old message.
    #[test]
    fn a_changed_attachment_with_nothing_kept_says_so_the_same_way_each_time() {
        let path = write("edited_elsewhere.txt", b"the original");
        let mut file = Attachment::at(&path).unwrap();
        file.seal(1000);
        std::fs::write(&path, b"someone else's edit").unwrap();
        let dir = copies("nothing-kept");
        let prints = Fingerprints::default();
        assert_eq!(file.settle(&dir, &prints, &nothing_kept), Standing::Changed);
        assert_eq!(file.gone, Some(Gone::Changed));
        let once = rendered(&file);
        assert!(once.contains("status=\"changed\""), "{once}");
        assert!(!once.contains("someone else's edit"), "{once}");
        file.settle(&dir, &prints, &nothing_kept);
        assert_eq!(rendered(&file), once);
    }

    /// A file that is gone is said to be gone, at its path; a copy of what was
    /// sent, when there is one, is still what history renders.
    #[test]
    fn a_removed_attachment_is_not_found_unless_a_copy_was_kept() {
        let path = write("removed_later.txt", b"short lived");
        let mut file = Attachment::at(&path).unwrap();
        file.seal(1000);
        std::fs::remove_file(&path).unwrap();
        let dir = copies("removed");
        let prints = Fingerprints::default();
        assert_eq!(file.settle(&dir, &prints, &nothing_kept), Standing::Missing);
        let said = rendered(&file);
        assert!(said.contains("status=\"not_found\""), "{said}");
        assert!(
            said.contains(&format!("No file is at {}", path.display())),
            "{said}"
        );
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(file.sha.as_ref().unwrap()), b"short lived").unwrap();
        assert_eq!(file.settle(&dir, &prints, &nothing_kept), Standing::Missing);
        assert!(rendered(&file).contains("short lived"));
    }

    /// A row an earlier build wrote kept a copy of every attachment under the
    /// same hash: while its file is unchanged it is read from its path now,
    /// and once it changes, the copy it already has is what keeps the cache.
    #[test]
    fn an_older_row_reads_its_path_and_falls_back_to_the_copy_it_kept() {
        let path = write("older_row.txt", b"sent by 0.4.7");
        let sha = crate::checkpoint::objects::hash_of(b"sent by 0.4.7");
        let row = serde_json::json!({
            "name": "older_row.txt",
            "mime": "text/plain",
            "path": path.display().to_string(),
            "size": 13,
            "snapshot": sha,
            "text_chars": 1000
        });
        let mut file: Attachment = serde_json::from_value(row).unwrap();
        assert_eq!(file.sha.as_deref(), Some(sha.as_str()));
        assert_eq!(file.modified, None);
        let dir = copies("older");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(&sha), b"sent by 0.4.7").unwrap();
        let prints = Fingerprints::default();
        assert_eq!(file.settle(&dir, &prints, &nothing_kept), Standing::Same);
        assert_eq!(file.bytes_at(), path.as_path(), "the path, not the copy");
        std::fs::write(&path, b"edited since").unwrap();
        assert_eq!(file.settle(&dir, &prints, &nothing_kept), Standing::Changed);
        assert_eq!(std::fs::read(file.bytes_at()).unwrap(), b"sent by 0.4.7");
        // Stored under the key it always had.
        let stored = serde_json::to_value(&file).unwrap();
        assert_eq!(stored["snapshot"], serde_json::json!(sha));
    }

    /// A hash read back from a stored row names a file inside the copies'
    /// folder or nothing: anything else is read live, never joined as a path.
    #[test]
    fn a_stored_hash_that_is_not_one_is_never_a_path() {
        let path = write("odd_hash.txt", b"content");
        let mut file = Attachment::at(&path).unwrap();
        file.sha = Some("../../etc/passwd".into());
        let dir = copies("odd");
        let prints = Fingerprints::default();
        assert_eq!(file.settle(&dir, &prints, &nothing_kept), Standing::Live);
        assert!(file.source.is_none() && file.gone.is_none());
    }

    #[test]
    fn all_unicode_encodings_and_arbitrary_text_extensions_are_cleanly_decoded() {
        // Plain UTF-8
        assert_eq!(
            decode_text(b"hello world \xf0\x9f\x91\x8d"),
            Some("hello world 👍".into())
        );

        // UTF-8 with BOM
        assert_eq!(
            decode_text(b"\xef\xbb\xbfhello with BOM"),
            Some("hello with BOM".into())
        );

        // UTF-16 LE with BOM
        let utf16_le: Vec<u8> = vec![0xFF, 0xFE, b'H', 0, b'i', 0];
        assert_eq!(decode_text(&utf16_le), Some("Hi".into()));

        // UTF-16 BE with BOM
        let utf16_be: Vec<u8> = vec![0xFE, 0xFF, 0, b'H', 0, b'i'];
        assert_eq!(decode_text(&utf16_be), Some("Hi".into()));

        // Invalid binary
        assert!(decode_text(b"\xff\xff\x00\x01\x02").is_none());

        // Any arbitrary text extension (e.g. .zig, .svelte, .sol, .ini, .proto, Makefile)
        for ext_file in [
            "build.zig",
            "Component.svelte",
            "contract.sol",
            "config.ini",
            "service.proto",
            "Makefile",
        ] {
            let p = std::path::Path::new(ext_file);
            let mime = sniff(b"const x = 42;\n", p);
            assert!(
                is_text_mime(mime),
                "must be text mime for {ext_file}: {mime}"
            );
        }
    }

    /// A sealed attachment renders the same way for the rest of the session:
    /// the bytes are what was sent (from a copy, once the file changed), and
    /// the text budget is the one in force when it was sent — editing the file
    /// or the limit afterwards changes nothing in a message the provider
    /// already cached.
    #[test]
    fn a_sealed_attachment_ignores_later_edits_to_its_file_and_to_the_limit() {
        let path = write("sealed_notes.txt", b"0123456789 the original text");
        let mut file = Attachment::at(&path).unwrap();
        let dir = std::env::temp_dir().join(format!("rune-seal-{}", std::process::id()));
        file.seal(1000);
        assert!(file.sha.is_some() && file.text_chars == Some(1000));

        std::fs::write(&path, b"rewritten afterwards").unwrap();
        let kept = |_: &str| Some(b"0123456789 the original text".to_vec());
        file.settle(&dir, &Fingerprints::default(), &kept);
        let tight = MediaLimits {
            text_chars: 100,
            ..MediaLimits::default()
        };
        // Rendered as history (not the current message), with a live limit far
        // below the file: the recorded budget still inlines the original bytes.
        let mut allowlist = std::collections::HashSet::new();
        allowlist.insert((0, 0));
        let text = |limits: &MediaLimits| -> String {
            parts_for_message(
                std::slice::from_ref(&file),
                "",
                None,
                Dialect::OpenAi,
                limits,
                false,
                0,
                &allowlist,
            )
            .into_iter()
            .map(|part| match part {
                Inlined::Text(t) | Inlined::Notice(t) => t,
                Inlined::Bytes { .. } => String::new(),
            })
            .collect()
        };
        let rendered = text(&tight);
        assert!(rendered.contains("the original text"), "{rendered}");
        assert_eq!(rendered, text(&MediaLimits::default()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_inspect_binary_when_payload_full_prioritizes_active_tool_and_evicts_old_history() {
        let limits = MediaLimits {
            max_payload_bytes: 30 * 1024 * 1024,
            ..MediaLimits::default()
        };
        let budget = provider_payload_budget(&limits);
        let m = model(&["image"]);

        // Turn 1: User previously attached an old 15MB image
        let p_old = write("old_user_photo.png", b"\x89PNG\r\n\x1a\nold_photo");
        let mut a_old = Attachment::at(&p_old).unwrap();
        a_old.size = 15 * 1024 * 1024;
        let mut msg_user = crate::provider::Message::user("Please look at this");
        msg_user.media = vec![a_old.clone()];

        // Turn 2: Assistant called inspect_file, which produced a 10MB image in the active step
        let p_new = write(
            "inspected_screenshot.png",
            b"\x89PNG\r\n\x1a\ninspected_shot",
        );
        let mut a_new = Attachment::at(&p_new).unwrap();
        a_new.size = 10 * 1024 * 1024;
        let mut msg_tool =
            crate::provider::Message::tool_result("call_1", "inspect_file", "Read image");
        msg_tool.media = vec![a_new.clone()];

        let mut messages = vec![msg_user, msg_tool];
        for (i, j) in evictions(&messages, Some(1), budget, max_history_files()) {
            messages[i].media[j].evicted = true;
        }
        let allowlist = history_allowlist(&messages, Some(1));

        // 1. Verify that the older 15MB image in history is EVICTED because 10MB (new) + 15MB (old) = 25MB > 22.3MB budget!
        assert!(
            !allowlist.contains(&(0, 0)),
            "Old 15MB image must NOT be in allowlist"
        );

        // 2. Render the active tool message parts: the newly inspected 10MB image MUST BE INLINED as Bytes!
        let active_parts = parts_for_message(
            &[a_new],
            "",
            Some(&m),
            Dialect::OpenAi,
            &limits,
            true, // is_current = true
            1,
            &allowlist,
        );
        assert_eq!(
            active_parts.len(),
            3,
            "New image must be inlined (open tag, bytes, close tag)"
        );
        assert!(
            matches!(active_parts[1], Inlined::Bytes { .. }),
            "Active inspected image must be inlined as Bytes so AI can see it!"
        );

        // 3. Render the old history message parts: it MUST be degraded to status="skipped_context_budget"
        let old_parts = parts_for_message(
            &[a_old],
            "",
            Some(&m),
            Dialect::OpenAi,
            &limits,
            false, // is_current = false
            0,
            &allowlist,
        );
        match &old_parts[0] {
            Inlined::Text(xml) => {
                assert!(
                    xml.contains("status=\"skipped_context_budget\""),
                    "Old image must degrade to skipped_context_budget: {xml}"
                );
            }
            other => panic!("Expected Inlined::Text for evicted old image, got {other:?}"),
        }
    }

    #[test]
    fn multiple_attachments_in_current_turn_respect_consumed_budget() {
        let limits = MediaLimits {
            max_payload_bytes: 25 * 1024 * 1024, // ~18.6 MB raw budget
            ..MediaLimits::default()
        };
        let m = model(&["image"]);

        let p1 = write("shot1.png", b"\x89PNG\r\n\x1a\nshot1");
        let mut a1 = Attachment::at(&p1).unwrap();
        a1.size = 12 * 1024 * 1024; // 12 MB fits in 18.6 MB

        let p2 = write("shot2.png", b"\x89PNG\r\n\x1a\nshot2");
        let mut a2 = Attachment::at(&p2).unwrap();
        a2.size = 12 * 1024 * 1024; // 12 + 12 = 24 MB > 18.6 MB!

        let empty = std::collections::HashSet::new();
        let parts = parts_for_message(
            &[a1, a2],
            "",
            Some(&m),
            Dialect::OpenAi,
            &limits,
            true,
            0,
            &empty,
        );

        // a1 is inlined as Bytes
        assert!(
            matches!(parts[1], Inlined::Bytes { .. }),
            "First 12MB image must be inlined"
        );

        // a2 exceeds remaining budget (12 + 12 > 18.6MB) -> degraded to status="too_large"
        match &parts[3] {
            Inlined::Text(xml) => {
                assert!(
                    xml.contains("status=\"too_large\""),
                    "Second 12MB image must degrade to too_large: {xml}"
                );
                assert!(
                    xml.contains("payload limit"),
                    "Must mention payload limit: {xml}"
                );
            }
            other => panic!("Expected Inlined::Text for second image, got {other:?}"),
        }
    }
}
