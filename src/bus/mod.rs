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

// Consumed by `bridge::bus` (Lua bridge) and `host::BusRelayHandler`
// (mesh → bus adapter). `HandlerKey` / `Source` are not yet referenced
// outside `bus::dispatcher` / `bus::source` — kept public for forthcoming
// adapters (webhook, WSS, timer).
#[allow(unused_imports)]
pub use dispatcher::HandlerKey;
pub use dispatcher::{EventBus, Handler};
#[allow(unused_imports)]
pub use event::{AckReceiver, AckSender};
pub use event::{AckResult, Event};
#[allow(unused_imports)]
pub use source::Source;
