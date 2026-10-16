//! Tool calls being built out of stream fragments.
//!
//! # Why this is not in either provider
//!
//! OpenAI-compatible endpoints and Anthropic disagree about almost everything on
//! the wire — one sends `tool_calls[].function.arguments`, the other sends
//! `content_block_delta.delta.partial_json` — and they agree completely about
//! what is being built: a numbered slot with an id, a name, and JSON text that
//! arrives in pieces. That agreement is this file. Each provider parses its own
//! frames and calls [`Assembly::open`] and [`Assembly::push`]; neither owns a
//! second copy of the accumulator, and a fix to argument handling is a fix for
//! both.
//!
//! Indexed rather than appended because a model may interleave two calls, and
//! the id and name arrive only in the first fragment of each.

use serde_json::Value;

use super::Chunk;
use crate::error::{Error, Result};

pub struct Assembly {
    max_bytes: usize,
    max_calls: usize,
    bytes: usize,
    failure: Option<String>,
    parts: Vec<Part>,
}

impl Default for Assembly {
    fn default() -> Self {
        Self::bounded(&crate::settings::configuration::Configuration::default())
    }
}

struct Part {
    index: u32,
    id: String,
    name: String,
    args: String,
}

impl Assembly {
    pub fn bounded(config: &crate::settings::configuration::Configuration) -> Self {
        Self {
            parts: Vec::new(),
            max_bytes: crate::limits::MAX_TOOL_ARG_BYTES,
            max_calls: config.number("tools.max_actions_per_response") as usize + 1,
            bytes: 0,
            failure: None,
        }
    }

    /// A call begins, or is described further. Both fields are optional because
    /// neither wire format guarantees which fragment carries them.
    pub fn open(&mut self, index: u32, id: Option<&str>, name: Option<&str>) {
        if self.failure.is_some() {
            return;
        }
        if id.is_some_and(|s| s.len() > 256 || s.chars().any(char::is_control))
            || name.is_some_and(|s| s.len() > 256 || s.chars().any(char::is_control))
        {
            self.failure = Some("invalid tool identity".into());
            return;
        }
        if self.parts.len() >= self.max_calls && !self.parts.iter().any(|p| p.index == index) {
            self.failure = Some("too many tool descriptors".into());
            return;
        }
        let slot = self.slot(index);
        if let Some(id) = id {
            slot.id = id.to_string();
        }
        if let Some(name) = name {
            slot.name = name.to_string();
        }
    }

    /// More argument text. Returns the progress event for it, or `None` when the
    /// fragment was empty — there is nothing to show and nothing to count.
    pub fn push(&mut self, index: u32, delta: &str) -> Option<Chunk> {
        if self.failure.is_some() {
            return None;
        }
        self.bytes = self.bytes.saturating_add(delta.len());
        if self.bytes > self.max_bytes
            || (self.parts.len() >= self.max_calls && !self.parts.iter().any(|p| p.index == index))
        {
            self.failure = Some("tool arguments exceed configured limits".into());
            return None;
        }
        if delta.is_empty() {
            return None;
        }
        let slot = self.slot(index);
        slot.args.push_str(delta);
        Some(Chunk::ToolArgs {
            id: slot.id.clone(),
            index,
            name: slot.name.clone(),
            delta: delta.to_string(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// The finished calls, in the order the provider indexed them.
    pub fn finish(mut self) -> Result<Vec<Chunk>> {
        if let Some(why) = self.failure {
            return Err(Error::Provider(why));
        }
        self.parts.sort_by_key(|p| p.index);
        let mut out = Vec::new();
        for part in self.parts {
            if part.name.is_empty() {
                return Err(Error::Provider("unnamed tool descriptor".into()));
            }
            let text = if part.args.trim().is_empty() {
                "{}"
            } else {
                part.args.trim()
            };
            let args: std::result::Result<Value, _> = serde_json::from_str(text);
            let id = if part.id.is_empty() {
                // Rare, and fatal downstream if left empty: every result must be
                // addressable by an id when it is sent back.
                format!("call_{}", part.index)
            } else {
                part.id
            };
            match args {
                Ok(args) => out.push(Chunk::ToolCall { id, name: part.name, args }),
                Err(error) => out.push(Chunk::InvalidToolCall {
                    id,
                    name: part.name,
                    reason: format!("Arguments are not valid JSON: {error}. Re-send a complete JSON object; no operation was executed."),
                }),
            }
        }
        Ok(out)
    }

    fn slot(&mut self, index: u32) -> &mut Part {
        if let Some(at) = self.parts.iter().position(|p| p.index == index) {
            return &mut self.parts[at];
        }
        self.parts.push(Part {
            index,
            id: String::new(),
            name: String::new(),
            args: String::new(),
        });
        self.parts.last_mut().expect("just pushed")
    }
}
