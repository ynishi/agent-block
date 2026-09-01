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
/// Turn 1: an fs_edit whose `expect` does not match the file (deliberate
///   mismatch). std.fs rejects it, compile_loop reports zero edits, and the
///   next iteration carries that feedback.
/// Turn 2+: an fs_edit whose `expect` matches, so the file is written.
/// The loop lists its target files in the prompt; the mock edits whichever
/// path it was given rather than hard-coding a temp dir.
///
/// Scans for a path-shaped token anywhere in the body rather than for a line
/// that is one. The prompt introduces the file inline (`Target file: <path>`),
/// so a line-shaped match finds nothing on the first turn — and an empty path
/// is not an inert failure here: `fs_edit` rejects it as `path_not_allowed`,
/// which silently replaces the `expect_mismatch` this mock exists to produce.
fn target_path(body: &axum::body::Bytes) -> String {
    let text = String::from_utf8_lossy(body);
    for token in text.split(|c: char| c.is_whitespace() || c == '"' || c == '\\') {
        if token.starts_with('/') && token.ends_with(".lua") {
            return token.to_string();
        }
    }
    String::new()
}

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

    // The fixture writes `print("hello")` as the only line of the file, so
    // line 1 is the whole address space here.
    let expect_text = if prev == 0 {
        // Turn 1: `WRONG` is not what line 1 contains.
        "print(\"WRONG\")"
    } else {
        "print(\"hello\")"
    };

    // Every turn that carries a tool_result is the model's chance to stop; the
    // loop only needs one edit per iteration, so answering DONE keeps the
    // iteration short.
    let carries_tool_result = String::from_utf8_lossy(&body).contains("tool_result");

    let content = if carries_tool_result {
        json!([{ "type": "text", "text": "DONE" }])
    } else {
        json!([{
            "type": "tool_use",
            "id": format!("toolu_diff_{}", prev + 1),
            "name": "fs_edit",
            "input": {
                "path": target_path(&body),
                "edits": [{
                    "start_line": 1,
                    "end_line": 1,
                    "expect": expect_text,
                    "replace": "print(\"world\")"
                }]
            }
        }])
    };

    let response_json = json!({
        "id": format!("msg_diff_mock_{}", prev + 1),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": "claude-haiku-mock",
        "stop_reason": if carries_tool_result { "end_turn" } else { "tool_use" },
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
