//! agent-block-core — host runtime + Lua stdlib bridge + EventBus.
//!
//! Depends on `agent-block-types` (error / obs) and `agent-block-mcp`
//! (rmcp wrapper).  The bin crate `agent-block` is a thin CLI on top.
//!
//! # Cargo features
//!
//! All three are **on by default**, so the default build (and the
//! `agent-block` CLI) keeps the full runtime surface. SDK embedders that
//! only need a subset can opt out with `default-features = false` and
//! re-enable individual axes:
//!
//! | Feature    | Enables                                                          | Pulls in |
//! |------------|------------------------------------------------------------------|----------|
//! | `mesh`     | `mesh.*` Lua bridge, relay connect, Ed25519 mesh identity        | `agent-mesh-core`, `agent-mesh-sdk` |
//! | `sqlite`   | `sql.*` / `kv.*` / `ts.*` Lua bridges (SQLite-backed)            | `rusqlite` (bundled), `mlua-batteries/{sql,kv}` |
//! | `mcp-http` | `mcp.connect_http` (Streamable HTTP / SSE MCP transport)         | `agent-block-mcp/mcp-http` → rmcp HTTP-client transports |
//!
//! The stdio MCP transport (`mcp.connect`), `http.*`, `tool.*`, `sh.*`,
//! `log.*`, `bus.*`, and the embedded StdPkg blocks are always available.
//!
//! When a feature is **off**:
//! - `mesh`: the `mesh.*` bridge is not registered; `BlockConfig::relay_url`
//!   / `secret_key` are accepted but ignored.
//! - `sqlite`: the `sql.*` / `kv.*` / `ts.*` bridges are not registered;
//!   `BlockConfig::sql_path` / `kv_path` / `ts_path` are accepted but ignored
//!   (fields retained for API stability).
//! - `mcp-http`: `mcp.connect_http` returns an explicit error when called.
//!
//! # Sandbox
//!
//! [`sandbox`] installs an optional process-wide execution boundary (Landlock +
//! seccomp, Linux only): filesystem writes are confined to an allowlist and
//! io_uring is denied, while reads and executes stay open. It is inherited by
//! `sh.exec` / `mcp.connect` children, and must be applied before any async
//! runtime spawns worker threads — see the module docs.

pub mod bridge;
pub mod bus;
pub mod host;
pub mod sandbox;

pub use host::{run, BlockConfig, BlockConfigBuilder, HostContext};
