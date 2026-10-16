//! A provider that plays back a written script instead of calling a model.
//!
//! It serves both jobs the design called for and is one implementation rather
//! than two, because they are the same machine:
//!
//!   * **mock** — a hand-written fixture that drives the loop through tool
//!     calls, so every path can be exercised with no network and no cost
//!   * **replay** — the same file format, produced from a recorded run, which
//!     is what makes a loop change testable for regressions
//!
//! Recording a real conversation is not implemented: there is nothing to record
//! from yet, and a recorder with no producer would be code that cannot run.
//!
//! ## Fixture format
//!
//! ```json
//! {
//!   "responses": [
//!     { "chunks": [
//!         { "thinking": "I should look at the file" },
//!         { "tool_call": { "name": "inspect_file",
//!                          "args": { "action": "read it", "files": [{ "path": "a.rs" }] } } },
//!         { "usage": { "input": 100, "output": 20 } }
//!     ]},
//!     { "chunks": [ { "text": "It defines main()." } ] }
//!   ]
//! }
//! ```
//!
//! One entry per model call — not per user turn. A turn that uses two tools
//! consumes three responses.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use serde_json::Value;

use super::{Chunk, Flow, Provider, Request};
use crate::error::{Error, Result};

pub struct Scripted {
    name: String,
    responses: Vec<Vec<Chunk>>,
    next: AtomicUsize,
    /// What to do when the script runs out. A fixture that ends is normal; the
    /// loop must not hang waiting for more.
    fallback: String,
    /// The most recent requests, kept so a test can assert on what was actually
    /// sent rather than on what the code that builds it says it sends.
    ///
    /// Bounded, not a log: this runs in a real session when `-script` is used,
    /// and an unbounded transcript of every call is a leak. One by default; a
    /// test that compares rounds asks for more with [`Scripted::keeping`].
    sent: Mutex<VecDeque<Sent>>,
    keep: usize,
}

/// What a request carried: flattened views for asserting on content, and the
/// request itself for asserting on what the wire carries.
#[derive(Debug, Clone)]
pub struct Sent {
    pub system: String,
    pub settings: crate::settings::ModelSettings,
    /// Every message, rendered as `role: content`. Flattened because a test asks
    /// "was the plan in front of the model", not "which struct field held it".
    pub messages: Vec<String>,
    /// The request itself, for a test that needs what the wire carries.
    pub request: Request,
    /// Its wire bodies, built when it was sent: an attachment is read from
    /// its file then, as a live call reads it — never later, when the file
    /// may hold something else.
    bodies: Vec<Value>,
}

impl Sent {
    /// The body this request became on the wire in each dialect Rune speaks —
    /// built when it was sent, by the same code a live call uses, so a test of
    /// the prompt cache compares the bytes a provider would actually key it on.
    pub fn wire(&self) -> Vec<Value> {
        self.bodies.clone()
    }

    fn bodies(request: &Request) -> Vec<Value> {
        ["openai", "anthropic"]
            .into_iter()
            .filter_map(crate::settings::providers::preset)
            .map(|preset| {
                let cfg = preset.build("test-key");
                if preset.name == "anthropic" {
                    super::anthropic::body(request, &cfg)
                } else {
                    super::openai::body(request, &cfg)
                }
            })
            .collect()
    }
}

impl Scripted {
    pub fn from_file(path: &Path) -> Result<Scripted> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Provider(format!("fixture {}: {e}", path.display())))?;
        let value: Value = serde_json::from_str(&text)?;
        let name = format!(
            "scripted:{}",
            path.file_name().unwrap_or_default().to_string_lossy()
        );
        Scripted::from_value(&value, name)
    }

    pub fn from_value(value: &Value, name: String) -> Result<Scripted> {
        let list = value
            .get("responses")
            .and_then(|v| v.as_array())
            .ok_or_else(|| Error::Provider("fixture needs a `responses` array".into()))?;

        let mut responses = Vec::with_capacity(list.len());
        for (i, r) in list.iter().enumerate() {
            let chunks = r
                .get("chunks")
                .and_then(|v| v.as_array())
                .ok_or_else(|| Error::Provider(format!("response {i} needs `chunks`")))?;
            let mut parsed = Vec::with_capacity(chunks.len());
            for c in chunks {
                parsed.push(parse_chunk(c)?);
            }
            responses.push(parsed);
        }

        let fallback = value
            .get("fallback")
            .and_then(|v| v.as_str())
            .unwrap_or("The script has no more responses.")
            .to_string();

        Ok(Scripted {
            name,
            responses,
            next: AtomicUsize::new(0),
            fallback,
            sent: Mutex::default(),
            keep: 1,
        })
    }

    /// Build one directly, for tests that would rather not write a file.
    pub fn new(responses: Vec<Vec<Chunk>>) -> Scripted {
        Scripted {
            name: "scripted".into(),
            responses,
            next: AtomicUsize::new(0),
            fallback: "The script has no more responses.".into(),
            sent: Mutex::default(),
            keep: 1,
        }
    }

    /// Keep the last `n` requests instead of one.
    pub fn keeping(mut self, n: usize) -> Scripted {
        self.keep = n.max(1);
        self
    }

    /// The requests kept, oldest first.
    pub fn sent(&self) -> Vec<Sent> {
        self.sent
            .lock()
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// What the last request carried. `None` before the model has been called.
    pub fn last_sent(&self) -> Option<Sent> {
        self.sent.lock().ok().and_then(|s| s.back().cloned())
    }

    /// How many responses have been consumed. Tests assert on this to prove the
    /// loop called the model exactly as often as it should have.
    pub fn calls(&self) -> usize {
        self.next.load(Ordering::Relaxed)
    }
}

fn parse_chunk(c: &Value) -> Result<Chunk> {
    if let Some(t) = c.get("text").and_then(|v| v.as_str()) {
        return Ok(Chunk::Text(t.to_string()));
    }
    if let Some(t) = c.get("thinking").and_then(|v| v.as_str()) {
        return Ok(Chunk::Thinking(t.to_string()));
    }
    if let Some(tc) = c.get("invalid_tool_call") {
        return Ok(Chunk::InvalidToolCall {
            id: tc["id"]
                .as_str()
                .ok_or_else(|| Error::Provider("invalid_tool_call needs an id".into()))?
                .into(),
            name: tc["name"]
                .as_str()
                .ok_or_else(|| Error::Provider("invalid_tool_call needs a name".into()))?
                .into(),
            reason: tc["reason"]
                .as_str()
                .ok_or_else(|| Error::Provider("invalid_tool_call needs a reason".into()))?
                .into(),
        });
    }
    if let Some(tc) = c.get("tool_call") {
        let name = tc
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| Error::Provider("tool_call needs a name".into()))?;
        let args = tc
            .get("args")
            .cloned()
            .unwrap_or(Value::Object(Default::default()));
        let id = tc
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| format!("call-{name}"));
        return Ok(Chunk::ToolCall {
            id,
            name: name.to_string(),
            args,
        });
    }
    if let Some(u) = c.get("usage") {
        return Ok(Chunk::Usage {
            details: crate::provider::UsageDetails {
                measured: true,
                input_known: u.get("input").is_some(),
                output_known: u.get("output").is_some(),
                native: u.clone(),
                complete: true,
                ..Default::default()
            },
            input: u.get("input").and_then(|v| v.as_u64()).unwrap_or(0),
            output: u.get("output").and_then(|v| v.as_u64()).unwrap_or(0),
            cached: u.get("cached").and_then(|v| v.as_u64()).unwrap_or(0),
        });
    }
    Err(Error::Provider(format!("unrecognised chunk: {c}")))
}

impl Provider for Scripted {
    fn name(&self) -> &str {
        &self.name
    }

    fn stream(&self, req: &Request, sink: &mut dyn FnMut(Chunk) -> Flow) -> Result<()> {
        if let Ok(mut kept) = self.sent.lock() {
            kept.push_back(Sent {
                system: req.system.clone(),
                settings: req.settings.clone(),
                messages: req
                    .messages
                    .iter()
                    .map(|m| format!("{}: {}", m.role.as_str(), m.content))
                    .collect(),
                request: req.clone(),
                bodies: Sent::bodies(req),
            });
            while kept.len() > self.keep {
                kept.pop_front();
            }
        }
        let i = self.next.fetch_add(1, Ordering::Relaxed);
        let chunks = match self.responses.get(i) {
            Some(c) => c.clone(),
            // Running out is an ordinary end, not a failure: the loop gets a
            // final text answer and stops.
            None => vec![Chunk::Text(self.fallback.clone())],
        };
        for c in chunks {
            if sink(c) == Flow::Stop {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req() -> Request {
        Request {
            control: Default::default(),
            system: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            settings: crate::settings::ModelSettings::default(),
        }
    }

    fn collect(p: &Scripted) -> Vec<Chunk> {
        let mut out = Vec::new();
        p.stream(&req(), &mut |c| {
            out.push(c);
            Flow::Continue
        })
        .unwrap();
        out
    }

    #[test]
    fn plays_responses_in_order_one_per_call() {
        let p = Scripted::new(vec![
            vec![Chunk::Text("first".into())],
            vec![Chunk::Text("second".into())],
        ]);
        assert_eq!(collect(&p), vec![Chunk::Text("first".into())]);
        assert_eq!(collect(&p), vec![Chunk::Text("second".into())]);
        assert_eq!(p.calls(), 2);
    }

    #[test]
    fn running_out_ends_with_text_instead_of_hanging() {
        let p = Scripted::new(vec![]);
        match &collect(&p)[0] {
            Chunk::Text(t) => assert!(t.contains("no more responses")),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn stop_from_the_sink_halts_the_stream() {
        let p = Scripted::new(vec![vec![
            Chunk::Text("a".into()),
            Chunk::Text("b".into()),
            Chunk::Text("c".into()),
        ]]);
        let mut seen = 0;
        p.stream(&req(), &mut |_| {
            seen += 1;
            Flow::Stop
        })
        .unwrap();
        assert_eq!(seen, 1);
    }

    #[test]
    fn parses_every_chunk_kind_from_json() {
        let fixture = json!({
            "responses": [{
                "chunks": [
                    { "thinking": "hmm" },
                    { "text": "hello" },
                    { "tool_call": { "id": "t7", "name": "inspect_file", "args": { "action": "a" } } },
                    { "usage": { "input": 10, "output": 3 } }
                ]
            }]
        });
        let p = Scripted::from_value(&fixture, "t".into()).unwrap();
        let chunks = collect(&p);
        assert_eq!(chunks[0], Chunk::Thinking("hmm".into()));
        assert_eq!(chunks[1], Chunk::Text("hello".into()));
        match &chunks[2] {
            Chunk::ToolCall { id, name, args } => {
                assert_eq!((id.as_str(), name.as_str()), ("t7", "inspect_file"));
                assert_eq!(args["action"], "a");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            chunks[3],
            Chunk::Usage {
                input: 10,
                output: 3,
                cached: 0,
                details: crate::provider::UsageDetails {
                    measured: true,
                    input_known: true,
                    output_known: true,
                    native: serde_json::json!({"input":10,"output":3}),
                    complete: true,
                    ..Default::default()
                }
            }
        );
    }

    #[test]
    fn a_tool_call_without_an_id_gets_one() {
        let fixture = json!({
            "responses": [{ "chunks": [{ "tool_call": { "name": "find_files" } }] }]
        });
        let p = Scripted::from_value(&fixture, "t".into()).unwrap();
        match &collect(&p)[0] {
            Chunk::ToolCall { id, .. } => assert!(!id.is_empty()),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn malformed_fixtures_are_rejected_with_a_reason() {
        assert!(Scripted::from_value(&json!({}), "t".into()).is_err());
        assert!(Scripted::from_value(&json!({ "responses": [{}] }), "t".into()).is_err());
        let bad_chunk = json!({ "responses": [{ "chunks": [{ "nonsense": 1 }] }] });
        assert!(Scripted::from_value(&bad_chunk, "t".into()).is_err());
    }
}
