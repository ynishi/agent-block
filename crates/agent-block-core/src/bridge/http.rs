//! http.* — Async HTTP client bridge.
//!
//! Provides `http.request(url, opts)` as an async Rust function.
//! When called from Lua via `coroutine_eval`, the coroutine yields
//! during the HTTP request and other coroutines can make progress.
//!
//! # Streaming (SSE)
//!
//! When `stream = true`, the response body is read as Server-Sent
//! Events.  Each `data:` line is passed to the `on_data(data_string)`
//! Lua callback.  The `[DONE]` sentinel terminates the stream.
//!
//! # Security
//!
//! No URL restrictions during development.  The trust boundary is
//! the Lua script author.  A security model will be designed
//! separately before production use.

use mlua::prelude::*;
use std::collections::HashSet;
use std::time::Duration;

use crate::host::HostContext;
use agent_block_types::obs;

/// Default request timeout in seconds.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Maximum response body size (10 MiB).  Non-streaming only.
const MAX_BODY_SIZE: usize = 10 * 1024 * 1024;

pub fn register(lua: &Lua, ctx: &HostContext) -> LuaResult<()> {
    let http_tbl = lua.create_table()?;

    let script_name: String = lua
        .globals()
        .get::<Option<String>>("_SCRIPT_NAME")?
        .unwrap_or_else(|| "unknown".to_string());
    let client = ctx.http_client.clone();
    let fallback_agent_id = ctx.mesh_agent.as_ref().map(|a| a.agent_id().to_string());
    http_tbl.set(
        "request",
        lua.create_async_function(move |lua, (url, opts): (String, Option<LuaTable>)| {
            let client = client.clone();
            let fallback_agent_id = fallback_agent_id.clone();
            let script_name = script_name.clone();
            async move {
                // ── Parse options ─────────────────────────────────
                let method = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<String>>("method").ok().flatten())
                    .unwrap_or_else(|| "GET".to_string());

                let timeout_secs: u64 = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<u64>>("timeout").ok().flatten())
                    .unwrap_or(DEFAULT_TIMEOUT_SECS);

                let body: Option<String> = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<String>>("body").ok().flatten());

                let stream_mode: bool = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<bool>>("stream").ok().flatten())
                    .unwrap_or(false);

                let on_data: Option<LuaFunction> = if stream_mode {
                    opts.as_ref()
                        .and_then(|t| t.get::<Option<LuaFunction>>("on_data").ok().flatten())
                } else {
                    None
                };

                // ── Build request ─────────────────────────────────
                let reqwest_method = method.parse::<reqwest::Method>().map_err(|e| {
                    LuaError::external(format!("invalid HTTP method '{method}': {e}"))
                })?;

                let mut req = client
                    .request(reqwest_method, &url)
                    .timeout(Duration::from_secs(timeout_secs));

                let mut explicit_headers = HashSet::<String>::new();
                if let Some(ref opts_tbl) = opts {
                    if let Some(headers_tbl) = opts_tbl.get::<Option<LuaTable>>("headers")? {
                        for pair in headers_tbl.pairs::<String, String>() {
                            let (k, v) = pair?;
                            explicit_headers.insert(k.to_ascii_lowercase());
                            req = req.header(&k, &v);
                        }
                    }
                }

                // Auto-propagate trace context to outbound HTTP requests.
                // User-provided headers always win (no override).
                let trace_headers = [
                    ("x-trace-id", std::env::var("AGENT_BLOCK_TRACE_ID").ok()),
                    ("x-run-id", std::env::var("AGENT_BLOCK_RUN_ID").ok()),
                    (
                        "x-agent-id",
                        std::env::var("AGENT_BLOCK_AGENT_ID")
                            .ok()
                            .or_else(|| fallback_agent_id.clone()),
                    ),
                    ("x-agent-name", std::env::var("AGENT_BLOCK_AGENT_NAME").ok()),
                ];
                for (name, val_opt) in trace_headers {
                    if explicit_headers.contains(name) {
                        continue;
                    }
                    if let Some(v) = val_opt {
                        if !v.is_empty() {
                            req = req.header(name, v);
                        }
                    }
                }

                if let Some(b) = body {
                    req = req.body(b);
                }

                // ── Send (yields here) ────────────────────────────
                tracing::info!(
                    target: "lua",
                    script = %script_name,
                    "{}",
                    obs::obs_line(
                        "http",
                        "http_request",
                        &obs::obs_context(fallback_agent_id.as_deref()),
                        &[("method", method.as_str()), ("url", url.as_str())],
                    )
                );
                let resp = req.send().await.map_err(|e| {
                    if e.is_timeout() {
                        LuaError::external(format!("http timeout after {timeout_secs}s: {e}"))
                    } else if e.is_connect() {
                        LuaError::external(format!("http connection error: {e}"))
                    } else {
                        LuaError::external(format!("http request error: {e}"))
                    }
                })?;

                let status = resp.status().as_u16();
                let status_s = status.to_string();
                tracing::info!(
                    target: "lua",
                    script = %script_name,
                    "{}",
                    obs::obs_line(
                        "http",
                        "http_response",
                        &obs::obs_context(fallback_agent_id.as_deref()),
                        &[("method", method.as_str()), ("url", url.as_str()), ("status", status_s.as_str())],
                    )
                );

                let resp_headers = lua.create_table()?;
                for (k, v) in resp.headers() {
                    if let Ok(vs) = v.to_str() {
                        resp_headers.set(k.as_str(), vs.to_string())?;
                    }
                }

                if stream_mode {
                    // ── SSE streaming mode ────────────────────────
                    read_sse(resp, &on_data).await?;

                    let result = lua.create_table()?;
                    result.set("status", status)?;
                    result.set("headers", resp_headers)?;
                    Ok(result)
                } else {
                    // ── Buffered mode ─────────────────────────────
                    let body_bytes = resp
                        .bytes()
                        .await
                        .map_err(|e| LuaError::external(format!("http read body error: {e}")))?;

                    if body_bytes.len() > MAX_BODY_SIZE {
                        return Err(LuaError::external(format!(
                            "response body too large: {} bytes (max {MAX_BODY_SIZE})",
                            body_bytes.len()
                        )));
                    }

                    let body_str = String::from_utf8_lossy(&body_bytes).to_string();

                    let result = lua.create_table()?;
                    result.set("status", status)?;
                    result.set("headers", resp_headers)?;
                    result.set("body", body_str)?;
                    Ok(result)
                }
            }
        })?,
    )?;

    lua.globals().set("http", http_tbl)?;
    Ok(())
}

/// Read SSE stream and dispatch `data:` lines to the Lua callback.
///
/// SSE format:
/// ```text
/// event: message_start
/// data: {"type":"message_start",...}
///
/// data: {"type":"content_block_delta",...}
///
/// data: [DONE]
/// ```
///
/// Each `data:` value is passed as a string to `on_data`.
/// The `[DONE]` sentinel terminates the stream.
async fn read_sse(mut resp: reqwest::Response, on_data: &Option<LuaFunction>) -> LuaResult<()> {
    let mut buffer = String::new();

    // Read chunks as they arrive (yields between chunks).
    loop {
        let chunk = resp
            .chunk()
            .await
            .map_err(|e| LuaError::external(format!("http stream read error: {e}")))?;

        let chunk = match chunk {
            Some(c) => c,
            None => break, // EOF
        };

        buffer.push_str(&String::from_utf8_lossy(&chunk));

        // Process complete SSE events (delimited by blank lines).
        while let Some(pos) = buffer.find("\n\n") {
            let event_block = buffer[..pos].to_string();
            buffer = buffer[pos + 2..].to_string();

            for line in event_block.lines() {
                if let Some(data) = line
                    .strip_prefix("data: ")
                    .or_else(|| line.strip_prefix("data:"))
                {
                    let data = data.trim();
                    if data == "[DONE]" {
                        return Ok(());
                    }
                    if let Some(ref cb) = on_data {
                        cb.call::<()>(data.to_string())?;
                    }
                }
                // `event:`, `id:`, `retry:` lines are silently skipped.
            }
        }
    }

    Ok(())
}
