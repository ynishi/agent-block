//! Shared error types, credential constants and observability utilities for
//! agent-block.
//!
//! This crate is the leaf dependency in the workspace — it does not depend on
//! any other `agent-block-*` crate.

pub mod creds;
pub mod error;
pub mod obs;
