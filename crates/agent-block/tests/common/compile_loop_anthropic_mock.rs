//! In-process Anthropic Messages API mock server for compile_loop e2e tests.
#![allow(dead_code)]
//!
//! Implements a minimal 2-turn scenario for the compile_loop block:
//!   - Turn 1 (prev == 0): assistant returns broken Lua code in a fenced block
//!     (stop_reason="end_turn"). The mock_runner in the Lua fixture returns
//!     {ok=false} for call 1, triggering a retry.
//!   - Turn 2 (prev >= 1): assistant returns fixed Lua code in a fenced block
//!     (stop_reason="end_turn"). The mock_runner returns {ok=true}, ending the loop.
//!
//! The response shape matches the Anthropic Messages API:
//!   `{ "content": [{"type": "text", "text": "..."}], "stop_reason": "end_turn", ... }`
//!
//! `blocks/compile_loop/init.lua:173-186` expects `decoded.content` to be a table
//! (array of objects with `type` and `text` fields) — NOT a string. This mock
//! returns the correct array shape.
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

/// POST /v1/messages handler for compile_loop Anthropic tests.
///
/// # Purpose
/// Simulates the Anthropic Messages endpoint for the compile_loop block.
/// Returns fenced Lua code blocks in the Anthropic `content` array shape.
/// `blocks/compile_loop/init.lua:173-186` decodes this array format.
///
/// # Arguments
/// - `state`: shared `MockState` carrying the `Arc<AtomicUsize>` call counter.
/// - `body`: raw request bytes; parsed as JSON for validation.
///
/// # Returns
/// - `400 Bad Request` if the body is not valid JSON.
/// - Turn 1 (prev == 0): `200 OK` with broken Lua code in `content` array.
/// - Turn 2+ (prev >= 1): `200 OK` with fixed Lua code in `content` array.
///
/// # Errors
/// Returns `400 Bad Request` (not a panic) on JSON parse failure.
async fn messages_handler(
    State(state): State<MockState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Validate that the request body is parseable JSON. Return 400 instead of panicking.
    if let Err(e) = serde_json::from_slice::<serde_json::Value>(&body) {
        eprintln!("[compile_loop_anthropic_mock] failed to parse request body: {e}");
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
            "id": "msg_mock_1",
            "type": "message",
            "role": "assistant",
            "content": [
                {
                    "type": "text",
                    "text": "```lua\nprint(\"broken\"; -- syntax err\n```"
                }
            ],
            "model": "claude-haiku-mock",
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 20
            }
        })
    } else {
        // Turn 2+: fixed Lua code — mock_runner in fixture returns {ok=true},
        // ending the compile_loop iteration.
        json!({
            "id": "msg_mock_2",
            "type": "message",
            "role": "assistant",
            "content": [
                {
                    "type": "text",
                    "text": "```lua\nprint(\"fixed\")\n```"
                }
            ],
            "model": "claude-haiku-mock",
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 20,
                "output_tokens": 10
            }
        })
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_json.to_string(),
    )
}

/// Spawn an in-process Anthropic mock server on an ephemeral port for compile_loop tests.
///
/// # Purpose
/// Binds a random local port, serves POST `/v1/messages`, and returns
/// the base URL so the Lua fixture can point `compile_loop.make({llm={base_url=...}})` at it.
/// The Lua fixture passes this URL as `ANTHROPIC_BASE_URL_TEST`; `blocks/compile_loop/init.lua`
/// appends `"/v1/messages"` to form the full endpoint URL (after ST1 base_url forward fix).
///
/// # Returns
/// A tuple of:
/// - `base_url`: e.g. `"http://127.0.0.1:PORT"` — pass as `ANTHROPIC_BASE_URL_TEST` to the fixture.
/// - `call_count`: shared `Arc<AtomicUsize>`; assert `load(SeqCst) == 2` after the subprocess.
/// - `ct`: `CancellationToken`; call `ct.cancel()` to shut down the server gracefully.
///
/// # Panics
/// Panics if the ephemeral port cannot be bound (test infrastructure failure).
pub async fn spawn_compile_loop_anthropic_mock_server(
) -> (String, Arc<AtomicUsize>, CancellationToken) {
    let call_count = Arc::new(AtomicUsize::new(0));
    let ct = CancellationToken::new();

    let state = MockState {
        call_count: call_count.clone(),
    };

    let router = Router::new()
        .route("/v1/messages", post(messages_handler))
        .with_state(state);

    // Safety: TcpListener::bind with port 0 requests an OS-assigned ephemeral port.
    // This panics only if the OS refuses to assign any port, which is a fatal test infra failure.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port for compile_loop anthropic mock");
    let addr = listener.local_addr().expect("local_addr");

    let ct_shutdown = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { ct_shutdown.cancelled_owned().await })
            .await;
    });

    (format!("http://{addr}"), call_count, ct)
}
