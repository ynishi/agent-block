//! EventBus: serial event dispatcher feeding Lua handlers registered via
//! `bus.on(kind, fn)` / `bus.on_any(fn)`.
//!
//! This subtask (Subtask 1) defines the pure-Rust core: [`Event`],
//! [`Source`], [`EventBus`], plus a [`Handler`] trait placeholder that
//! Subtask 3 will swap for an `mlua::RegistryKey`-backed implementation.
//!
//! Module wiring (`mod bus;` in `main.rs`) and `tokio-util` Cargo
//! dependency are deferred to Subtask 2.

pub mod dispatcher;
pub mod event;
pub mod source;

// These re-exports are consumed by Subtask 3 (Lua bridge + mesh adapter).
// The `#[allow(unused_imports)]` suppresses `unused import` warnings
// during the Subtask 1 isolated build.
#[allow(unused_imports)]
pub use dispatcher::{EventBus, Handler, HandlerKey};
#[allow(unused_imports)]
pub use event::{AckReceiver, AckResult, AckSender, Event};
#[allow(unused_imports)]
pub use source::Source;
