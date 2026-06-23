//! In-process OpenAI Chat Completions mock server for e2e tests.
#![allow(dead_code)]
//!
//! Implements a minimal 2-turn scenario:
//!   - Turn 1: assistant returns a `tool_calls` response (finish_reason="tool_calls")
//!   - Turn 2: assistant returns a final text response (finish_reason="stop")
//!
//! The call counter is tracked with `Arc<AtomicUsize>` so the test can assert
//! exactly 2 HTTP requests were made.

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

/// Shared state passed to the axum handler.
#[derive(Clone)]
pub struct MockState {
    pub call_count: Arc<AtomicUsize>,
}

/// POST /chat/completions handler.
///
/// Turn 1 (first call): returns a tool_calls response so the agent dispatches
/// the `echo` tool.
/// Turn 2+ (subsequent calls): returns a final stop response.
///
/// Parse errors return 400 with an error message instead of panicking.
async fn chat_completions_handler(
    State(state): State<MockState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Validate that the request body is parseable JSON. If not, return 400.
    if let Err(e) = serde_json::from_slice::<serde_json::Value>(&body) {
        eprintln!("[openai_mock] failed to parse request body: {e}");
        let err_body = json!({ "error": format!("bad request: {e}") }).to_string();
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            err_body,
        );
    }

    let prev = state.call_count.fetch_add(1, Ordering::SeqCst);

    let response_json = if prev == 0 {
        // Turn 1: assistant requests tool call
        json!({
            "id": "chatcmpl-mock-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_mock_1",
                        "type": "function",
                        "function": {
                            "name": "echo",
                            "arguments": "{\"message\": \"hello\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        })
    } else {
        // Turn 2+: final response with finish_reason="stop"
        json!({
            "id": "chatcmpl-mock-2",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Tool result received. Task complete.",
                    "tool_calls": null
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 20,
                "completion_tokens": 10,
                "total_tokens": 30
            }
        })
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_json.to_string(),
    )
}

/// Spawn an in-process OpenAI mock server on an ephemeral port.
///
/// Returns `(base_url, call_count, cancellation_token)`.
/// - `base_url`: e.g. `"http://127.0.0.1:PORT"` — pass directly as `base_url` to the fixture.
/// - `call_count`: shared counter; assert `load(SeqCst) == 2` after the test.
/// - `ct`: cancel to shut down the server gracefully.
///
/// Panics if the ephemeral port cannot be bound (test infra failure).
pub async fn spawn_openai_mock_server() -> (String, Arc<AtomicUsize>, CancellationToken) {
    let call_count = Arc::new(AtomicUsize::new(0));
    let ct = CancellationToken::new();

    let state = MockState {
        call_count: call_count.clone(),
    };

    let router = Router::new()
        .route("/chat/completions", post(chat_completions_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port for openai mock");
    let addr = listener.local_addr().expect("local_addr");

    let ct_shutdown = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { ct_shutdown.cancelled_owned().await })
            .await;
    });

    (format!("http://{addr}"), call_count, ct)
}
