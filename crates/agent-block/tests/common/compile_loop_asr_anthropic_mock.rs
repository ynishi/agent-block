//! In-process Anthropic Messages API mock server for compile_loop
//! apply_search_replace (write-channel tool) e2e tests.
#![allow(dead_code)]
//!
//! Provides two mock servers:
//!
//! ## Tool round-trip (2-call, 1-iter):
//!   - Call 1: returns two `apply_search_replace` tool_use blocks (file_a and file_b),
//!     stop_reason "tool_use". Also records whether the request declared the
//!     apply_search_replace tool (tool_mode="auto" contract).
//!   - Call 2 (tool_result blocks present): returns the plain text "DONE",
//!     stop_reason "end_turn". compile_loop must proceed to verify with zero
//!     text SR blocks because the edits were applied via the tool channel.
//!
//! ## tool_mode="none" (1-call):
//!   - Records whether the request carried a "tools" key (it must NOT), then
//!     returns path-header SEARCH/REPLACE text for both files in a single turn.
//!
//! Initial file contents written by the Lua fixtures:
//!   file_a: `print("a-old")\n`
//!   file_b: `print("b-old")\n`
//! After successful apply:
//!   file_a: `print("a-new")\n`
//!   file_b: `print("b-new")\n`

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
    Arc,
};
use tokio_util::sync::CancellationToken;

/// Shared state for the apply_search_replace mock.
#[derive(Clone)]
pub struct AsrMockState {
    /// Total POST /v1/messages calls.
    pub call_count: Arc<AtomicUsize>,
    /// Calls whose request body declared the apply_search_replace tool.
    pub asr_tool_declared_count: Arc<AtomicUsize>,
    /// Calls whose request body carried any "tools" key at all.
    pub tools_declared_count: Arc<AtomicUsize>,
}

/// Extract absolute file paths from the multi-file lazy-load user message
/// (`Files:\n  <abs_path>\n  <abs_path>`). Returns at most 2 paths.
fn extract_paths_from_request(body: &serde_json::Value) -> Vec<String> {
    let mut paths = Vec::new();
    let messages = match body.get("messages").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return paths,
    };
    for msg in messages {
        let content = match msg.get("content").and_then(|c| c.as_str()) {
            Some(c) => c,
            None => continue,
        };
        let mut in_files_section = false;
        for line in content.lines() {
            if line.trim() == "Files:" {
                in_files_section = true;
                continue;
            }
            if in_files_section {
                let trimmed = line.trim();
                if trimmed.starts_with('/') {
                    let p = trimmed.to_string();
                    if !paths.contains(&p) {
                        paths.push(p);
                    }
                    if paths.len() >= 2 {
                        return paths;
                    }
                } else if !trimmed.is_empty() {
                    in_files_section = false;
                }
            }
        }
    }
    paths
}

/// True when any user message carries a tool_result content block.
fn has_tool_results(body: &serde_json::Value) -> bool {
    body.get("messages")
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
        .unwrap_or(false)
}

/// Record tools-declaration facts about the request into the mock state.
fn record_tools_facts(state: &AsrMockState, body: &serde_json::Value) {
    if let Some(tools) = body.get("tools").and_then(|t| t.as_array()) {
        state.tools_declared_count.fetch_add(1, Ordering::SeqCst);
        let has_asr = tools
            .iter()
            .any(|t| t.get("name").and_then(|n| n.as_str()) == Some("apply_search_replace"));
        if has_asr {
            state.asr_tool_declared_count.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// POST /v1/messages handler — apply_search_replace tool round-trip.
///
/// Call 1: two apply_search_replace tool_use blocks (one per file).
/// Call 2: plain "DONE" text (edits already applied via the tool channel).
async fn asr_tool_handler(
    State(state): State<AsrMockState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let req_value = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[compile_loop_asr_anthropic_mock] bad request body: {e}");
            let err_body = json!({ "error": format!("bad request: {e}") }).to_string();
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                err_body,
            );
        }
    };

    let prev = state.call_count.fetch_add(1, Ordering::SeqCst);
    record_tools_facts(&state, &req_value);

    let response_json = if !has_tool_results(&req_value) {
        // Call 1: emit one apply_search_replace tool_use per file.
        let paths = extract_paths_from_request(&req_value);
        let (path_a, path_b) = if paths.len() >= 2 {
            (paths[0].clone(), paths[1].clone())
        } else {
            ("file_a.lua".to_string(), "file_b.lua".to_string())
        };
        json!({
            "id": format!("msg_asr_turn0_{}", prev + 1),
            "type": "message",
            "role": "assistant",
            "content": [
                {
                    "type": "tool_use",
                    "id": "toolu_asr_1",
                    "name": "apply_search_replace",
                    "input": {
                        "path": path_a,
                        "search": "print(\"a-old\")",
                        "replace": "print(\"a-new\")"
                    }
                },
                {
                    "type": "tool_use",
                    "id": "toolu_asr_2",
                    "name": "apply_search_replace",
                    "input": {
                        "path": path_b,
                        "search": "print(\"b-old\")",
                        "replace": "print(\"b-new\")"
                    }
                }
            ],
            "model": "claude-haiku-mock",
            "stop_reason": "tool_use",
            "usage": { "input_tokens": 30, "output_tokens": 25 }
        })
    } else {
        // Call 2: no text SR blocks — the tool channel already applied the edits.
        json!({
            "id": format!("msg_asr_done_{}", prev + 1),
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "text", "text": "DONE" }],
            "model": "claude-haiku-mock",
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 40, "output_tokens": 5 }
        })
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_json.to_string(),
    )
}

/// POST /v1/messages handler — tool_mode="none" contract.
///
/// Records whether "tools" was declared (must not be), then returns path-header
/// SEARCH/REPLACE text for both files in a single turn.
async fn asr_none_handler(
    State(state): State<AsrMockState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let req_value = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[compile_loop_asr_anthropic_mock/none] bad request body: {e}");
            let err_body = json!({ "error": format!("bad request: {e}") }).to_string();
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                err_body,
            );
        }
    };

    let prev = state.call_count.fetch_add(1, Ordering::SeqCst);
    record_tools_facts(&state, &req_value);

    let paths = extract_paths_from_request(&req_value);
    let (path_a, path_b) = if paths.len() >= 2 {
        (paths[0].clone(), paths[1].clone())
    } else {
        ("file_a.lua".to_string(), "file_b.lua".to_string())
    };

    let text = format!(
        "<<< path={path_a} >>>\n<<<<<<< SEARCH\nprint(\"a-old\")\n=======\nprint(\"a-new\")\n>>>>>>> REPLACE\n\n<<< path={path_b} >>>\n<<<<<<< SEARCH\nprint(\"b-old\")\n=======\nprint(\"b-new\")\n>>>>>>> REPLACE"
    );

    let response_json = json!({
        "id": format!("msg_asr_none_{}", prev + 1),
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "text", "text": text }],
        "model": "claude-haiku-mock",
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 20, "output_tokens": 20 }
    });

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_json.to_string(),
    )
}

fn new_state() -> AsrMockState {
    AsrMockState {
        call_count: Arc::new(AtomicUsize::new(0)),
        asr_tool_declared_count: Arc::new(AtomicUsize::new(0)),
        tools_declared_count: Arc::new(AtomicUsize::new(0)),
    }
}

async fn spawn(router: Router, bind_msg: &str) -> (String, CancellationToken) {
    let ct = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|e| panic!("{bind_msg}: {e}"));
    let addr = listener.local_addr().expect("local_addr");
    let ct_shutdown = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { ct_shutdown.cancelled_owned().await })
            .await;
    });
    (format!("http://{addr}"), ct)
}

/// Spawn the apply_search_replace tool round-trip mock.
///
/// # Returns
/// - `base_url`: pass as `ANTHROPIC_BASE_URL_TEST`.
/// - `state`: assert `call_count == 2` and `asr_tool_declared_count >= 1`.
/// - `ct`: call `ct.cancel()` to shut down gracefully.
pub async fn spawn_compile_loop_asr_anthropic_mock_server(
) -> (String, AsrMockState, CancellationToken) {
    let state = new_state();
    let router = Router::new()
        .route("/v1/messages", post(asr_tool_handler))
        .with_state(state.clone());
    let (base_url, ct) = spawn(router, "bind ephemeral port for compile_loop asr mock").await;
    (base_url, state, ct)
}

/// Spawn the tool_mode="none" mock.
///
/// # Returns
/// - `base_url`: pass as `ANTHROPIC_BASE_URL_TEST`.
/// - `state`: assert `tools_declared_count == 0` (no tools were declared).
/// - `ct`: call `ct.cancel()` to shut down gracefully.
pub async fn spawn_compile_loop_tool_mode_none_mock_server(
) -> (String, AsrMockState, CancellationToken) {
    let state = new_state();
    let router = Router::new()
        .route("/v1/messages", post(asr_none_handler))
        .with_state(state.clone());
    let (base_url, ct) = spawn(
        router,
        "bind ephemeral port for compile_loop tool_mode none mock",
    )
    .await;
    (base_url, state, ct)
}
