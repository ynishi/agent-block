//! HTTP → bus source adapter: a listener whose every request is an [`Event`].
//!
//! The mesh adapter in `host.rs` turns a relay request into an event with an
//! ack channel and answers with what the Lua handler returned. This is the
//! same shape over HTTP: one listener, every request (any method, any path)
//! becomes an event of one `kind` (`"http"` unless configured), and the
//! handler's return value is the response. Nothing here knows what the
//! paths mean — routing is the handler's, in Lua, which is what keeps the
//! adapter one file and the surface it fronts a Lua concern.
//!
//! # What crosses
//!
//! `payload` is `{ method, path, query?, body }` — `body` parsed as JSON when
//! it is JSON, the raw text otherwise, `null` when empty. `meta` is the
//! configured [`HttpSourceConfig::meta`] with `remote` (the peer address)
//! added. The handler answers `{ status?, body? }` — `status` defaults to
//! 200 and `body` is sent as JSON — or any other value, which is sent as a
//! 200 JSON body. A handler error is a 500 `{ error }`, a handler that did
//! not answer within [`ACK_TIMEOUT`] a 504.
//!
//! # What is refused before it crosses
//!
//! Binding to loopback is not enough on its own: a page in a browser on the
//! same machine can resolve its own name to `127.0.0.1` after the origin
//! check and reach a loopback server that does not look at `Host` (DNS
//! rebinding; Ollama CVE-2024-28224, and the same class in rmcp's streamable
//! HTTP server, GHSA-89vp-x53w-74fx). So the `Host` header must name a
//! loopback name, or the address this listener is bound to, and an `Origin`
//! header, when present, must too — a 403 otherwise. When a bearer token is
//! configured every request must carry it, loopback included: a 401
//! otherwise. Those two checks happen before the request becomes an event,
//! so a refused request never reaches Lua.
//!
//! Remote use is a matter of what the listener is bound to. The default
//! callers pass is loopback; binding wider is their opt-in, and the token is
//! what stands between the wider bind and anyone who can reach the port.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, State};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::Event;

/// How long a request waits for the handler's answer. Mirrors the mesh
/// adapter's bound.
pub const ACK_TIMEOUT: Duration = Duration::from_secs(30);

/// How long shutdown waits for the listener to finish in-flight requests.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The event kind requests are dispatched under when none is configured.
pub const DEFAULT_KIND: &str = "http";

/// What the listener is.
#[derive(Debug, Clone)]
pub struct HttpSourceConfig {
    /// The address to bind, e.g. `127.0.0.1:7788`. Port `0` picks a free
    /// one; the bound address is on the [`HttpSourceHandle`].
    pub bind: String,
    /// The bearer token every request must carry. `None` means no token,
    /// which is only defensible on a bind nothing else can reach.
    pub token: Option<String>,
    /// The `kind` requests are dispatched under.
    pub kind: String,
    /// Carried on every event's `meta` (an object; anything else is treated
    /// as empty). What a handler needs to know that a request cannot tell
    /// it — which store to open, which manager it fronts.
    pub meta: Value,
}

impl HttpSourceConfig {
    /// A listener on `bind`, no token, kind [`DEFAULT_KIND`], empty meta.
    pub fn new(bind: impl Into<String>) -> Self {
        Self {
            bind: bind.into(),
            token: None,
            kind: DEFAULT_KIND.to_string(),
            meta: Value::Object(Default::default()),
        }
    }

    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    pub fn kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = kind.into();
        self
    }

    pub fn meta(mut self, meta: Value) -> Self {
        self.meta = meta;
        self
    }
}

/// A running listener: its bound address, and the means to stop it.
#[derive(Debug)]
pub struct HttpSourceHandle {
    /// Where it is listening — what the caller prints, and the only way to
    /// learn the port when `bind` asked for `0`.
    pub addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
    cancel: CancellationToken,
}

impl HttpSourceHandle {
    /// Stop accepting, let in-flight requests finish (bounded), and join.
    pub async fn shutdown(self) {
        self.cancel.cancel();
        match tokio::time::timeout(SHUTDOWN_TIMEOUT, self.task).await {
            Ok(Ok(())) => tracing::info!("http source: listener shut down"),
            Ok(Err(e)) => tracing::error!(error = %e, "http source: listener task join error"),
            Err(_) => tracing::warn!(
                timeout_secs = SHUTDOWN_TIMEOUT.as_secs(),
                "http source: listener did not stop in time; abandoned"
            ),
        }
    }
}

struct Inner {
    tx: mpsc::Sender<Event>,
    token: Option<String>,
    kind: String,
    meta: Value,
    allowed_hosts: Vec<String>,
}

/// Bind and start serving. Requests become events on `tx`; the handle stops
/// the listener. Fails only when the bind fails.
pub async fn start(
    config: HttpSourceConfig,
    tx: mpsc::Sender<Event>,
) -> Result<HttpSourceHandle, String> {
    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .map_err(|e| format!("http source: bind {}: {e}", config.bind))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("http source: local_addr: {e}"))?;

    let mut allowed_hosts = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    if !addr.ip().is_loopback() {
        allowed_hosts.push(addr.ip().to_string());
    }

    let inner = Arc::new(Inner {
        tx,
        token: config.token,
        kind: config.kind,
        meta: if config.meta.is_object() {
            config.meta
        } else {
            Value::Object(Default::default())
        },
        allowed_hosts,
    });
    let app = Router::new().fallback(handle).with_state(inner);

    let cancel = CancellationToken::new();
    let stop = cancel.clone();
    let task = tokio::spawn(async move {
        let server = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { stop.cancelled().await });
        if let Err(e) = server.await {
            tracing::error!(error = %e, "http source: listener ended with error");
        }
    });
    tracing::info!(%addr, "http source: listening");
    Ok(HttpSourceHandle { addr, task, cancel })
}

/// The host part of a `Host` header value, or of an origin's authority:
/// the port dropped, IPv6 brackets dropped.
pub fn host_only(value: &str) -> String {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return rest[..end].to_string();
        }
    }
    match value.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) => host.to_string(),
        _ => value.to_string(),
    }
}

/// The host part of an `Origin` header value (`scheme://host[:port]`).
fn origin_host(value: &str) -> Option<String> {
    let rest = value.trim().split_once("://")?.1;
    let authority = rest.split('/').next()?;
    Some(host_only(authority))
}

fn reply(status: u16, body: Value) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, axum::Json(body)).into_response()
}

/// What a handler's return value means as a response: `{ status?, body? }`
/// is read as such, anything else is a 200 with that value as the body.
fn answer(value: Value) -> Response {
    if let Some(obj) = value.as_object() {
        if obj.contains_key("status") || obj.contains_key("body") {
            let status = obj
                .get("status")
                .and_then(Value::as_u64)
                .and_then(|s| u16::try_from(s).ok())
                .unwrap_or(200);
            let body = obj.get("body").cloned().unwrap_or(Value::Null);
            return reply(status, body);
        }
    }
    reply(200, value)
}

async fn handle(
    State(inner): State<Arc<Inner>>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(host_only);
    let host_ok = host
        .as_ref()
        .map(|h| inner.allowed_hosts.iter().any(|a| a == h))
        .unwrap_or(false);
    if !host_ok {
        return reply(403, json!({ "error": "host not allowed" }));
    }
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let origin_ok = origin_host(origin)
            .map(|h| inner.allowed_hosts.iter().any(|a| a == &h))
            .unwrap_or(false);
        if !origin_ok {
            return reply(403, json!({ "error": "origin not allowed" }));
        }
    }
    if let Some(expected) = &inner.token {
        let presented = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim);
        if presented != Some(expected.as_str()) {
            return reply(401, json!({ "error": "token required" }));
        }
    }

    let body_value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&body).into_owned()))
    };
    let payload = json!({
        "method": method.as_str(),
        "path": uri.path(),
        "query": uri.query(),
        "body": body_value,
    });
    let mut meta = inner.meta.clone();
    if let Some(obj) = meta.as_object_mut() {
        obj.insert("remote".to_string(), Value::String(remote.to_string()));
    }

    let id = uuid::Uuid::new_v4().to_string();
    let (ack_tx, ack_rx) = oneshot::channel();
    let event = Event {
        kind: inner.kind.clone(),
        id: id.clone(),
        payload,
        meta,
        ack_tx: Some(ack_tx),
    };
    if let Err(e) = inner.tx.send(event).await {
        tracing::error!(error = %e, id = %id, "http source: bus channel closed; rejecting request");
        return reply(503, json!({ "error": "bus channel closed" }));
    }
    match tokio::time::timeout(ACK_TIMEOUT, ack_rx).await {
        Ok(Ok(Ok(value))) => answer(value),
        Ok(Ok(Err(e))) => {
            tracing::error!(id = %id, error = %e, "http source: handler returned error");
            reply(500, json!({ "error": e.to_string() }))
        }
        Ok(Err(e)) => {
            tracing::error!(id = %id, error = %e, "http source: ack receiver dropped");
            reply(500, json!({ "error": "ack dropped" }))
        }
        Err(_) => {
            tracing::error!(id = %id, timeout_secs = ACK_TIMEOUT.as_secs(), "http source: handler timeout");
            reply(504, json!({ "error": "handler timeout" }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_only_drops_a_port_and_ipv6_brackets() {
        assert_eq!(host_only("localhost:7788"), "localhost");
        assert_eq!(host_only("127.0.0.1"), "127.0.0.1");
        assert_eq!(host_only("[::1]:7788"), "::1");
        assert_eq!(host_only("[::1]"), "::1");
        assert_eq!(host_only("evil.example"), "evil.example");
    }

    #[test]
    fn origin_host_reads_the_authority() {
        assert_eq!(
            origin_host("http://localhost:3000").as_deref(),
            Some("localhost")
        );
        assert_eq!(
            origin_host("https://evil.example/x").as_deref(),
            Some("evil.example")
        );
        assert_eq!(origin_host("null"), None);
    }

    /// A listener on an ephemeral loopback port whose one handler echoes the
    /// event back as `{ status = 201, body = { path, method, body, remote? } }`.
    async fn echoing() -> (HttpSourceHandle, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel::<Event>(8);
        let handle = start(HttpSourceConfig::new("127.0.0.1:0").token("t0k3n"), tx)
            .await
            .expect("bind");
        let pump = tokio::spawn(async move {
            while let Some(mut ev) = rx.recv().await {
                let ack = ev.ack_tx.take().expect("ack channel");
                let has_remote = ev.meta.get("remote").is_some();
                let _ = ack.send(Ok(json!({
                    "status": 201,
                    "body": { "echo": ev.payload, "kind": ev.kind, "remote": has_remote },
                })));
            }
        });
        (handle, pump)
    }

    #[tokio::test]
    async fn a_request_with_the_token_becomes_an_event_and_the_answer_the_response() {
        let (handle, pump) = echoing().await;
        let url = format!(
            "http://127.0.0.1:{}/jobs/x/runs?limit=3",
            handle.addr.port()
        );
        let res = reqwest::Client::new()
            .post(&url)
            .bearer_auth("t0k3n")
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"by":"test"}"#)
            .send()
            .await
            .expect("request");
        assert_eq!(res.status().as_u16(), 201);
        let body: Value = res.json().await.expect("json");
        assert_eq!(body["kind"], "http");
        assert_eq!(body["remote"], true);
        assert_eq!(body["echo"]["method"], "POST");
        assert_eq!(body["echo"]["path"], "/jobs/x/runs");
        assert_eq!(body["echo"]["query"], "limit=3");
        assert_eq!(body["echo"]["body"]["by"], "test");
        handle.shutdown().await;
        pump.abort();
    }

    #[tokio::test]
    async fn without_the_token_nothing_reaches_the_bus() {
        let (handle, pump) = echoing().await;
        let url = format!("http://127.0.0.1:{}/jobs", handle.addr.port());
        let res = reqwest::get(&url).await.expect("request");
        assert_eq!(res.status().as_u16(), 401);
        let res = reqwest::Client::new()
            .get(&url)
            .bearer_auth("wrong")
            .send()
            .await
            .expect("request");
        assert_eq!(res.status().as_u16(), 401);
        handle.shutdown().await;
        pump.abort();
    }

    #[tokio::test]
    async fn a_foreign_host_or_origin_is_refused_before_the_token_is_read() {
        let (handle, pump) = echoing().await;
        let url = format!("http://127.0.0.1:{}/jobs", handle.addr.port());
        let client = reqwest::Client::new();
        let res = client
            .get(&url)
            .header(header::HOST, "evil.example")
            .send()
            .await
            .expect("request");
        assert_eq!(res.status().as_u16(), 403);
        let res = client
            .get(&url)
            .bearer_auth("t0k3n")
            .header(header::ORIGIN, "http://evil.example")
            .send()
            .await
            .expect("request");
        assert_eq!(res.status().as_u16(), 403);
        let res = client
            .get(&url)
            .bearer_auth("t0k3n")
            .header(header::ORIGIN, "http://localhost:5173")
            .send()
            .await
            .expect("request");
        assert_eq!(res.status().as_u16(), 201);
        handle.shutdown().await;
        pump.abort();
    }
}
