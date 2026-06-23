//! In-process mock server for compile_loop distill subloop e2e tests.
//! Supports both Anthropic and OpenAI providers via a runtime `provider` argument.
#![allow(dead_code)]
//!
//! Implements a 3-turn scenario for the compile_loop distill subloop:
//!
//! - Turn 0 (main LLM, with `tools`): returns tool_use=read_file for the target file.
//!   compile_loop dispatches read_file → file size > threshold → calls distill_subloop.
//!
//! - Turn 1 (distill LLM calls, NO `tools` in request): call_distill_llm issues N HTTP
//!   requests (one per chunk). Mock identifies these by absence of `tools` field +
//!   presence of the DISTILL_CHUNK_PROMPT signature string "Summarize the following code chunk".
//!   Returns a short text summary per call. Increments `distill_call_count`.
//!   Stores the last received body in `received_distill_body` for BC5 assertion.
//!
//! - Turn 2 (main LLM, with `tools`, after tool results): returns a correct
//!   SEARCH/REPLACE block that changes "REPLACE_ME" → "DONE" in the target file.
//!   The file path is extracted from the request body (Files: section).
//!
//! MockState fields:
//!   - `call_count`:           total HTTP requests received (all turns combined)
//!   - `distill_call_count`:   HTTP requests identified as distill calls (Turn 1)
//!   - `received_distill_body`: last body received from a distill call (for BC5: tools absent)
//!
//! Shared mock for both providers — router schema is selected by the `provider`
//! argument to `spawn_distill_mock`. No compile-time feature flags.
//!
//! Section layout:
//!   === Shared state ===
//!   === Path extraction helper ===
//!   === Distill detection helper ===
//!   === OpenAI handlers ===
//!   === Anthropic handlers ===
//!   === Spawn helper ===

// === Shared state ===

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

/// Shared state for the distill mock server.
///
/// Clone-safe via Arc wrappers, satisfying axum's `with_state` requirement.
#[derive(Clone)]
pub struct MockState {
    /// Total HTTP requests received (distill calls + main LLM calls combined).
    pub call_count: Arc<AtomicUsize>,
    /// HTTP requests identified as distill calls (no `tools` field in request).
    pub distill_call_count: Arc<AtomicUsize>,
    /// Last request body received from a distill call.
    /// Test side asserts that `tools` key is absent (BC5).
    pub received_distill_body: Arc<Mutex<Option<serde_json::Value>>>,
}

// === Path extraction helper ===

/// Extract the first absolute target file path from a compile_loop request body.
///
/// The multi-file lazy-load initial user message contains:
/// ```text
/// Files:
///   /absolute/path/to/file
/// ```
/// Returns the first path found, or `None` if not present.
fn extract_first_path(body: &serde_json::Value) -> Option<String> {
    let messages = body.get("messages").and_then(|m| m.as_array())?;
    for msg in messages {
        // Collect message content as owned String regardless of schema shape.
        let content: String = {
            if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
                s.to_string()
            } else if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
                // Anthropic content-block array — join text blocks.
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                continue;
            }
        };
        // Parse "Files:\n  /abs/path" section.
        let mut in_files = false;
        for line in content.lines() {
            if line.trim() == "Files:" {
                in_files = true;
                continue;
            }
            if in_files {
                let trimmed = line.trim();
                if trimmed.starts_with('/') {
                    return Some(trimmed.to_string());
                } else if !trimmed.is_empty() {
                    in_files = false;
                }
            }
        }
    }
    None
}

// === Distill detection helper ===

/// Return true when the request body looks like a distill LLM call.
///
/// Criteria (both must hold, per subtask-5.md AC 4):
///   1. `tools` field is absent from the top-level body.
///   2. Any user message content contains the DISTILL_CHUNK_PROMPT signature string.
///
/// The signature string is the first distinctive phrase in `DISTILL_CHUNK_PROMPT`
/// (blocks/compile_loop/init.lua): "You are summarizing a chunk of a source code file".
fn is_distill_call(body: &serde_json::Value) -> bool {
    // Criterion 1: no `tools` key.
    if body.get("tools").is_some() {
        return false;
    }
    // Criterion 2: prompt signature present.
    const DISTILL_SIG: &str = "You are summarizing a chunk of a source code file";
    let messages = match body.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return false,
    };
    for msg in messages {
        if let Some(content) = msg.get("content").and_then(|c| c.as_str()) {
            if content.contains(DISTILL_SIG) {
                return true;
            }
        }
    }
    false
}

// === OpenAI handlers ===

/// POST /chat/completions handler for the distill mock (OpenAI schema).
///
/// Dispatches based on whether the request is a distill call or a main LLM call:
///
/// Distill call (no `tools` + DISTILL_SIG): returns a short text summary.
///   Increments `distill_call_count`, stores body in `received_distill_body`.
///
/// Main call turn 0 (first call with `tools`): returns tool_use=read_file for
///   the target path extracted from the request body.
///
/// Main call turn 1+ (subsequent calls with `tools` and tool results):
///   returns a SEARCH/REPLACE SR block to change "REPLACE_ME" → "DONE".
async fn openai_distill_handler(
    State(state): State<MockState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let req_value = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[compile_loop_distill_mock/openai] bad request body: {e}");
            let err_body = json!({ "error": format!("bad request: {e}") }).to_string();
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                err_body,
            );
        }
    };

    // Increment total call counter.
    let prev = state.call_count.fetch_add(1, Ordering::SeqCst);

    // Check if this is a distill call (no `tools`, has DISTILL_SIG).
    if is_distill_call(&req_value) {
        state.distill_call_count.fetch_add(1, Ordering::SeqCst);
        {
            let mut guard = state.received_distill_body.lock().unwrap();
            *guard = Some(req_value.clone());
        }
        let response_json = json!({
            "id": format!("chatcmpl-distill-{}", prev + 1),
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "This chunk defines utility functions and constants used throughout the module."
                },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 50, "completion_tokens": 20, "total_tokens": 70 }
        });
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            response_json.to_string(),
        );
    }

    // Main LLM call: check how many main (non-distill) calls have been made so far.
    // `prev` counts all calls; distill calls are interleaved. Use presence of tool results
    // in the message list to distinguish turn 0 from turn 1.
    let has_tool_results = req_value
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|msgs| {
            msgs.iter().any(|msg| {
                // OpenAI tool results appear as messages with role="tool".
                msg.get("role").and_then(|r| r.as_str()) == Some("tool")
            })
        })
        .unwrap_or(false);

    let response_json = if !has_tool_results {
        // Turn 0: first main call — return tool_use=read_file.
        let path = extract_first_path(&req_value).unwrap_or_else(|| "/unknown/path".to_string());
        json!({
            "id": format!("chatcmpl-main-turn0-{}", prev + 1),
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_read_file_1",
                        "type": "function",
                        "function": {
                            "name": "read_file",
                            "arguments": format!("{{\"path\":\"{}\"}}", path)
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 30, "completion_tokens": 15, "total_tokens": 45 }
        })
    } else {
        // Turn 1+: return SR pass block.
        let path = extract_first_path(&req_value).unwrap_or_else(|| "/unknown/path".to_string());
        let sr_text = format!(
            "<<< path={path} >>>\n<<<<<<< SEARCH\n-- marker: REPLACE_ME\n=======\n-- marker: DONE\n>>>>>>> REPLACE"
        );
        json!({
            "id": format!("chatcmpl-main-turn1-{}", prev + 1),
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": sr_text
                },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 60, "completion_tokens": 30, "total_tokens": 90 }
        })
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_json.to_string(),
    )
}

// === Anthropic handlers ===

/// POST /v1/messages handler for the distill mock (Anthropic schema).
///
/// Same dispatch logic as `openai_distill_handler`, using Anthropic response shapes.
///
/// Distill call: returns `content` array with a single text block (raw summary).
/// Main call turn 0: returns `tool_use` block for read_file.
/// Main call turn 1+: returns `text` block with the SEARCH/REPLACE SR.
async fn anthropic_distill_handler(
    State(state): State<MockState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let req_value = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[compile_loop_distill_mock/anthropic] bad request body: {e}");
            let err_body = json!({ "error": format!("bad request: {e}") }).to_string();
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                err_body,
            );
        }
    };

    let prev = state.call_count.fetch_add(1, Ordering::SeqCst);

    if is_distill_call(&req_value) {
        state.distill_call_count.fetch_add(1, Ordering::SeqCst);
        {
            let mut guard = state.received_distill_body.lock().unwrap();
            *guard = Some(req_value.clone());
        }
        let response_json = json!({
            "id": format!("msg_distill_{}", prev + 1),
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "text",
                "text": "This chunk defines utility functions and constants used throughout the module."
            }],
            "model": "claude-haiku-mock",
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 50, "output_tokens": 20 }
        });
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            response_json.to_string(),
        );
    }

    // Main LLM call: detect turn by presence of tool_result content blocks.
    let has_tool_results = req_value
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|msgs| {
            msgs.iter().any(|msg| {
                // Anthropic tool results: user message with content array containing tool_result blocks.
                if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
                    return false;
                }
                msg.get("content")
                    .and_then(|c| c.as_array())
                    .map(|blocks| {
                        blocks
                            .iter()
                            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    let response_json = if !has_tool_results {
        // Turn 0: return tool_use=read_file.
        let path = extract_first_path(&req_value).unwrap_or_else(|| "/unknown/path".to_string());
        json!({
            "id": format!("msg_main_turn0_{}", prev + 1),
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "toolu_read_file_1",
                "name": "read_file",
                "input": { "path": path }
            }],
            "model": "claude-haiku-mock",
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 30, "output_tokens": 15 }
        })
    } else {
        // Turn 1+: return SR pass block.
        let path = extract_first_path(&req_value).unwrap_or_else(|| "/unknown/path".to_string());
        let sr_text = format!(
            "<<< path={path} >>>\n<<<<<<< SEARCH\n-- marker: REPLACE_ME\n=======\n-- marker: DONE\n>>>>>>> REPLACE"
        );
        json!({
            "id": format!("msg_main_turn1_{}", prev + 1),
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": sr_text }],
            "model": "claude-haiku-mock",
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 60, "output_tokens": 30 }
        })
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_json.to_string(),
    )
}

// === Range mock handler (Anthropic) ===

/// POST /v1/messages handler for the read_file_range verbatim test (Anthropic schema).
///
/// 2-turn scenario:
///   Turn 0 (no tool_results in messages): returns tool_use=read_file_range(path, 10, 20).
///   Turn 1 (tool_results present):        returns SR pass block (REPLACE_ME → DONE).
///
/// This confirms that read_file_range is dispatched by the tool loop and returns verbatim
/// lines regardless of file size (crux-card §3).
async fn anthropic_range_handler(
    State(state): State<MockState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let req_value = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[compile_loop_distill_mock/range] bad request body: {e}");
            let err_body = json!({ "error": format!("bad request: {e}") }).to_string();
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                err_body,
            );
        }
    };

    let prev = state.call_count.fetch_add(1, Ordering::SeqCst);

    // Detect tool_result presence to determine turn.
    let has_tool_results = req_value
        .get("messages")
        .and_then(|m| m.as_array())
        .map(|msgs| {
            msgs.iter().any(|msg| {
                if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
                    return false;
                }
                msg.get("content")
                    .and_then(|c| c.as_array())
                    .map(|blocks| {
                        blocks
                            .iter()
                            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                    })
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    let response_json = if !has_tool_results {
        // Turn 0: request read_file_range(path, 10, 20).
        let path = extract_first_path(&req_value).unwrap_or_else(|| "/unknown/path".to_string());
        json!({
            "id": format!("msg_range_turn0_{}", prev + 1),
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "toolu_range_1",
                "name": "read_file_range",
                "input": { "path": path, "line_start": 10, "line_end": 20 }
            }],
            "model": "claude-haiku-mock",
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 20, "output_tokens": 10 }
        })
    } else {
        // Turn 1: return SR pass block (REPLACE_ME → DONE).
        let path = extract_first_path(&req_value).unwrap_or_else(|| "/unknown/path".to_string());
        let sr_text = format!(
            "<<< path={path} >>>\n<<<<<<< SEARCH\n-- marker: REPLACE_ME\n=======\n-- marker: DONE\n>>>>>>> REPLACE"
        );
        json!({
            "id": format!("msg_range_turn1_{}", prev + 1),
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": sr_text }],
            "model": "claude-haiku-mock",
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 40, "output_tokens": 20 }
        })
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_json.to_string(),
    )
}

/// Spawn an in-process Anthropic mock for the read_file_range verbatim e2e test.
///
/// 2-turn scenario:
///   Turn 0: returns tool_use=read_file_range(path, 10, 20).
///   Turn 1: returns SR block (REPLACE_ME → DONE) after receiving the tool result.
///
/// # Returns
/// - `addr`: `SocketAddr`. Convert to URL with `format!("http://{addr}")`.
/// - `state`: `Arc<MockState>` — `call_count` should equal 2 after the subprocess.
///
/// # Panics
/// Panics only on OS-level port bind failure.
pub async fn spawn_range_mock() -> (std::net::SocketAddr, Arc<MockState>) {
    let state = Arc::new(MockState {
        call_count: Arc::new(AtomicUsize::new(0)),
        distill_call_count: Arc::new(AtomicUsize::new(0)),
        received_distill_body: Arc::new(Mutex::new(None)),
    });

    let router = Router::new()
        .route("/v1/messages", post(anthropic_range_handler))
        .with_state((*state).clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port for compile_loop range mock");
    let addr = listener.local_addr().expect("local_addr");

    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    (addr, state)
}

// === Spawn helper ===

/// Spawn an in-process distill mock server on an ephemeral port.
///
/// # Arguments
/// - `provider`: `"openai"` or `"anthropic"`. Selects the router schema at runtime.
///   OpenAI: serves `POST /chat/completions`.
///   Anthropic: serves `POST /v1/messages`.
///
/// # Returns
/// - `addr`: `SocketAddr` of the bound port. Convert to URL via `format!("http://{addr}")`.
/// - `state`: `Arc<MockState>` — access `distill_call_count` and `received_distill_body`
///   from the test after the subprocess finishes.
///
/// The mock identifies distill calls (from `call_distill_llm` inside `distill_subloop`)
/// by the absence of a `tools` field in the request body plus the DISTILL_CHUNK_PROMPT
/// signature string. Main LLM calls carry `tools` (the read_file/read_file_range spec).
///
/// # Panics
/// Panics only on OS-level port bind failure (fatal test infrastructure condition).
pub async fn spawn_distill_mock(provider: &str) -> (std::net::SocketAddr, Arc<MockState>) {
    let state = Arc::new(MockState {
        call_count: Arc::new(AtomicUsize::new(0)),
        distill_call_count: Arc::new(AtomicUsize::new(0)),
        received_distill_body: Arc::new(Mutex::new(None)),
    });

    let router = match provider {
        "anthropic" => Router::new()
            .route("/v1/messages", post(anthropic_distill_handler))
            .with_state((*state).clone()),
        _ => {
            // Default to OpenAI-compatible (also covers "openai" explicitly).
            Router::new()
                .route("/chat/completions", post(openai_distill_handler))
                .with_state((*state).clone())
        }
    };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port for compile_loop distill mock");
    let addr = listener.local_addr().expect("local_addr");

    // Serve with no graceful shutdown — the server runs until the test process exits.
    // Ephemeral port is released automatically on process termination.
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    (addr, state)
}
