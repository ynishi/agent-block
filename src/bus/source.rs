//! [`Source`] trait: something that produces [`Event`]s.
//!
//! In the final wiring (Subtask 3), concrete implementations will include a
//! mesh adapter that turns `agent_mesh_sdk::RequestHandler::handle` calls
//! into events pushed to the bus.
//!
//! Sources that need a response from the Lua handler (request/response
//! round-trip) construct events via [`Event::with_ack`] and await the
//! receiver side themselves. Sources that are fire-and-forget use
//! [`Event::fire_and_forget`].
//!
//! Note: the `next()` API on this trait is kept for symmetry with
//! pull-style sources. The canonical wiring in `agent-block` uses a single
//! shared `mpsc::Sender<Event>` that sources push into directly (see
//! plan.md §設計選択 A1). `next()` is retained for adapters that prefer a
//! pull interface and for the in-crate mock used by `#[cfg(test)]` in the
//! dispatcher module.

use async_trait::async_trait;

use crate::bus::event::Event;
use crate::error::BlockError;

/// A producer of [`Event`]s.
///
/// ST3 uses push-style ingress (`mpsc::Sender<Event>` cloned into each
/// adapter) and does not exercise this trait. Retained for ST4+ adapters
/// that prefer a pull interface.
#[allow(dead_code)]
#[async_trait]
pub trait Source: Send + Sync {
    /// Canonical kind string used by this source (e.g. `"mesh"`).
    fn kind(&self) -> &str;

    /// Pull the next event. `Ok(None)` signals the source has been
    /// exhausted and will produce no more events. `Err` is logged by the
    /// dispatcher (or the caller) and does not terminate the bus.
    async fn next(&mut self) -> Result<Option<Event>, BlockError>;
}
