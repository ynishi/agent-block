//! Event type flowing through the [`EventBus`](crate::bus::EventBus).
//!
//! Each [`Event`] carries a `kind` string used for handler dispatch, an `id`
//! for correlation/logging, a `payload` (source-defined), a `meta` map
//! (source-defined), and an optional `ack_tx` one-shot channel used to return
//! the Lua handler's return value back to the source that produced the
//! request (e.g. a mesh request/response round-trip).
//!
//! The `ack_tx` is `Option` because some sources are fire-and-forget
//! (e.g. a future webhook broadcast) and do not need a response.

use serde_json::Value;
use tokio::sync::oneshot;

use crate::error::BlockError;

/// Result carried back to the originating source via [`Event::ack_tx`].
pub type AckResult = Result<Value, BlockError>;

/// Sender half of the ack channel. Carried inside [`Event`].
pub type AckSender = oneshot::Sender<AckResult>;

/// Receiver half of the ack channel. Held by whatever source produced the
/// event and awaits the handler's return value.
pub type AckReceiver = oneshot::Receiver<AckResult>;

/// A normalized event flowing through the bus.
///
/// Ownership: produced by a [`Source`](crate::bus::Source), moved through a
/// bounded `mpsc::Sender<Event>` into the single dispatcher loop. The
/// dispatcher consumes the `ack_tx` (via `Option::take`) to send the
/// handler's return value back to the source.
#[derive(Debug)]
pub struct Event {
    /// Dispatch key. Matched against `bus.on(kind, fn)` registrations.
    pub kind: String,
    /// Correlation id (source-assigned). Used in tracing/logging.
    pub id: String,
    /// Source-defined payload. Converted to Lua table at dispatch time.
    pub payload: Value,
    /// Source-defined metadata (e.g. mesh `from`, timestamps). Converted to
    /// Lua table at dispatch time.
    pub meta: Value,
    /// Optional one-shot channel used to return the Lua handler's result.
    /// `None` for fire-and-forget sources.
    pub ack_tx: Option<AckSender>,
}

impl Event {
    /// Construct a new event without an ack channel (fire-and-forget).
    pub fn fire_and_forget(kind: impl Into<String>, id: impl Into<String>, payload: Value) -> Self {
        Self {
            kind: kind.into(),
            id: id.into(),
            payload,
            meta: Value::Null,
            ack_tx: None,
        }
    }

    /// Construct a new event paired with a fresh ack channel. Returns the
    /// event (to be pushed to the bus) and the receiver half (to be awaited
    /// by the source).
    pub fn with_ack(
        kind: impl Into<String>,
        id: impl Into<String>,
        payload: Value,
        meta: Value,
    ) -> (Self, AckReceiver) {
        let (tx, rx) = oneshot::channel();
        let evt = Self {
            kind: kind.into(),
            id: id.into(),
            payload,
            meta,
            ack_tx: Some(tx),
        };
        (evt, rx)
    }

    /// Send `result` on `ack_tx` if it is still present. Logs a warning when
    /// the receiver has been dropped (tracing-missing-on-err policy).
    ///
    /// Returns `Ok(())` when the ack was delivered or the event was
    /// fire-and-forget. Returns `Err(BlockError::Bus)` only when the
    /// receiver had been dropped — the caller can decide whether to treat
    /// that as fatal.
    pub fn deliver_ack(&mut self, result: AckResult) -> Result<(), BlockError> {
        let Some(tx) = self.ack_tx.take() else {
            return Ok(());
        };
        if let Err(dropped) = tx.send(result) {
            tracing::warn!(
                kind = %self.kind,
                id = %self.id,
                "ack receiver dropped; handler result discarded: {:?}",
                dropped.as_ref().map(|_| "ok").unwrap_or_else(|e| match e {
                    BlockError::Bus(_) => "bus-err",
                    _ => "other-err",
                })
            );
            return Err(BlockError::Bus(format!(
                "ack receiver dropped (kind={}, id={})",
                self.kind, self.id
            )));
        }
        Ok(())
    }
}
