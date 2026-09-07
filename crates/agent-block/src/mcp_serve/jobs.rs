//! The job manager's five verbs as MCP tools: a thin client of the HTTP
//! listener `agent-block serve` answers on (`crate::serve`).
//!
//! Nothing is decided here. Each tool is one route — the same route `curl`
//! would take — sent to the manager over loopback with the bearer token it
//! minted, and the manager's JSON is the tool result. The MCP server does
//! not start the manager, retry, or stand in for it: when `serve` is not
//! running the tool says so, and starting it is the operator's (it is the
//! one unit a service manager holds).
//!
//! | tool | route |
//! |---|---|
//! | `jobs_list` | `GET /jobs` |
//! | `runs_list` | `GET /runs?job=&limit=` |
//! | `run_get` | `GET /runs/<id>` |
//! | `job_run` | `POST /jobs/<name>/runs` |
//! | `run_stop` | `DELETE /runs/<id>` |
//!
//! The token is read from `$AGENT_BLOCK_HOME/serve.token` on every call
//! rather than once at start, so an MCP server that came up before the
//! manager ever ran still works once it has.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::model::{CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Tool};
use rmcp::ErrorData as McpError;
use serde_json::{json, Map, Value};

/// Where the manager listens unless `--serve-url` says otherwise; the same
/// default `agent-block serve --bind` has.
pub const DEFAULT_SERVE_URL: &str = "http://127.0.0.1:7788";

/// The tool names, in the order they are listed; what the spec pins.
#[cfg(test)]
const TOOL_NAMES: [&str; 5] = ["jobs_list", "runs_list", "run_get", "job_run", "run_stop"];

/// A client of one manager.
#[derive(Clone, Debug)]
pub struct JobsClient {
    url: String,
    http: reqwest::Client,
}

impl JobsClient {
    pub fn new(url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            url: url.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    /// Where the token is: `$AGENT_BLOCK_HOME/serve.token`.
    fn token_path() -> Result<PathBuf, String> {
        agent_block_core::bridge::config::base_dir().map(|home| home.join("serve.token"))
    }

    fn token() -> Result<String, String> {
        let path = Self::token_path()?;
        let text = std::fs::read_to_string(&path).map_err(|e| {
            format!(
                "no token at {} ({e}); `agent-block serve` mints one on its first start — is it running?",
                path.display()
            )
        })?;
        let token = text.trim().to_string();
        if token.is_empty() {
            return Err(format!("the token at {} is empty", path.display()));
        }
        Ok(token)
    }

    /// One request to the manager: the status and the body it answered, or
    /// why it could not be asked.
    async fn call(&self, method: reqwest::Method, path: &str) -> Result<(u16, Value), String> {
        let token = Self::token()?;
        let url = format!("{}{path}", self.url);
        let res = self
            .http
            .request(method, &url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| {
                format!(
                    "agent-block serve is not reachable at {} ({e}); start it with `agent-block serve`",
                    self.url
                )
            })?;
        let status = res.status().as_u16();
        let text = res
            .text()
            .await
            .map_err(|e| format!("reading the answer: {e}"))?;
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Ok((status, body))
    }

    /// The tools this client answers, with their schemas.
    pub fn tools(&self) -> Vec<Tool> {
        fn schema(properties: Value, required: &[&str]) -> Arc<Map<String, Value>> {
            let s = json!({ "type": "object", "properties": properties, "required": required });
            Arc::new(s.as_object().cloned().unwrap_or_default())
        }
        let job = json!({ "type": "string", "description": "A job name, as listed by jobs_list" });
        let run_id = json!({ "type": "string", "description": "A run id, as listed by runs_list" });
        vec![
            Tool::new(
                "jobs_list",
                "The jobs `agent-block serve` manages: each with its interval, timeout, last end \
                 and live run. A job is a block with a `job.toml` beside it, run on its interval \
                 in a process of its own.",
                schema(json!({}), &[]),
            ),
            Tool::new(
                "runs_list",
                "Runs the manager recorded, newest first: job, run_id, when it started and ended, \
                 outcome (ok / failed / timeout / stopped / lost), exit code, and the path of the \
                 run's own session log.",
                schema(
                    json!({
                        "job": job,
                        "limit": { "type": "integer", "minimum": 1, "description": "At most this many (default 50)" }
                    }),
                    &[],
                ),
            ),
            Tool::new(
                "run_get",
                "One run by id, with the tail of its stderr.",
                schema(json!({ "run_id": run_id }), &["run_id"]),
            ),
            Tool::new(
                "job_run",
                "Ask the manager to run a job now. Recorded as a request; the manager's next tick \
                 starts it (unless a run of that job is already live, which is recorded as a skip). \
                 Answers 202 — this does not wait for the run; read it back with runs_list.",
                schema(json!({ "job": job }), &["job"]),
            ),
            Tool::new(
                "run_stop",
                "Ask a live run to stop. Recorded as a request; the manager's next tick kills the \
                 run's process group and records it as stopped. Answers 202.",
                schema(json!({ "run_id": run_id }), &["run_id"]),
            ),
        ]
    }

    /// Answer one tool call, or `None` when the name is not one of these.
    pub async fn call_tool(
        &self,
        params: &CallToolRequestParams,
    ) -> Option<Result<CallToolResponse, McpError>> {
        let args = params.arguments.clone().unwrap_or_default();
        let text = |key: &str| -> Result<String, McpError> {
            args.get(key)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .ok_or_else(|| McpError::invalid_params(format!("`{key}` is required"), None))
        };

        let request =
            match params.name.as_ref() {
                "jobs_list" => Ok((reqwest::Method::GET, "/jobs".to_string())),
                "runs_list" => {
                    let mut query = Vec::new();
                    if let Some(job) = args.get("job").and_then(Value::as_str) {
                        query.push(format!("job={}", encode(job)));
                    }
                    if let Some(limit) = args.get("limit").and_then(Value::as_u64) {
                        query.push(format!("limit={limit}"));
                    }
                    let path = if query.is_empty() {
                        "/runs".to_string()
                    } else {
                        format!("/runs?{}", query.join("&"))
                    };
                    Ok((reqwest::Method::GET, path))
                }
                "run_get" => text("run_id")
                    .map(|id| (reqwest::Method::GET, format!("/runs/{}", encode(&id)))),
                "job_run" => text("job").map(|job| {
                    (
                        reqwest::Method::POST,
                        format!("/jobs/{}/runs", encode(&job)),
                    )
                }),
                "run_stop" => text("run_id")
                    .map(|id| (reqwest::Method::DELETE, format!("/runs/{}", encode(&id)))),
                _ => return None,
            };
        let (method, path) = match request {
            Ok(r) => r,
            Err(e) => return Some(Err(e)),
        };

        tracing::info!(tool = %params.name, %method, %path, "jobs: request");
        let response = match self.call(method, &path).await {
            Ok((status, body)) if (200..300).contains(&status) => {
                let text = serde_json::to_string_pretty(&body).unwrap_or_else(|_| body.to_string());
                CallToolResult::success(vec![ContentBlock::text(text)])
            }
            Ok((status, body)) => {
                let reason = body
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| body.to_string());
                CallToolResult::error(vec![ContentBlock::text(format!(
                    "agent-block serve answered {status}: {reason}"
                ))])
            }
            Err(e) => CallToolResult::error(vec![ContentBlock::text(e)]),
        };
        Some(Ok(response.into()))
    }
}

/// Percent-encode a path segment or query value: what is not unreserved
/// (RFC 3986) is `%XX`, so a job name or run id with a space or a slash
/// stays one segment.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_segment_survives_the_path() {
        assert_eq!(encode("drain"), "drain");
        assert_eq!(encode("echo-1788817170939"), "echo-1788817170939");
        assert_eq!(encode("a b/c"), "a%20b%2Fc");
    }

    #[test]
    fn the_client_keeps_the_url_without_a_trailing_slash() {
        assert_eq!(
            JobsClient::new("http://127.0.0.1:7788/").url,
            "http://127.0.0.1:7788"
        );
        assert_eq!(JobsClient::new(DEFAULT_SERVE_URL).url, DEFAULT_SERVE_URL);
    }

    #[test]
    fn the_five_tools_are_listed_in_order() {
        let names: Vec<String> = JobsClient::new(DEFAULT_SERVE_URL)
            .tools()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert_eq!(names, TOOL_NAMES);
    }

    #[tokio::test]
    async fn a_name_that_is_not_one_of_these_is_not_answered() {
        let client = JobsClient::new(DEFAULT_SERVE_URL);
        let params = CallToolRequestParams::new("run_block");
        assert!(client.call_tool(&params).await.is_none());
    }

    #[tokio::test]
    async fn a_required_argument_that_is_missing_is_an_invalid_params_error() {
        let client = JobsClient::new(DEFAULT_SERVE_URL);
        let mut params = CallToolRequestParams::new("run_get");
        params.arguments = Some(Map::new());
        let err = client
            .call_tool(&params)
            .await
            .expect("answered")
            .expect_err("invalid");
        assert!(err.message.contains("run_id"), "{}", err.message);
    }
}
