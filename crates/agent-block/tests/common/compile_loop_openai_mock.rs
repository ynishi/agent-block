//! In-process OpenAI Chat Completions mock server for compile_loop e2e tests.
#![allow(dead_code)]
//!
//! Implements a minimal 2-turn scenario for the compile_loop block:
//!   - Turn 1 (prev == 0): assistant returns broken Lua code in a fenced block
//!     (finish_reason="stop"). The mock_runner in the Lua fixture returns
//!     {ok=false} for call 1, triggering a retry.
//!   - Turn 2 (prev >= 1): assistant returns fixed Lua code in a fenced block
//!     (finish_reason="stop"). The mock_runner returns {ok=true}, ending the loop.
//!
//! No `tool_calls` field is present in any response — compile_loop child LLM
//! does not use tools.
//!
//! The HTTP call counter is tracked with `Arc<AtomicUsize>` so the test can
//! assert exactly 2 HTTP requests were made.

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

/// POST /chat/completions handler for compile_loop tests.
///
/// # Purpose
/// Simulates the OpenAI Chat Completions endpoint for the compile_loop block.
/// Returns fenced Lua code blocks without any `tool_calls` fields.
///
/// # Arguments
/// - `state`: shared `MockState` carrying the `Arc<AtomicUsize>` call counter.
/// - `body`: raw request bytes; parsed as JSON for validation.
///
/// # Returns
/// - `400 Bad Request` if the body is not valid JSON.
/// - Turn 1 (prev == 0): `200 OK` with broken Lua code fenced block.
/// - Turn 2+ (prev >= 1): `200 OK` with fixed Lua code fenced block.
///
/// # Errors
/// Returns `400 Bad Request` (not a panic) on JSON parse failure.
async fn chat_completions_handler(
    State(state): State<MockState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Validate that the request body is parseable JSON. Return 400 instead of panicking.
    if let Err(e) = serde_json::from_slice::<serde_json::Value>(&body) {
        eprintln!("[compile_loop_openai_mock] failed to parse request body: {e}");
        let err_body = json!({ "error": format!("bad request: {e}") }).to_string();
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            err_body,
        );
    }

    let prev = state.call_count.fetch_add(1, Ordering::SeqCst);

    let response_json = if prev == 0 {
        // Turn 1: broken Lua code — mock_runner in fixture returns {ok=false},
        // causing compile_loop to retry with a second LLM call.
        json!({
            "id": "chatcmpl-clmock-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "```lua\nprint(\"broken\"; -- syntax err\n```"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        })
    } else {
        // Turn 2+: fixed Lua code — mock_runner in fixture returns {ok=true},
        // ending the compile_loop iteration.
        json!({
            "id": "chatcmpl-clmock-2",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "```lua\nprint(\"fixed\")\n```"
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

/// Spawn an in-process OpenAI mock server on an ephemeral port for compile_loop tests.
///
/// # Purpose
/// Binds a random local port, serves POST `/chat/completions`, and returns
/// the base URL so the Lua fixture can point `compile_loop.make({llm={base_url=...}})` at it.
///
/// # Returns
/// A tuple of:
/// - `base_url`: e.g. `"http://127.0.0.1:PORT"` — pass as `OPENAI_BASE_URL_TEST` to the fixture.
/// - `call_count`: shared `Arc<AtomicUsize>`; assert `load(SeqCst) == 2` after the subprocess.
/// - `ct`: `CancellationToken`; call `ct.cancel()` to shut down the server gracefully.
///
/// # Panics
/// Panics if the ephemeral port cannot be bound (test infrastructure failure).
pub async fn spawn_compile_loop_openai_mock_server() -> (String, Arc<AtomicUsize>, CancellationToken)
{
    let call_count = Arc::new(AtomicUsize::new(0));
    let ct = CancellationToken::new();

    let state = MockState {
        call_count: call_count.clone(),
    };

    let router = Router::new()
        .route("/chat/completions", post(chat_completions_handler))
        .with_state(state);

    // Safety: TcpListener::bind with port 0 requests an OS-assigned ephemeral port.
    // This panics only if the OS refuses to assign any port, which is a fatal test infra failure.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port for compile_loop openai mock");
    let addr = listener.local_addr().expect("local_addr");

    let ct_shutdown = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { ct_shutdown.cancelled_owned().await })
            .await;
    });

    (format!("http://{addr}"), call_count, ct)
}
