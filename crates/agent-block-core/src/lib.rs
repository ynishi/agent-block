//! agent-block-core — host runtime + Lua stdlib bridge + EventBus.
//!
//! Depends on `agent-block-types` (error / obs) and `agent-block-mcp`
//! (rmcp wrapper).  The bin crate `agent-block` is a thin CLI on top.

pub mod bridge;
pub mod bus;
pub mod host;

pub use host::{run, BlockConfig, HostContext};
