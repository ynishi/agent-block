//! Typed error types for agent-block internals.
//!
//! All library/internal code uses `BlockError`.
//! Only `main.rs` converts to `anyhow::Error` for CLI output.

#[derive(Debug, thiserror::Error)]
pub enum BlockError {
    #[error("MCP error: {0}")]
    Mcp(String),

    #[error("mesh error: {0}")]
    Mesh(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("script error: {0}")]
    Script(String),

    #[error("timeout: {0}")]
    Timeout(String),

    #[error("runtime error: {0}")]
    Runtime(String),

    // Used by `crate::bus` (dispatcher + event) and `bridge::bus`.
    #[error("bus error: {0}")]
    Bus(String),

    /// External `BlockConfig.shutdown_token` was cancelled while `run()`
    /// was driving the script. The script may have been interrupted
    /// mid-execution; in-flight handlers may have observed partial state.
    #[error("cancelled")]
    Cancelled,
}

pub type BlockResult<T> = Result<T, BlockError>;
