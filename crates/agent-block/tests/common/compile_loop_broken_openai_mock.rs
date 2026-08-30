//! In-process OpenAI-compatible mock returning a deliberately non-conformant
//! ("broken") tool_calls shape, as observed on OpenAI-compatible stacks in the
//! wild (Ollama native leak-through, Gemini functionCall.args, some vLLM
//! tool-call parsers): `function.arguments` is a JSON *object* instead of a
//! string, and the `id` field is absent.
#![allow(dead_code)]
//!
//! 2-call scenario:
//!   - Call 1: two apply_search_replace tool_calls, object arguments, no ids.
//!     compile_loop must synthesize deterministic ids (call_synth_N) and accept
//!     the object arguments verbatim.
//!   - Call 2 (a role="tool" message present): records whether every role="tool"
//!     message carries a `tool_call_id` starting with "call_synth_", then
//!     returns the plain text "DONE".

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

/// Shared state for the broken-OpenAI mock.
#[derive(Clone)]
pub struct BrokenOaiMockState {
    /// Total POST /chat/completions calls.
    pub call_count: Arc<AtomicUsize>,
    /// role="tool" messages seen whose tool_call_id starts with "call_synth_".
    pub synth_id_tool_msg_count: Arc<AtomicUsize>,
    /// role="tool" messages seen with any other (or missing) tool_call_id.
    pub non_synth_tool_msg_count: Arc<AtomicUsize>,
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

/// POST /chat/completions handler for the broken tool_calls shape scenario.
async fn broken_oai_handler(
    State(state): State<BrokenOaiMockState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let req_value = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("[compile_loop_broken_openai_mock] bad request body: {e}");
            let err_body = json!({ "error": format!("bad request: {e}") }).to_string();
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                err_body,
            );
        }
    };

    let prev = state.call_count.fetch_add(1, Ordering::SeqCst);

    // Record tool_call_id facts about any role="tool" messages in the request.
    let mut has_tool_role = false;
    if let Some(msgs) = req_value.get("messages").and_then(|m| m.as_array()) {
        for msg in msgs {
            if msg.get("role").and_then(|r| r.as_str()) == Some("tool") {
                has_tool_role = true;
                let synth = msg
                    .get("tool_call_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.starts_with("call_synth_"))
                    .unwrap_or(false);
                if synth {
                    state.synth_id_tool_msg_count.fetch_add(1, Ordering::SeqCst);
                } else {
                    state
                        .non_synth_tool_msg_count
                        .fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    }

    let response_json = if !has_tool_role {
        // Call 1: broken shape — object arguments, no id fields.
        let paths = extract_paths_from_request(&req_value);
        let (path_a, path_b) = if paths.len() >= 2 {
            (paths[0].clone(), paths[1].clone())
        } else {
            ("file_a.lua".to_string(), "file_b.lua".to_string())
        };
        json!({
            "id": format!("chatcmpl-broken-turn0-{}", prev + 1),
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "type": "function",
                            "function": {
                                "name": "apply_search_replace",
                                "arguments": {
                                    "path": path_a,
                                    "search": "print(\"a-old\")",
                                    "replace": "print(\"a-new\")"
                                }
                            }
                        },
                        {
                            "type": "function",
                            "function": {
                                "name": "apply_search_replace",
                                "arguments": {
                                    "path": path_b,
                                    "search": "print(\"b-old\")",
                                    "replace": "print(\"b-new\")"
                                }
                            }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": { "prompt_tokens": 30, "completion_tokens": 25, "total_tokens": 55 }
        })
    } else {
        // Call 2: no SR text — the tool channel already applied the edits.
        json!({
            "id": format!("chatcmpl-broken-done-{}", prev + 1),
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "DONE" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 40, "completion_tokens": 5, "total_tokens": 45 }
        })
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_json.to_string(),
    )
}

/// Spawn the broken-OpenAI tool_calls mock.
///
/// # Returns
/// - `base_url`: pass as `OPENAI_BASE_URL_TEST`.
/// - `state`: assert `call_count == 2`, `synth_id_tool_msg_count >= 2`,
///   `non_synth_tool_msg_count == 0`.
/// - `ct`: call `ct.cancel()` to shut down gracefully.
pub async fn spawn_compile_loop_broken_openai_mock_server(
) -> (String, BrokenOaiMockState, CancellationToken) {
    let state = BrokenOaiMockState {
        call_count: Arc::new(AtomicUsize::new(0)),
        synth_id_tool_msg_count: Arc::new(AtomicUsize::new(0)),
        non_synth_tool_msg_count: Arc::new(AtomicUsize::new(0)),
    };
    let ct = CancellationToken::new();

    let router = Router::new()
        .route("/chat/completions", post(broken_oai_handler))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port for compile_loop broken openai mock");
    let addr = listener.local_addr().expect("local_addr");

    let ct_shutdown = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { ct_shutdown.cancelled_owned().await })
            .await;
    });

    (format!("http://{addr}"), state, ct)
}
