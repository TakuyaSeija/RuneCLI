//! Prompt-cache breakpoints, for the endpoints that cache only where asked.
//!
//! Anthropic's Messages API (and the gateways that pass its blocks through)
//! caches nothing without a `cache_control` marker. Rune keeps every request an
//! append-only extension of the previous one (`session::context`), so two
//! markers are enough: the end of the system prompt, which covers the tools and
//! the system text, and the last block of the newest message, which the next
//! request finds again as its prefix.
//!
//! Whether an endpoint wants markers is protocol/capability truth, never a
//! price or a model-name guess. Native Messages knows its carrier; compatible
//! endpoints opt into their named marker via supported_parameters. Their tool
//! results remain strings, while only portable system/user text carries marks.

use serde_json::{json, Value};

fn marker() -> Value {
    json!({ "type": "ephemeral" })
}

/// The system prompt as a single text block ending in a breakpoint.
pub(crate) fn system(text: &str) -> Value {
    json!([{ "type": "text", "text": text, "cache_control": marker() }])
}

/// Give a plain string body the one-text-block form a breakpoint needs.
///
/// A dialect that marks applies this to every message that can ever be the
/// marked one, not only the newest: a message is marked in the request where it
/// is newest and unmarked in every request after, and if only the marked copy
/// were a block the same message would reach the provider in two shapes — a
/// prefix that no longer matches the one it cached.
pub(crate) fn as_blocks(message: &mut Value) {
    let Some(content) = message.get_mut("content") else {
        return;
    };
    if let Some(text) = content.as_str().filter(|text| !text.is_empty()) {
        *content = json!([{ "type": "text", "text": text }]);
    }
}

/// OpenAI-compatible gateways document cache_control on text content blocks,
/// not inside role:tool or on arbitrary media. Normalize every eligible row
/// before marking so the prefix keeps its shape when the breakpoint moves.
/// Mark the newest eligible text, even when tools follow: those tools are the
/// uncached suffix, preserved exactly, with no fabricated user message.
pub(crate) fn openai(messages: &mut [Value], key: &str, marker: Value) {
    fn text(message: &mut Value) -> Option<&mut Value> {
        if !matches!(message["role"].as_str(), Some("system" | "user")) {
            return None;
        }
        message
            .get_mut("content")?
            .as_array_mut()?
            .iter_mut()
            .rev()
            .find(|block| {
                block["type"] == "text"
                    && block["text"].as_str().is_some_and(|text| !text.is_empty())
            })
    }
    for message in messages
        .iter_mut()
        .filter(|m| matches!(m["role"].as_str(), Some("system" | "user")))
    {
        as_blocks(message);
    }
    if let Some(system) = messages.first_mut().filter(|m| m["role"] == "system") {
        if let Some(block) = text(system) {
            block[key] = marker.clone();
        }
    }
    if let Some(block) = messages.iter_mut().rev().find_map(text) {
        block[key] = marker;
    }
}

/// Put a breakpoint on the last block of an Anthropic message that can carry
/// one. A tool_result wrapper is eligible; its content is never traversed.
pub(crate) fn mark_last(message: &mut Value) {
    as_blocks(message);
    let Some(content) = message.get_mut("content") else {
        return;
    };
    let Some(blocks) = content.as_array_mut() else {
        return;
    };
    if let Some(block) = blocks.iter_mut().rev().find(|block| {
        matches!(
            block.get("type").and_then(Value::as_str),
            Some("text" | "tool_result" | "image" | "image_url" | "document" | "file")
        )
    }) {
        block["cache_control"] = marker();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_string_body_becomes_one_marked_text_block() {
        let mut message = json!({ "role": "user", "content": "hello" });
        mark_last(&mut message);
        assert_eq!(
            message["content"],
            json!([{ "type": "text", "text": "hello", "cache_control": { "type": "ephemeral" } }])
        );
    }

    /// A thinking block cannot carry a marker; the last block that can, does.
    #[test]
    fn the_last_markable_block_is_chosen() {
        let mut message = json!({ "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "a", "content": "x" },
            { "type": "thinking", "thinking": "…" }
        ]});
        mark_last(&mut message);
        assert!(message["content"][0].get("cache_control").is_some());
        assert!(message["content"][1].get("cache_control").is_none());
    }

    #[test]
    fn portable_breakpoints_do_not_enter_tools_or_media_and_leave_the_suffix_intact() {
        for key in ["cache_control", "prompt_cache_breakpoint"] {
            let mut messages = vec![
                json!({"role":"system","content":"sys"}),
                json!({"role":"user","content":[{"type":"text","text":"request"},{"type":"image_url","image_url":{"url":"data:image/png;base64,synthetic"}}]}),
                json!({"role":"assistant","content":"","tool_calls":[]}),
                json!({"role":"tool","tool_call_id":"a","content":"a"}),
                json!({"role":"tool","tool_call_id":"b","content":""}),
                json!({"role":"user","content":[{"type":"image_url","image_url":{"url":"synthetic"}}]}),
            ];
            let suffix = messages[2..].to_vec();
            openai(&mut messages, key, json!({"synthetic":true}));
            assert!(messages[0]["content"][0].get(key).is_some());
            assert!(messages[1]["content"][0].get(key).is_some());
            assert!(messages[1]["content"][1].get(key).is_none());
            assert_eq!(messages[2..], suffix);
            let old = messages.clone();
            messages.push(json!({"role":"user","content":"follow-up"}));
            openai(&mut messages, key, json!({"synthetic":true}));
            for (before, after) in old.iter().zip(&messages) {
                assert_eq!(before["content"].is_array(), after["content"].is_array());
            }
        }
    }

    #[test]
    fn an_empty_body_is_left_alone() {
        let mut message = json!({ "role": "assistant", "content": "" });
        mark_last(&mut message);
        assert_eq!(message["content"], json!(""));
    }
}
