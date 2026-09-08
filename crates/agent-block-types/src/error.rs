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

    /// The script looked at what it needs and did not start — the raise a
    /// block makes with `job.defer(reason)` (blocks/lib/job/init.lua), read
    /// off the script failure by its prefix. The CLI exits 75 on it
    /// (sysexits `EX_TEMPFAIL`) so the job manager records the run as
    /// `deferred` rather than `failed`: nothing was done, and nothing was
    /// wrong with the block. The payload is the reason.
    #[error("deferred: {0}")]
    Deferred(String),

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
