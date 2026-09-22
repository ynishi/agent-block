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
    routing::{get, post},
    Router,
};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
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

// ============================================================
// The scripted mock — a run's answers, in order
// ============================================================
//
// The fixed mock above serves one two-turn scenario. A loop test needs to say
// what the model answers on each call: `spawn_scripted_openai_mock` takes the
// `message` objects, n-th call answers the n-th of them, and a call past the
// end answers the last one again (so a loop that takes one turn more than the
// script expected does not get a 500 it would report as an llm_call failure).
//
// Beside the chat route it serves the rest of the vLLM-compatible surface a
// `dialect = "vllm"` conf reaches for — `POST /tokenize` (the token count) and
// `GET /v1/models` (the model card's `max_model_len`) — so a conf that names
// the dialect is answered on every route it asks, not only the one it calls
// most.
//
// Every chat request body is kept, in order, so a test can assert on what the
// harness SENT and not only on what it did with the answer: the note a loop
// puts in front of the next turn, and the fields a policy writes onto the
// request, exist nowhere else.
//
// A scripted message may carry a `finish_reason` of its own. Without one the
// handler derives it (a message with `tool_calls` asked for a tool, anything
// else answered), which is the honest reading of a whole reply; naming it is
// how a test scripts a reply that was CUT — `finish_reason = "length"` — which
// no shape of the message itself can express.

/// Shared state for the scripted handler.
#[derive(Clone)]
pub struct ScriptedState {
    pub call_count: Arc<AtomicUsize>,
    pub bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    script: Arc<Vec<serde_json::Value>>,
    model: Arc<String>,
}

/// The token count `POST /tokenize` answers. Fixed and small: what these
/// tests exercise is the loop, not the fold's dropping, so every request
/// fits and `policy.window{ fit }` never has a beat to drop.
const SCRIPTED_TOKEN_COUNT: u64 = 128;

/// The window `GET /v1/models` names for the scripted model.
const SCRIPTED_MAX_MODEL_LEN: u64 = 32768;

/// `POST /chat/completions` — answer the n-th scripted message, recording the
/// body it was sent.
///
/// `finish_reason` is derived from the message unless the script names one: a
/// message carrying `tool_calls` is a turn that asked for a tool, anything
/// else is a turn that answered. One fact, in one place — and a scripted
/// `finish_reason` is the one thing that reading cannot reach, a reply the
/// server cut off.
async fn scripted_chat_handler(
    State(state): State<ScriptedState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let parsed = match serde_json::from_slice::<serde_json::Value>(&body) {
        Ok(value) => value,
        Err(e) => {
            eprintln!("[scripted_openai_mock] failed to parse request body: {e}");
            let err_body = json!({ "error": format!("bad request: {e}") }).to_string();
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                err_body,
            );
        }
    };
    state
        .bodies
        .lock()
        .expect("the recorded bodies are not poisoned")
        .push(parsed);

    let n = state.call_count.fetch_add(1, Ordering::SeqCst);
    let idx = n.min(state.script.len() - 1);
    let mut message = state.script[idx].clone();
    let asked_for_tools = message
        .get("tool_calls")
        .map(|v| !v.is_null())
        .unwrap_or(false);
    // The script's own word for how this reply ended, taken off the message
    // and not sent as part of it: `finish_reason` is the choice's field, not
    // the message's, and a test that scripts a cut reply is saying something
    // about the turn rather than about its content.
    let scripted_finish = message
        .as_object_mut()
        .and_then(|m| m.remove("finish_reason"))
        .and_then(|v| v.as_str().map(str::to_owned));
    let finish_reason = scripted_finish.unwrap_or_else(|| {
        if asked_for_tools {
            "tool_calls"
        } else {
            "stop"
        }
        .to_owned()
    });

    let response_json = json!({
        "id": format!("chatcmpl-scripted-{}", n + 1),
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": message,
            "finish_reason": finish_reason
        }],
        "usage": {
            "prompt_tokens": 10,
            "completion_tokens": 5,
            "total_tokens": 15
        }
    });

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response_json.to_string(),
    )
}

/// `GET /v1/models` — the model card vLLM's window discovery reads.
async fn scripted_models_handler(State(state): State<ScriptedState>) -> impl IntoResponse {
    let body = json!({
        "object": "list",
        "data": [{
            "id": state.model.as_str(),
            "object": "model",
            "max_model_len": SCRIPTED_MAX_MODEL_LEN
        }]
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
}

/// `POST /tokenize` — vLLM's token count for a request.
async fn scripted_tokenize_handler() -> impl IntoResponse {
    let body = json!({ "count": SCRIPTED_TOKEN_COUNT }).to_string();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
}

/// Spawn a scripted OpenAI-compatible mock on an ephemeral port.
///
/// `script` is the `message` object each `/chat/completions` call answers
/// with, in order; the last one answers every call past the end. A message
/// carrying a `finish_reason` string has it lifted onto the choice (and
/// removed from the message), which is how a cut reply is scripted.
///
/// Returns `(base_url, call_count, bodies, cancellation_token)` —
/// `call_count` is the number of chat calls made, which is the number of
/// beats that reached the provider, and `bodies` is every chat request body
/// in the order they arrived.
///
/// Panics on an empty script (there would be nothing to answer with) or if
/// the ephemeral port cannot be bound (test infra failure).
pub async fn spawn_scripted_openai_mock(
    script: Vec<serde_json::Value>,
    model: &str,
) -> (
    String,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<serde_json::Value>>>,
    CancellationToken,
) {
    assert!(
        !script.is_empty(),
        "spawn_scripted_openai_mock: the script must name at least one answer"
    );

    let call_count = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let ct = CancellationToken::new();

    let state = ScriptedState {
        call_count: call_count.clone(),
        bodies: bodies.clone(),
        script: Arc::new(script),
        model: Arc::new(model.to_string()),
    };

    let router = Router::new()
        .route("/chat/completions", post(scripted_chat_handler))
        // `llm_proto` builds the chat URL as `base_url .. "/chat/completions"`,
        // so the first route is the one it calls; the second is here for a
        // caller that passes a base_url already ending in `/v1`.
        .route("/v1/chat/completions", post(scripted_chat_handler))
        .route("/v1/models", get(scripted_models_handler))
        .route("/tokenize", post(scripted_tokenize_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port for scripted openai mock");
    let addr = listener.local_addr().expect("local_addr");

    let ct_shutdown = ct.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { ct_shutdown.cancelled_owned().await })
            .await;
    });

    (format!("http://{addr}"), call_count, bodies, ct)
}
