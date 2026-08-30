//! OpenAI Chat Completions response shapes.
//!
//! The spec (<https://developers.openai.com/api/docs/guides/function-calling>)
//! puts tool calls in `message.tool_calls[]` with `function.arguments` as a
//! JSON **string** and a mandatory `id`. The broken variants below reproduce
//! deviations observed on OpenAI-compatible stacks in the wild.

use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

static RESPONSE_SEQ: AtomicUsize = AtomicUsize::new(0);

fn next_id(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        RESPONSE_SEQ.fetch_add(1, Ordering::Relaxed) + 1
    )
}

/// Spec-conformant tool call: string `arguments`, `id` present.
pub fn tool_call(id: &str, name: &str, arguments_json: &str) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": { "name": name, "arguments": arguments_json }
    })
}

/// Broken-but-observed variant: `arguments` is a JSON **object** and the `id`
/// field is absent. Emitted through OpenAI-compatible endpoints by Ollama's
/// native `/api/chat` shape (which has no `id` at all — see ollama
/// `docs/api.md`), by Gemini's `functionCall.args`, and by some vLLM
/// tool-call parsers.
pub fn tool_call_object_args_no_id(name: &str, arguments: Value) -> Value {
    json!({
        "type": "function",
        "function": { "name": name, "arguments": arguments }
    })
}

/// Broken variant: `arguments` is not valid JSON. Clients must surface a
/// recoverable error rather than dying (a model that emits one malformed
/// payload can be asked to retry).
pub fn tool_call_malformed_args(id: &str, name: &str) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": { "name": name, "arguments": "{not json" }
    })
}

/// Assistant text response (`finish_reason: "stop"`).
pub fn text_response(text: &str) -> Value {
    json!({
        "id": next_id("chatcmpl-testkit"),
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20 }
    })
}

/// Assistant tool-calls response (`finish_reason: "tool_calls"`, `content: null`).
pub fn tool_calls_response(calls: Vec<Value>) -> Value {
    json!({
        "id": next_id("chatcmpl-testkit"),
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": null, "tool_calls": calls },
            "finish_reason": "tool_calls"
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 10, "total_tokens": 20 }
    })
}
