//! MCP Client — manages MCP server child processes via rmcp.
//!
//! Uses `rmcp` (1.4.x) `RunningService<RoleClient, ()>` internally.
//! The `()` unit type provides the default `ClientHandler` implementation
//! which returns `method_not_found` for `create_message` (sampling not advertised).
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
use std::process::Stdio;

use rmcp::{
    model::CallToolRequestParams,
    service::{RoleClient, RunningService},
    transport::TokioChildProcess,
    ServiceExt,
};
use tokio::process::Command;

use crate::error::{BlockError, BlockResult};

pub struct McpManager {
    servers: HashMap<String, RunningService<RoleClient, ()>>,
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            servers: HashMap::new(),
        }
    }

    /// Spawn the MCP server process and complete the MCP initialize handshake.
    pub async fn connect(&mut self, name: &str, command: &str, args: &[String]) -> BlockResult<()> {
        let mut cmd = Command::new(command);
        cmd.args(args).stderr(Stdio::inherit());
        let transport = TokioChildProcess::new(cmd)
            .map_err(|e| BlockError::Mcp(format!("spawn {command}: {e}")))?;
        let running = ()
            .serve(transport)
            .await
            .map_err(|e| BlockError::Mcp(format!("initialize {name}: {e}")))?;
        self.servers.insert(name.to_string(), running);
        Ok(())
    }

    /// Call `tools/list` and return `{"tools": [...]}`.
    pub async fn list_tools(&mut self, name: &str) -> BlockResult<serde_json::Value> {
        let srv = self
            .servers
            .get(name)
            .ok_or_else(|| BlockError::Mcp(format!("no server named '{name}'")))?;
        let tools = srv
            .list_all_tools()
            .await
            .map_err(|e| BlockError::Mcp(format!("list_tools '{name}': {e}")))?;
        Ok(serde_json::json!({ "tools": tools }))
    }

    /// Call `tools/call` with the given tool name and arguments.
    ///
    /// Returns `{"content": [...], ...}` on success.
    /// If `arguments` is not a JSON object, an empty object is used.
    pub async fn call_tool(
        &mut self,
        name: &str,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> BlockResult<serde_json::Value> {
        let srv = self
            .servers
            .get(name)
            .ok_or_else(|| BlockError::Mcp(format!("no server named '{name}'")))?;
        let args_obj = arguments.as_object().cloned();
        let mut params = CallToolRequestParams::new(tool_name.to_string());
        if let Some(obj) = args_obj {
            params = params.with_arguments(obj);
        }
        let result = srv
            .call_tool(params)
            .await
            .map_err(|e| BlockError::Mcp(format!("call_tool '{tool_name}' on '{name}': {e}")))?;
        serde_json::to_value(&result)
            .map_err(|e| BlockError::Mcp(format!("serialize call_tool result: {e}")))
    }

    /// Cancel the named server and remove it from the manager.
    pub async fn disconnect(&mut self, name: &str) -> BlockResult<()> {
        if let Some(running) = self.servers.remove(name) {
            running
                .cancel()
                .await
                .map_err(|e| BlockError::Mcp(format!("cancel '{name}': {e}")))?;
        }
        Ok(())
    }

    /// Cancel all managed servers, collecting the first error if any.
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
        let res = mgr.call_tool("none", "dummy", serde_json::json!({})).await;
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
