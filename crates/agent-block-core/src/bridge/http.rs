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
//! # Dump sink (`AGENT_BLOCK_LLM_DUMP_DIR`)
//!
//! A request may opt into a byte-level audit trail by passing
//! `dump = "full"` in the opts table.  Which calls are dump-worthy is
//! decided by the calling block (policy); this bridge only provides the
//! mechanics (file naming, redaction, IO).  When `AGENT_BLOCK_LLM_DUMP_DIR`
//! is set, flagged requests append one JSON object per line to a
//! process-scoped `<UTC>-<id>-p<pid>.jsonl` file in that directory.  The sink
//! is best-effort: any failure is logged once and disables dumping for the
//! rest of the process — it never fails the HTTP request.
//!
//! # Security
//!
//! No URL restrictions during development.  The trust boundary is
//! the Lua script author.  A security model will be designed
//! separately before production use.

use mlua::prelude::*;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    let fallback_agent_id = ctx.mesh_agent_id();
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

                // Per-request dump opt-in.  The caller (Lua block) owns the
                // policy; only the literal "full" activates the JSONL sink, and
                // the sink itself is inert unless AGENT_BLOCK_LLM_DUMP_DIR is set.
                let dump_full: bool = opts
                    .as_ref()
                    .and_then(|t| t.get::<Option<String>>("dump").ok().flatten())
                    .is_some_and(|v| v == "full");

                // ── Build request ─────────────────────────────────
                let reqwest_method = method.parse::<reqwest::Method>().map_err(|e| {
                    LuaError::external(format!("invalid HTTP method '{method}': {e}"))
                })?;

                let mut req = client
                    .request(reqwest_method, &url)
                    .timeout(Duration::from_secs(timeout_secs));

                // Effective outbound header set, captured for the dump record:
                // user-provided headers plus the auto-propagated trace family.
                let mut dump_req_headers: Vec<(String, String)> = Vec::new();

                let mut explicit_headers = HashSet::<String>::new();
                if let Some(ref opts_tbl) = opts {
                    if let Some(headers_tbl) = opts_tbl.get::<Option<LuaTable>>("headers")? {
                        for pair in headers_tbl.pairs::<String, String>() {
                            let (k, v) = pair?;
                            explicit_headers.insert(k.to_ascii_lowercase());
                            if dump_full {
                                dump_req_headers.push((k.clone(), v.clone()));
                            }
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
                            if dump_full {
                                dump_req_headers.push((name.to_string(), v.clone()));
                            }
                            req = req.header(name, v);
                        }
                    }
                }

                // Capture the request body before it is moved into the builder.
                let dump_req_body = if dump_full { body.clone() } else { None };

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
                // `dump_sink()` is only resolved for flagged requests, so an
                // unset AGENT_BLOCK_LLM_DUMP_DIR never touches the filesystem.
                if let Some(sink) = dump_full.then(dump_sink).flatten() {
                    let obs_ctx = obs::obs_context(fallback_agent_id.as_deref());
                    sink.write_record(dump_request_record(
                        &obs_ctx,
                        &method,
                        &url,
                        &dump_req_headers,
                        dump_req_body.as_deref(),
                    ));
                }
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

                let mut dump_resp_headers: Vec<(String, String)> = Vec::new();
                let resp_headers = lua.create_table()?;
                for (k, v) in resp.headers() {
                    if let Ok(vs) = v.to_str() {
                        if dump_full {
                            dump_resp_headers.push((k.as_str().to_string(), vs.to_string()));
                        }
                        resp_headers.set(k.as_str(), vs.to_string())?;
                    }
                }

                if stream_mode {
                    // ── SSE streaming mode ────────────────────────
                    // Streamed bodies are not dumped: the chunks are consumed by
                    // the Lua callback, so the record carries `body_skipped`
                    // instead of a body.  Written before the stream is read so a
                    // mid-stream error still leaves the response record behind.
                    if let Some(sink) = dump_full.then(dump_sink).flatten() {
                        let obs_ctx = obs::obs_context(fallback_agent_id.as_deref());
                        sink.write_record(dump_response_record(
                            &obs_ctx,
                            &method,
                            &url,
                            status,
                            &dump_resp_headers,
                            None,
                            Some("sse_stream"),
                        ));
                    }
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

                    // Dump the exact text handed to Lua below — the body is read
                    // once (`resp.bytes()` above) and borrowed here, not re-read.
                    if let Some(sink) = dump_full.then(dump_sink).flatten() {
                        let obs_ctx = obs::obs_context(fallback_agent_id.as_deref());
                        sink.write_record(dump_response_record(
                            &obs_ctx,
                            &method,
                            &url,
                            status,
                            &dump_resp_headers,
                            Some(body_str.as_str()),
                            None,
                        ));
                    }

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

// ── JSONL dump sink ───────────────────────────────────────────────────
//
// Mechanics only: the decision of *which* requests are dumped is made by the
// caller (`dump = "full"` in the opts table).  This half owns file naming,
// header redaction and the append-only IO, and is guaranteed never to fail a
// request: every error path warns once and disables the sink.

/// Header names (compared case-insensitively) whose values never reach the sink.
///
/// Keep this list in sync with the other two copies:
/// `blocks/agent/init.lua` and `blocks/compile_loop/init.lua`
/// (`sanitize_headers_for_dump`).
const REDACTED_HEADERS: [&str; 5] = [
    "x-api-key",
    "authorization",
    "set-cookie",
    "cookie",
    "proxy-authorization",
];

/// Replacement written in place of a redacted header value.
const REDACTED_VALUE: &str = "***REDACTED***";

/// Process-wide append-only JSONL sink.
///
/// `file` is `None` once the sink is poisoned — the first write failure
/// disables dumping for the remaining lifetime of the process.
struct DumpSink {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

impl DumpSink {
    /// Append one JSON record as a single line, flushing immediately.
    ///
    /// Never panics and never propagates an error: a failing sink is closed
    /// and subsequent records are dropped.
    fn write_record(&self, record: serde_json::Value) {
        // Record and newline are emitted by a single `write_all`: splitting them
        // would let a concurrent appender interleave between the two writes.
        let mut line = match serde_json::to_string(&record) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(target: "lua", "llm dump sink: record serialization failed: {e}");
                return;
            }
        };
        line.push('\n');
        // A poisoned mutex still carries a usable handle; a panic elsewhere
        // must not take the HTTP path down with it.
        let mut guard = self.file.lock().unwrap_or_else(|e| e.into_inner());
        let Some(file) = guard.as_mut() else {
            return; // already disabled
        };
        let written = file.write_all(line.as_bytes()).and_then(|()| file.flush());
        if let Err(e) = written {
            tracing::warn!(
                target: "lua",
                "llm dump sink disabled: write to {} failed: {e}",
                self.path.display()
            );
            *guard = None;
        }
    }
}

/// Resolve the process-wide sink, opening the file on first use.
///
/// Returns `None` when `AGENT_BLOCK_LLM_DUMP_DIR` is unset/empty or when the
/// directory or file could not be opened (warned once at resolve time).
fn dump_sink() -> Option<&'static DumpSink> {
    static SINK: OnceLock<Option<DumpSink>> = OnceLock::new();
    SINK.get_or_init(open_dump_sink).as_ref()
}

fn open_dump_sink() -> Option<DumpSink> {
    let dir = std::env::var("AGENT_BLOCK_LLM_DUMP_DIR")
        .ok()
        .filter(|d| !d.is_empty())?;
    let dir = PathBuf::from(dir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            target: "lua",
            "llm dump sink disabled: cannot create {}: {e}",
            dir.display()
        );
        return None;
    }

    // Correlation ID: run id → trace id → process-scoped agent id.
    let id = ["AGENT_BLOCK_RUN_ID", "AGENT_BLOCK_TRACE_ID"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| obs::process_agent_id().to_string());

    // The pid keeps the file name unique: the timestamp has second precision, so
    // two processes started in the same second under a shared AGENT_BLOCK_RUN_ID
    // would otherwise open — and interleave into — the same file.
    let path = dir.join(format!(
        "{}-{}-p{}.jsonl",
        utc_stamp(now_millis() / 1000),
        sanitize_id(&id),
        std::process::id()
    ));
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => Some(DumpSink {
            path,
            file: Mutex::new(Some(f)),
        }),
        Err(e) => {
            tracing::warn!(
                target: "lua",
                "llm dump sink disabled: cannot open {}: {e}",
                path.display()
            );
            None
        }
    }
}

/// Milliseconds since the Unix epoch (0 if the clock is before the epoch).
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Format epoch seconds as `yyyymmddThhmmssZ` (UTC).
///
/// Uses the civil-from-days conversion so no date crate is required.
fn utc_stamp(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let rem = epoch_secs % 86_400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);

    format!("{year:04}{month:02}{day:02}T{hh:02}{mm:02}{ss:02}Z")
}

/// Keep the sink file name a single, safe path component.
fn sanitize_id(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Render header pairs as a JSON object, masking sensitive values.
fn redact_headers(pairs: &[(String, String)]) -> serde_json::Value {
    let mut map = serde_json::Map::with_capacity(pairs.len());
    for (k, v) in pairs {
        let lower = k.to_ascii_lowercase();
        let value = if REDACTED_HEADERS.contains(&lower.as_str()) {
            REDACTED_VALUE.to_string()
        } else {
            v.clone()
        };
        map.insert(k.clone(), serde_json::Value::String(value));
    }
    serde_json::Value::Object(map)
}

fn dump_request_record(
    ctx: &(String, String, String, String),
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
) -> serde_json::Value {
    let mut rec = serde_json::json!({
        "ts": now_millis(),
        "kind": "http_request",
        "trace_id": ctx.0,
        "run_id": ctx.1,
        "agent_id": ctx.2,
        "agent_name": ctx.3,
        "method": method,
        "url": url,
        "headers": redact_headers(headers),
    });
    if let Some(b) = body {
        rec["body"] = serde_json::Value::String(b.to_string());
    }
    rec
}

fn dump_response_record(
    ctx: &(String, String, String, String),
    method: &str,
    url: &str,
    status: u16,
    headers: &[(String, String)],
    body: Option<&str>,
    body_skipped: Option<&str>,
) -> serde_json::Value {
    let mut rec = serde_json::json!({
        "ts": now_millis(),
        "kind": "http_response",
        "trace_id": ctx.0,
        "run_id": ctx.1,
        "agent_id": ctx.2,
        "agent_name": ctx.3,
        "method": method,
        "url": url,
        "status": status,
        "headers": redact_headers(headers),
    });
    if let Some(b) = body {
        rec["body"] = serde_json::Value::String(b.to_string());
    }
    if let Some(reason) = body_skipped {
        rec["body_skipped"] = serde_json::Value::String(reason.to_string());
    }
    rec
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_headers_masks_sensitive_keys_case_insensitively() {
        let pairs = vec![
            ("X-Api-Key".to_string(), "secret-key".to_string()),
            ("Authorization".to_string(), "Bearer tok".to_string()),
            ("set-cookie".to_string(), "sid=abc".to_string()),
            ("Cookie".to_string(), "session=xyz".to_string()),
            ("Proxy-Authorization".to_string(), "Basic pxy".to_string()),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        let got = redact_headers(&pairs);
        assert_eq!(got["X-Api-Key"], REDACTED_VALUE);
        assert_eq!(got["Authorization"], REDACTED_VALUE);
        assert_eq!(got["set-cookie"], REDACTED_VALUE);
        assert_eq!(got["Cookie"], REDACTED_VALUE);
        assert_eq!(got["Proxy-Authorization"], REDACTED_VALUE);
        assert_eq!(got["content-type"], "application/json");
        let rendered = got.to_string();
        assert!(!rendered.contains("secret-key"), "leaked: {rendered}");
        assert!(!rendered.contains("Bearer tok"), "leaked: {rendered}");
        assert!(!rendered.contains("sid=abc"), "leaked: {rendered}");
        assert!(!rendered.contains("session=xyz"), "leaked: {rendered}");
        assert!(!rendered.contains("Basic pxy"), "leaked: {rendered}");
    }

    #[test]
    fn utc_stamp_formats_known_instants() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        // 2021-01-01T00:00:00Z
        assert_eq!(utc_stamp(1_609_459_200), "20210101T000000Z");
        // 2024-02-29T23:59:59Z (leap day)
        assert_eq!(utc_stamp(1_709_251_199), "20240229T235959Z");
    }

    #[test]
    fn sanitize_id_keeps_file_name_single_component() {
        // Separators are neutralised, so the result can never escape the dir.
        assert_eq!(sanitize_id("run/../id"), "run_.._id");
        assert_eq!(sanitize_id("a b:c"), "a_b_c");
        assert_eq!(sanitize_id("run-1_a.b"), "run-1_a.b");
    }

    #[test]
    fn request_record_omits_body_when_absent() {
        let ctx = ("t".into(), "r".into(), "a".into(), "n".into());
        let rec = dump_request_record(&ctx, "GET", "https://example.com", &[], None);
        assert_eq!(rec["kind"], "http_request");
        assert!(rec.get("body").is_none());
    }

    #[test]
    fn response_record_carries_skip_reason_for_streams() {
        let ctx = (String::new(), String::new(), String::new(), String::new());
        let rec = dump_response_record(
            &ctx,
            "POST",
            "https://example.com",
            200,
            &[],
            None,
            Some("sse_stream"),
        );
        assert_eq!(rec["body_skipped"], "sse_stream");
        assert!(rec.get("body").is_none());
    }
}
