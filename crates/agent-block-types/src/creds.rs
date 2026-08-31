//! The block host's own credential environment variables.
//!
//! Lives in the leaf crate so every spawn path in the workspace strips the same
//! set: `sh.exec` children (`agent-block-core`) and MCP server subprocesses
//! (`agent-block-mcp`).

/// The block host's own credential environment variables.
///
/// These are removed from every child process the host spawns, so code executed
/// by the agent — including code the agent just wrote — cannot read the host's
/// keys.
///
/// The scope is deliberately narrow:
///
/// - Custom key env names configured per-LLM conf (`api_key_env`) are **not**
///   covered; generalizing that belongs to a planned exec-tool redesign.
/// - This is **not** an env allowlist. Every other variable is still inherited.
pub const OWN_CREDENTIAL_ENV_VARS: &[&str] = &[
    // Default key env of the anthropic provider path (blocks/agent, blocks/compile_loop).
    "ANTHROPIC_API_KEY",
    // Default key env of the openai-compatible provider path.
    "OPENAI_API_KEY",
    // Core's own mesh Ed25519 secret key (`--secret-key`).
    "AGENT_BLOCK_MESH_SECRET_KEY",
];
