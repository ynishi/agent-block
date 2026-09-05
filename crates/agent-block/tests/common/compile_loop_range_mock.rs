//! In-process Anthropic mock for compile_loop's read tools against a file that
//! is too big to send.
#![allow(dead_code)]
//!
//! Three iterations, one model call each — the loop runs one beat per
//! iteration, so the turn is told apart by how many tool results the request
//! carries:
//!
//! - 0 results: `fs_read` on the whole file. It is over the size threshold, so
//!   the loop answers with the file's length and a pointer to
//!   `read_file_range`. (There is no digest and no summarising sub-call any
//!   more; this is what replaced them.)
//! - 1 result: `read_file_range(path, 10, 20)`, which must come back verbatim
//!   and line-numbered whatever the file's size.
//! - 2 results: `fs_edit` on the marker line, after which the verify passes.
//!
//! `MockState.tool_result_texts` records every tool result the model was shown,
//! in order, which is the only place a test can see what a tool handler
//! actually handed it.

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

/// Shared state for the range mock server.
#[derive(Clone)]
pub struct MockState {
    /// Total HTTP requests received.
    pub call_count: Arc<AtomicUsize>,
    /// Every tool result text the mock has seen, in order of first appearance.
    pub tool_result_texts: Arc<Mutex<Vec<String>>>,
}

impl MockState {
    fn new() -> Self {
        Self {
            call_count: Arc::new(AtomicUsize::new(0)),
            tool_result_texts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Snapshot of the recorded tool result texts.
    pub fn tool_result_texts(&self) -> Vec<String> {
        self.tool_result_texts.lock().expect("lock").clone()
    }
}

/// The line the fixture puts the `REPLACE_ME` marker on.
const MARKER_LINE: u64 = 600;
const MARKER_EXPECT: &str = "-- marker: REPLACE_ME";
const MARKER_REPLACE: &str = "-- marker: DONE";

/// The `fs_edit` input that turns the marker line into `DONE`.
fn marker_edit_input(path: &str) -> serde_json::Value {
    json!({
        "path": path,
        "edits": [{
            "start_line": MARKER_LINE,
            "end_line": MARKER_LINE,
            "expect": MARKER_EXPECT,
            "replace": MARKER_REPLACE
        }]
    })
}

/// Tool result texts carried by an Anthropic request, in message order.
fn anthropic_tool_results(body: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(messages) = body.get("messages").and_then(|m| m.as_array()) else {
        return out;
    };
    for msg in messages {
        if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                out.push(
                    b.get("content")
                        .and_then(|c| c.as_str())
                        .unwrap_or_default()
                        .to_string(),
                );
            }
        }
    }
    out
}

/// Record any tool result texts this request carries that we have not seen yet.
fn record_tool_results(state: &MockState, results: &[String]) {
    let mut seen = state.tool_result_texts.lock().expect("lock");
    for (i, text) in results.iter().enumerate() {
        if i >= seen.len() {
            seen.push(text.clone());
        }
    }
}

/// Extract the first absolute target file path from the request body.
///
/// The loop's opening message lists them:
/// ```text
/// Files:
///   /absolute/path/to/file
/// ```
fn extract_first_path(body: &serde_json::Value) -> Option<String> {
    let messages = body.get("messages").and_then(|m| m.as_array())?;
    for msg in messages {
        let content: String = {
            if let Some(s) = msg.get("content").and_then(|c| c.as_str()) {
                s.to_string()
            } else if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
                blocks
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                continue;
            }
        };
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

/// POST /v1/messages handler for the range test.
async fn anthropic_range_handler(
    State(state): State<MockState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let req_value = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[compile_loop_range_mock] bad request body: {e}");
            let err_body = json!({ "error": format!("bad request: {e}") }).to_string();
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                err_body,
            );
        }
    };

    let prev = state.call_count.fetch_add(1, Ordering::SeqCst);

    let results = anthropic_tool_results(&req_value);
    record_tool_results(&state, &results);
    let path = extract_first_path(&req_value).unwrap_or_else(|| "/unknown/path".to_string());

    let response_json = match results.len() {
        0 => json!({
            "id": format!("msg_range_read_{}", prev + 1),
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "toolu_range_read_1",
                "name": "fs_read",
                "input": { "path": path }
            }],
            "model": "claude-haiku-mock",
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 20, "output_tokens": 10 }
        }),
        1 => json!({
            "id": format!("msg_range_slice_{}", prev + 1),
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
            "usage": { "input_tokens": 30, "output_tokens": 15 }
        }),
        2 => json!({
            "id": format!("msg_range_edit_{}", prev + 1),
            "type": "message",
            "role": "assistant",
            "content": [{
                "type": "tool_use",
                "id": "toolu_range_edit_1",
                "name": "fs_edit",
                "input": marker_edit_input(&path)
            }],
            "model": "claude-haiku-mock",
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 40, "output_tokens": 20 }
        }),
        _ => json!({
            "id": format!("msg_range_done_{}", prev + 1),
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": "DONE" }],
            "model": "claude-haiku-mock",
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 50, "output_tokens": 5 }
        }),
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_json.to_string(),
    )
}

/// Spawn the mock on an ephemeral port.
///
/// # Returns
/// - `addr`: `SocketAddr`. Convert to a URL with `format!("http://{addr}")`.
/// - `state`: `call_count` should equal 3 after the subprocess, and
///   `tool_result_texts()` holds what each read handed the model.
///
/// # Panics
/// Panics only on OS-level port bind failure.
pub async fn spawn_range_mock() -> (std::net::SocketAddr, Arc<MockState>) {
    let state = Arc::new(MockState::new());

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
