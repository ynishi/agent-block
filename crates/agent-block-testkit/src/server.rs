//! Axum-backed in-process mock LLM server.
//!
//! [`MockLlm`] serves one provider endpoint from a caller-supplied responder
//! closure. Per request it records facts useful for assertions (call count,
//! tool declarations, `tool_call_id`s of returned tool results) into a
//! shared [`MockState`], derives [`RequestFacts`], and returns whatever the
//! responder builds (typically via [`crate::shapes`]).

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio_util::sync::CancellationToken;

/// Which provider wire the mock speaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    /// Anthropic Messages API, served at `POST /v1/messages`.
    Anthropic,
    /// OpenAI Chat Completions, served at `POST /chat/completions`.
    OpenAi,
}

/// Facts derived from one incoming request, handed to the responder.
#[derive(Clone, Debug)]
pub struct RequestFacts {
    /// 0-based index of this call (0 = first call the mock received).
    pub call_index: usize,
    /// Whether the request carries tool results from a previous turn
    /// (Anthropic: `tool_result` content blocks; OpenAI: `role: "tool"` messages).
    pub has_tool_results: bool,
    /// Absolute paths extracted from a `Files:` section in any string message
    /// content (the multi-file lazy-load convention). At most 2, in order.
    pub paths: Vec<String>,
    /// The full parsed request body.
    pub body: Value,
}

/// Shared request-fact recorder. Cheap to clone; all clones share state.
#[derive(Clone, Default)]
pub struct MockState {
    call_count: Arc<AtomicUsize>,
    tools_declared_count: Arc<AtomicUsize>,
    declared_tool_names: Arc<Mutex<Vec<String>>>,
    tool_result_ids: Arc<Mutex<Vec<String>>>,
}

impl MockState {
    /// Total requests served.
    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    /// Requests that carried a `tools` array (of any content).
    pub fn tools_declared_count(&self) -> usize {
        self.tools_declared_count.load(Ordering::SeqCst)
    }

    /// Every tool name declared across all requests (with repetition).
    pub fn declared_tool_names(&self) -> Vec<String> {
        self.declared_tool_names.lock().expect("lock").clone()
    }

    /// How many times `name` was declared across all requests.
    pub fn declared_count_of(&self, name: &str) -> usize {
        self.declared_tool_names
            .lock()
            .expect("lock")
            .iter()
            .filter(|n| n.as_str() == name)
            .count()
    }

    /// The `tool_call_id` / `tool_use_id` of every tool result seen in
    /// requests, in order. A missing id is recorded as an empty string so
    /// callers can assert its absence.
    pub fn tool_result_ids(&self) -> Vec<String> {
        self.tool_result_ids.lock().expect("lock").clone()
    }
}

/// Handle to a spawned mock server.
pub struct MockHandle {
    /// Base URL (`http://127.0.0.1:<port>`); append nothing — the provider
    /// route is already registered under it.
    pub base_url: String,
    /// Shared fact recorder for assertions.
    pub state: MockState,
    /// Cancel to shut the server down gracefully.
    pub ct: CancellationToken,
}

type Responder = Arc<dyn Fn(&RequestFacts) -> Value + Send + Sync>;

/// Builder for an in-process mock LLM server.
pub struct MockLlm {
    provider: Provider,
    responder: Responder,
}

impl MockLlm {
    /// Mock an Anthropic Messages endpoint (`POST /v1/messages`).
    pub fn anthropic(responder: impl Fn(&RequestFacts) -> Value + Send + Sync + 'static) -> Self {
        Self {
            provider: Provider::Anthropic,
            responder: Arc::new(responder),
        }
    }

    /// Mock an OpenAI Chat Completions endpoint (`POST /chat/completions`).
    pub fn openai(responder: impl Fn(&RequestFacts) -> Value + Send + Sync + 'static) -> Self {
        Self {
            provider: Provider::OpenAi,
            responder: Arc::new(responder),
        }
    }

    /// Bind an ephemeral local port and serve until the handle's token is
    /// cancelled.
    ///
    /// # Panics
    /// Panics only on OS-level port bind failure (fatal test infra condition).
    pub async fn spawn(self) -> MockHandle {
        let state = MockState::default();
        let ct = CancellationToken::new();

        let route = match self.provider {
            Provider::Anthropic => "/v1/messages",
            Provider::OpenAi => "/chat/completions",
        };
        let shared = HandlerState {
            provider: self.provider,
            responder: self.responder,
            state: state.clone(),
        };
        let router = Router::new()
            .route(route, post(handle_request))
            .with_state(shared);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port for testkit mock LLM");
        let addr = listener.local_addr().expect("local_addr");

        let ct_shutdown = ct.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router)
                .with_graceful_shutdown(async move { ct_shutdown.cancelled_owned().await })
                .await;
        });

        MockHandle {
            base_url: format!("http://{addr}"),
            state,
            ct,
        }
    }
}

#[derive(Clone)]
struct HandlerState {
    provider: Provider,
    responder: Responder,
    state: MockState,
}

async fn handle_request(
    State(hs): State<HandlerState>,
    body: axum::body::Bytes,
) -> impl IntoResponse {
    let parsed = match serde_json::from_slice::<Value>(&body) {
        Ok(v) => v,
        Err(e) => {
            let err_body = json!({ "error": format!("bad request: {e}") }).to_string();
            return (
                StatusCode::BAD_REQUEST,
                [(header::CONTENT_TYPE, "application/json")],
                err_body,
            );
        }
    };

    let call_index = hs.state.call_count.fetch_add(1, Ordering::SeqCst);
    record_tools(&hs.state, &parsed);
    record_tool_result_ids(&hs.state, hs.provider, &parsed);

    let facts = RequestFacts {
        call_index,
        has_tool_results: has_tool_results(hs.provider, &parsed),
        paths: extract_paths(&parsed),
        body: parsed,
    };

    let response = (hs.responder)(&facts);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        response.to_string(),
    )
}

/// Record whether (and which) tools the request declared. Both wire forms are
/// accepted: Anthropic `{name, ...}` and OpenAI `{function: {name, ...}}`.
fn record_tools(state: &MockState, body: &Value) {
    let Some(tools) = body.get("tools").and_then(|t| t.as_array()) else {
        return;
    };
    state.tools_declared_count.fetch_add(1, Ordering::SeqCst);
    let mut names = state.declared_tool_names.lock().expect("lock");
    for t in tools {
        let name = t.get("name").and_then(|n| n.as_str()).or_else(|| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
        });
        if let Some(name) = name {
            names.push(name.to_string());
        }
    }
}

/// Record the id carried by every tool result in the request (empty string
/// when absent).
fn record_tool_result_ids(state: &MockState, provider: Provider, body: &Value) {
    let Some(messages) = body.get("messages").and_then(|m| m.as_array()) else {
        return;
    };
    let mut ids = state.tool_result_ids.lock().expect("lock");
    for msg in messages {
        match provider {
            Provider::OpenAi => {
                if msg.get("role").and_then(|r| r.as_str()) == Some("tool") {
                    let id = msg
                        .get("tool_call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    ids.push(id.to_string());
                }
            }
            Provider::Anthropic => {
                if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
                    continue;
                }
                let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) else {
                    continue;
                };
                for b in blocks {
                    if b.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                        let id = b.get("tool_use_id").and_then(|v| v.as_str()).unwrap_or("");
                        ids.push(id.to_string());
                    }
                }
            }
        }
    }
}

/// Provider-specific detection of "this request carries tool results".
fn has_tool_results(provider: Provider, body: &Value) -> bool {
    let Some(messages) = body.get("messages").and_then(|m| m.as_array()) else {
        return false;
    };
    messages.iter().any(|msg| match provider {
        Provider::OpenAi => msg.get("role").and_then(|r| r.as_str()) == Some("tool"),
        Provider::Anthropic => {
            msg.get("role").and_then(|r| r.as_str()) == Some("user")
                && msg
                    .get("content")
                    .and_then(|c| c.as_array())
                    .map(|blocks| {
                        blocks
                            .iter()
                            .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_result"))
                    })
                    .unwrap_or(false)
        }
    })
}

/// Extract absolute paths from a `Files:` section in any string message
/// content (the multi-file lazy-load convention: `Files:\n  <abs>\n  <abs>`).
/// Returns at most 2 paths in order of appearance.
fn extract_paths(body: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    let Some(messages) = body.get("messages").and_then(|m| m.as_array()) else {
        return paths;
    };
    for msg in messages {
        let Some(content) = msg.get("content").and_then(|c| c.as_str()) else {
            continue;
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
