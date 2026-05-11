//! In-process OpenAI Chat Completions mock server for compile_loop 3-turn e2e tests.
#![allow(dead_code)]
//!
//! Implements a 3-turn deterministic scenario for the compile_loop block:
//!   - Turn 1 (prev == 0): assistant returns broken Lua code A (syntax err A).
//!     The mock_runner in the Lua fixture returns {ok=false} for call 1,
//!     triggering a retry.
//!   - Turn 2 (prev == 1): assistant returns broken Lua code B (unclosed string).
//!     Different from Turn 1 to avoid triggering is_stagnant early.
//!     The mock_runner returns {ok=false} for call 2, triggering another retry.
//!   - Turn 3 (prev >= 2): assistant returns fixed Lua code.
//!     The mock_runner returns {ok=true}, ending the loop.
//!
//! No `tool_calls` field is present in any response — compile_loop child LLM
//! does not use tools.
//!
//! The HTTP call counter is tracked with `Arc<AtomicUsize>` so the test can
//! assert exactly 3 HTTP requests were made, and reset the counter between
//! deterministic runs (Crux: identical input sequences produce identical
//! tool-call sequences across runs).

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

/// POST /chat/completions handler for compile_loop 3-turn tests.
///
/// # Purpose
/// Simulates the OpenAI Chat Completions endpoint for a 3-turn compile_loop
/// scenario. Returns fenced Lua code blocks without any `tool_calls` fields.
///
/// # Arguments
/// - `state`: shared `MockState` carrying the `Arc<AtomicUsize>` call counter.
/// - `body`: raw request bytes; parsed as JSON for validation.
///
/// # Returns
/// - `400 Bad Request` if the body is not valid JSON.
/// - Turn 1 (prev == 0): `200 OK` with broken Lua code A (syntax err A).
/// - Turn 2 (prev == 1): `200 OK` with broken Lua code B (unclosed string literal).
/// - Turn 3+ (prev >= 2): `200 OK` with fixed Lua code.
///
/// # Note
/// broken1 and broken2 are deliberately different strings so that
/// `is_stagnant` (which fires on repeated identical stderr) does not fire
/// before the 3rd turn is reached.
///
/// # Errors
/// Returns `400 Bad Request` (not a panic) on JSON parse failure.
async fn chat_completions_handler(
    State(state): State<MockState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    // Validate that the request body is parseable JSON. Return 400 instead of panicking.
    if let Err(e) = serde_json::from_slice::<serde_json::Value>(&body) {
        eprintln!("[compile_loop_openai_mock_three_turn] failed to parse request body: {e}");
        let err_body = json!({ "error": format!("bad request: {e}") }).to_string();
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            err_body,
        );
    }

    let prev = state.call_count.fetch_add(1, Ordering::SeqCst);

    let response_json = if prev == 0 {
        // Turn 1: broken Lua code A — mock_runner in fixture returns {ok=false},
        // causing compile_loop to retry with a second LLM call.
        json!({
            "id": "chatcmpl-cl3mock-1",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "```lua\nprint(\"broken\"; -- syntax err A\n```"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        })
    } else if prev == 1 {
        // Turn 2: broken Lua code B (different from A) — mock_runner returns {ok=false}
        // for call 2. A different string prevents is_stagnant from firing early.
        json!({
            "id": "chatcmpl-cl3mock-2",
            "object": "chat.completion",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "```lua\nlocal x = \"unclosed string\n```"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 20,
                "completion_tokens": 8,
                "total_tokens": 28
            }
        })
    } else {
        // Turn 3+: fixed Lua code — mock_runner in fixture returns {ok=true},
        // ending the compile_loop iteration.
        json!({
            "id": "chatcmpl-cl3mock-3",
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
                "prompt_tokens": 30,
                "completion_tokens": 10,
                "total_tokens": 40
            }
        })
    };

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_json.to_string(),
    )
}

/// Spawn an in-process OpenAI mock server on an ephemeral port for compile_loop 3-turn tests.
///
/// # Purpose
/// Binds a random local port, serves POST `/chat/completions` with a 3-turn
/// sequenced response (broken1 / broken2 / fixed), and returns the base URL
/// so the Lua fixture can point `compile_loop.make({llm={base_url=...}})` at it.
///
/// The `call_count` counter can be reset with `call_count.store(0, SeqCst)`
/// between deterministic runs without restarting the server.
///
/// # Returns
/// A tuple of:
/// - `base_url`: e.g. `"http://127.0.0.1:PORT"` — pass as `OPENAI_BASE_URL_TEST` to the fixture.
/// - `call_count`: shared `Arc<AtomicUsize>`; assert `load(SeqCst) == 3` after the subprocess.
///   Reset with `call_count.store(0, SeqCst)` before a second run for deterministic verification.
/// - `ct`: `CancellationToken`; call `ct.cancel()` to shut down the server gracefully.
///
/// # Panics
/// Panics if the ephemeral port cannot be bound (test infrastructure failure).
pub async fn spawn_compile_loop_openai_mock_three_turn_server(
) -> (String, Arc<AtomicUsize>, CancellationToken) {
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
        .expect("bind ephemeral port for compile_loop openai 3-turn mock");
    let addr = listener
        .local_addr()
        .expect("local_addr for compile_loop openai 3-turn mock");

    let ct_shutdown = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { ct_shutdown.cancelled_owned().await })
            .await;
    });

    (format!("http://{addr}"), call_count, ct)
}
