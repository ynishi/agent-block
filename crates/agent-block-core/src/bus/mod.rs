//! EventBus: serial event dispatcher feeding Lua handlers registered via
//! `bus.on(kind, fn)` / `bus.on_any(fn)`.
//!
//! The pure-Rust core lives here: [`Event`], [`Source`], [`EventBus`] and
//! the [`Handler`] trait they dispatch through. Nothing in this module
//! knows about Lua — the Lua-side handler is one implementation of
//! [`Handler`], registered by the `bus` bridge.

pub mod dispatcher;
pub mod event;
pub mod http_source;
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
