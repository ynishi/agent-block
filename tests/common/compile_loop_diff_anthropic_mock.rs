//! In-process Anthropic Messages API mock server for compile_loop diff-mode e2e tests.
#![allow(dead_code)]
//!
//! Implements a 2-turn scenario exercising the SEARCH/REPLACE diff path:
//!   - Turn 1 (prev == 0): returns a SEARCH/REPLACE block whose SEARCH text does NOT match
//!     the current file content. compile_loop detects the apply failure and feeds back
//!     a "block N could not be applied" message, triggering a second LLM call.
//!   - Turn 2 (prev >= 1): returns a correct SEARCH/REPLACE block that matches and
//!     replaces the target text. mock_runner returns {ok=true}, ending the loop.
//!
//! The initial file written by the Lua fixture contains:
//!   `print("hello")\n`
//! Turn 1 SEARCH uses `print("WRONG")` — guaranteed not to match.
//! Turn 2 SEARCH uses `print("hello")` — exact match — REPLACE emits `print("world")`.
//! mock_runner checks for `world` in the executed file output and returns ok=true.

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

/// Shared state for the diff-mode Anthropic mock.
#[derive(Clone)]
pub struct DiffMockState {
    pub call_count: Arc<AtomicUsize>,
}

/// POST /v1/messages handler for the diff-mode compile_loop test.
///
/// Turn 1: emits a SEARCH/REPLACE block with a wrong SEARCH text (deliberate mismatch).
///   compile_loop will detect apply failure and send a failure message back.
/// Turn 2+: emits a correct SEARCH/REPLACE block matching the initial file.
async fn diff_messages_handler(
    State(state): State<DiffMockState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    if let Err(e) = serde_json::from_slice::<serde_json::Value>(&body) {
        eprintln!("[compile_loop_diff_anthropic_mock] bad request body: {e}");
        let err_body = json!({ "error": format!("bad request: {e}") }).to_string();
        return (
            StatusCode::BAD_REQUEST,
            [(header::CONTENT_TYPE, "application/json")],
            err_body,
        );
    }

    let prev = state.call_count.fetch_add(1, Ordering::SeqCst);

    let text = if prev == 0 {
        // Turn 1: deliberate SEARCH mismatch — "WRONG" is not in the file.
        "<<<<<<< SEARCH\nprint(\"WRONG\")\n=======\nprint(\"world\")\n>>>>>>> REPLACE"
    } else {
        // Turn 2+: correct SEARCH matching the initial file content.
        "<<<<<<< SEARCH\nprint(\"hello\")\n=======\nprint(\"world\")\n>>>>>>> REPLACE"
    };

    let response_json = json!({
        "id": format!("msg_diff_mock_{}", prev + 1),
        "type": "message",
        "role": "assistant",
        "content": [
            {
                "type": "text",
                "text": text
            }
        ],
        "model": "claude-haiku-mock",
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 10,
            "output_tokens": 20
        }
    });

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_json.to_string(),
    )
}

/// Spawn an in-process Anthropic mock server for the diff-mode compile_loop tests.
///
/// # Returns
/// - `base_url`: pass as `ANTHROPIC_BASE_URL_TEST` to the fixture.
/// - `call_count`: assert `load(SeqCst) == 2` after the subprocess.
/// - `ct`: call `ct.cancel()` to shut down gracefully.
///
/// # Panics
/// Panics only on OS-level port bind failure (fatal test infra condition).
pub async fn spawn_compile_loop_diff_anthropic_mock_server(
) -> (String, Arc<AtomicUsize>, CancellationToken) {
    let call_count = Arc::new(AtomicUsize::new(0));
    let ct = CancellationToken::new();

    let state = DiffMockState {
        call_count: call_count.clone(),
    };

    let router = Router::new()
        .route("/v1/messages", post(diff_messages_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port for compile_loop diff anthropic mock");
    let addr = listener.local_addr().expect("local_addr");

    let ct_shutdown = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { ct_shutdown.cancelled_owned().await })
            .await;
    });

    (format!("http://{addr}"), call_count, ct)
}
