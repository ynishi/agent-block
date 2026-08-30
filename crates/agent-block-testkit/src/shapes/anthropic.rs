//! Anthropic Messages API response shapes.
//!
//! Tool calls are `type: "tool_use"` content blocks with an `input` **object**
//! and a mandatory `id`; a tool-calling turn ends with
//! `stop_reason: "tool_use"`
//! (<https://platform.claude.com/docs/en/agents-and-tools/tool-use/overview>).

use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

static RESPONSE_SEQ: AtomicUsize = AtomicUsize::new(0);

fn next_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        RESPONSE_SEQ.fetch_add(1, Ordering::Relaxed) + 1
    )
}

/// A `tool_use` content block.
pub fn tool_use(id: &str, name: &str, input: Value) -> Value {
    json!({ "type": "tool_use", "id": id, "name": name, "input": input })
}

/// Assistant text response (`stop_reason: "end_turn"`).
pub fn text_response(text: &str) -> Value {
    json!({
        "id": next_id("msg-testkit"),
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "text", "text": text }],
        "model": "claude-testkit-mock",
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 10, "output_tokens": 10 }
    })
}

/// Assistant tool-use response (`stop_reason: "tool_use"`).
pub fn tool_use_response(blocks: Vec<Value>) -> Value {
    json!({
        "id": next_id("msg-testkit"),
        "type": "message",
        "role": "assistant",
        "content": blocks,
        "model": "claude-testkit-mock",
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 10, "output_tokens": 10 }
    })
}
