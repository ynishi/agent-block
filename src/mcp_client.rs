//! MCP Client — manages stdio-based MCP server child processes.
//!
//! Speaks JSON-RPC 2.0 over line-delimited JSON (newline-separated).
//! Each MCP server runs as a child process with piped stdin/stdout.
//!
//! All I/O is async via tokio, allowing Lua coroutines to yield
//! while waiting for MCP server responses.
//!
//! # Protocol
//!
//! - Send: `{"jsonrpc":"2.0","id":N,"method":"...","params":{...}}\n`
//! - Receive: one JSON line per response
//! - Handshake: `initialize` request → response → `notifications/initialized` notification
//!
//! # Usage from Lua
//!
//! ```lua
//! mcp.connect("outline", "outline-mcp", {})
//! local tools = mcp.list_tools("outline")
//! local result = mcp.call("outline", "shelf", {})
//! mcp.disconnect("outline")
//! ```

use std::collections::HashMap;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::error::{BlockError, BlockResult};

/// Default timeout for MCP JSON-RPC responses (30 seconds).
const RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub struct McpServer {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpServer {
    async fn send(&mut self, msg: &Value) -> BlockResult<()> {
        let body = serde_json::to_string(msg)?;
        self.stdin.write_all(body.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    /// Receive the response matching `expected_id`, skipping notifications.
    async fn recv_response(&mut self, expected_id: u64) -> BlockResult<Value> {
        let deadline = tokio::time::Instant::now() + RECV_TIMEOUT;
        loop {
            // Per-line timeout from remaining budget.
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(BlockError::Timeout(format!(
                    "MCP response timeout ({RECV_TIMEOUT:?})"
                )));
            }

            let mut line = String::new();
            let read = tokio::time::timeout(remaining, self.stdout.read_line(&mut line))
                .await
                .map_err(|_| {
                    BlockError::Timeout(format!("MCP response timeout ({RECV_TIMEOUT:?})"))
                })?
                .map_err(BlockError::Io)?;

            if read == 0 {
                return Err(BlockError::Mcp("MCP server closed stdout".into()));
            }

            let msg: Value = serde_json::from_str(line.trim())?;

            // No "id" → server-to-client notification; skip.
            let msg_id = match msg.get("id") {
                Some(id) => id.clone(),
                None => continue,
            };

            let matches = match &msg_id {
                Value::Number(n) => n.as_u64() == Some(expected_id),
                _ => false,
            };
            if !matches {
                tracing::warn!(
                    expected = expected_id,
                    got = %msg_id,
                    "MCP: skipping response with unexpected id"
                );
                continue;
            }

            return Ok(msg);
        }
    }

    fn next_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn request(&mut self, method: &str, params: Value) -> BlockResult<Value> {
        let id = self.next_id();
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send(&req).await?;
        let resp = self.recv_response(id).await?;
        if let Some(err) = resp.get("error") {
            return Err(BlockError::Mcp(format!("JSON-RPC error: {err}")));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&mut self, method: &str) -> BlockResult<()> {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
        });
        self.send(&msg).await?;
        Ok(())
    }
}

pub struct McpManager {
    servers: HashMap<String, McpServer>,
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
        }
    }

    /// Spawn the MCP server process and complete the initialize handshake.
    pub async fn connect(&mut self, name: &str, command: &str, args: &[String]) -> BlockResult<()> {
        let mut child = tokio::process::Command::new(command)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|e| BlockError::Mcp(format!("failed to spawn {command}: {e}")))?;

        let raw_stdin = child
            .stdin
            .take()
            .ok_or_else(|| BlockError::Mcp("stdin not available".into()))?;
        let raw_stdout = child
            .stdout
            .take()
            .ok_or_else(|| BlockError::Mcp("stdout not available".into()))?;

        let mut server = McpServer {
            child,
            stdin: BufWriter::new(raw_stdin),
            stdout: BufReader::new(raw_stdout),
            next_id: 1,
        };

        // initialize handshake
        server
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "agent-block", "version": "0.1.0"},
                }),
            )
            .await?;

        server.notify("notifications/initialized").await?;

        self.servers.insert(name.to_string(), server);
        Ok(())
    }

    /// Send a JSON-RPC request and return the `result` value.
    pub async fn call(&mut self, name: &str, method: &str, params: Value) -> BlockResult<Value> {
        let server = self
            .servers
            .get_mut(name)
            .ok_or_else(|| BlockError::Mcp(format!("no server named '{name}'")))?;
        server.request(method, params).await
    }

    /// Call `tools/list` and return the tool array.
    pub async fn list_tools(&mut self, name: &str) -> BlockResult<Value> {
        self.call(name, "tools/list", serde_json::json!({})).await
    }

    /// Call `tools/call` with the given tool name and arguments.
    pub async fn call_tool(
        &mut self,
        name: &str,
        tool_name: &str,
        arguments: Value,
    ) -> BlockResult<Value> {
        self.call(
            name,
            "tools/call",
            serde_json::json!({
                "name": tool_name,
                "arguments": arguments,
            }),
        )
        .await
    }

    /// Kill the named server process and wait for it to exit.
    pub async fn disconnect(&mut self, name: &str) -> BlockResult<()> {
        if let Some(mut server) = self.servers.remove(name) {
            server
                .child
                .kill()
                .await
                .map_err(|e| BlockError::Mcp(format!("kill server '{name}' failed: {e}")))?;
        }
        Ok(())
    }

    /// Kill all managed server processes.
    pub async fn disconnect_all(&mut self) -> BlockResult<()> {
        let mut first_err: Option<BlockError> = None;
        let names: Vec<String> = self.servers.keys().cloned().collect();
        for name in names {
            if let Err(e) = self.disconnect(&name).await {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Default for McpManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn new_manager_is_empty() {
        let mgr = McpManager::new();
        assert!(mgr.servers.is_empty());
    }

    #[tokio::test]
    async fn disconnect_nonexistent_is_ok() {
        let mut mgr = McpManager::new();
        assert!(mgr.disconnect("ghost").await.is_ok());
    }

    #[tokio::test]
    async fn call_unknown_server_returns_error() {
        let mut mgr = McpManager::new();
        let res = mgr.call("none", "tools/list", serde_json::json!({})).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn disconnect_all_empties_map() {
        let mut mgr = McpManager::new();
        mgr.disconnect_all()
            .await
            .expect("disconnect_all on empty manager should succeed");
        assert!(mgr.servers.is_empty());
    }
}
