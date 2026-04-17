//! EventBus dispatcher.
//!
//! Single-task, single-loop dispatcher. Events arrive on a bounded
//! `mpsc::Receiver<Event>` and are dispatched serially to the registered
//! handler (kind-specific first, then `any` fallback, otherwise the event
//! is NACK'd with a `BlockError::Bus` and a `tracing::warn!`).
//!
//! ## Concurrency primitives in use (see `concurrency-analysis.md` §1)
//!
//! - `tokio::sync::mpsc` (bounded) for ingress — backpressure rather than
//!   drop
//! - `tokio::sync::oneshot` for ack (carried on the [`Event`] itself)
//! - `tokio_util::sync::CancellationToken` for cooperative shutdown
//! - `tokio::select!` races `rx.recv()` against `shutdown.cancelled()`
//! - `tokio::task::spawn` + awaited `JoinHandle` is used to invoke
//!   handlers. A panic inside the handler surfaces as
//!   `JoinError::is_panic()` on the join handle, which the dispatcher
//!   converts into a `BlockError::Bus` ack — no panic propagates out of
//!   the loop. The dispatcher awaits the join handle immediately, so
//!   handlers still run serially.
//!
//! ## Serial guarantee
//!
//! The loop awaits each handler to completion before pulling the next
//! event. This gives Lua handlers the cooperative-serial model described
//! in `plan.md` (§設計選択 A1 + mlua-isle single-thread VM).
//!
//! ## Shutdown policy
//!
//! On cancel:
//! 1. The `tokio::select!` picks the cancel branch.
//! 2. We `rx.close()` — further send attempts by sources will fail fast.
//! 3. In-flight handler (if any) is *not* pre-empted; the current
//!    iteration finishes its await. The loop then exits.
//! 4. Events that had queued into the mpsc buffer are **not drained** —
//!    their ack senders drop, and callers see `RecvError` on their
//!    oneshot (documented in `plan.md` Risks).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bus::event::{AckResult, Event};
use crate::error::BlockError;

/// Callable target for a registered handler.
///
/// Subtask 1 uses a trait object as a placeholder; Subtask 3 will plug in
/// an `mlua::RegistryKey`-backed implementation that dispatches into the
/// Isle Lua thread. The trait contract here intentionally mirrors what
/// Subtask 3 needs: take an owned [`Event`] (minus its `ack_tx`, which is
/// managed by the dispatcher), return a result that becomes the ack.
///
/// `'static` bound is required because the dispatcher invokes handlers via
/// `tokio::task::spawn`, which requires the future to outlive any
/// references captured on the current stack.
#[async_trait]
pub trait Handler: Send + Sync + 'static {
    /// Invoke the handler with the event's kind/id/payload/meta.
    ///
    /// The handler implementation itself does not touch `ack_tx`. The
    /// dispatcher takes care of delivering the result on the oneshot.
    async fn call(&self, kind: String, id: String, payload: Value, meta: Value) -> AckResult;
}

/// Boxed handler reference used inside [`EventBus`].
///
/// Named `HandlerKey` to stay aligned with `subtask-1.md` §Design,
/// where the field is called `HandlerKey`. In Subtask 3 this type will be
/// replaced with a concrete struct wrapping `mlua::RegistryKey`.
pub type HandlerKey = Arc<dyn Handler>;

/// The serial event dispatcher.
pub struct EventBus {
    /// Ingress queue. Sources push events into the paired `Sender` (held
    /// outside). Capacity is configured by the caller (default comes from
    /// `AGENT_BLOCK_BUS_CAPACITY`, wired in Subtask 2).
    rx: mpsc::Receiver<Event>,
    /// kind -> handler. Populated via [`EventBus::on`] before [`EventBus::run`]
    /// is awaited.
    handlers: HashMap<String, HandlerKey>,
    /// Fallback handler — fires only when no `handlers[kind]` is present.
    any: Option<HandlerKey>,
    /// Set once when `run` begins. Used to reject `on` / `on_any` calls
    /// after dispatcher start (see plan.md §Constraints).
    running: bool,
}

impl EventBus {
    /// Build a new bus from an mpsc receiver. The paired `Sender` must be
    /// held by callers (and shared to sources) so they can push events.
    pub fn new(rx: mpsc::Receiver<Event>) -> Self {
        Self {
            rx,
            handlers: HashMap::new(),
            any: None,
            running: false,
        }
    }

    /// Register a kind-specific handler. Last write wins — re-registering
    /// the same `kind` silently replaces the previous handler (documented
    /// in plan.md §Phase 3 / wf-sim Counter-WF).
    ///
    /// Returns `Err(BlockError::Bus)` if called after [`EventBus::run`] has
    /// begun.
    pub fn on(&mut self, kind: impl Into<String>, handler: HandlerKey) -> Result<(), BlockError> {
        if self.running {
            return Err(BlockError::Bus(
                "bus.on cannot be called after bus.serve() has started".into(),
            ));
        }
        let kind = kind.into();
        if self.handlers.insert(kind.clone(), handler).is_some() {
            tracing::warn!(kind = %kind, "bus.on: duplicate registration (last-write-wins)");
        }
        Ok(())
    }

    /// Register the `on_any` fallback. Invoked only when no `on(kind)`
    /// handler matches the event's `kind`. NOT a fan-out/tap.
    pub fn on_any(&mut self, handler: HandlerKey) -> Result<(), BlockError> {
        if self.running {
            return Err(BlockError::Bus(
                "bus.on_any cannot be called after bus.serve() has started".into(),
            ));
        }
        if self.any.is_some() {
            tracing::warn!("bus.on_any: duplicate registration (last-write-wins)");
        }
        self.any = Some(handler);
        Ok(())
    }

    /// Test-only accessor used by `#[cfg(test)] mod tests` to check the
    /// table contents without exposing internals to the rest of the crate.
    #[cfg(test)]
    fn handler_count(&self) -> usize {
        self.handlers.len()
    }

    /// Drive the dispatcher loop until `shutdown` is cancelled.
    ///
    /// Cancel-safety: `mpsc::Receiver::recv` is cancel-safe; dropping the
    /// `select!` branch on the `recv` side loses no events (tokio docs).
    /// `CancellationToken::cancelled` is explicitly designed for this
    /// usage (tokio-util docs).
    pub async fn run(&mut self, shutdown: CancellationToken) -> Result<(), BlockError> {
        self.running = true;
        tracing::info!("bus: dispatcher loop starting");
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => {
                    tracing::info!("bus: shutdown signalled; closing receiver");
                    self.rx.close();
                    break;
                }
                maybe_evt = self.rx.recv() => {
                    let Some(evt) = maybe_evt else {
                        tracing::info!("bus: all senders dropped; exiting loop");
                        break;
                    };
                    self.dispatch(evt).await;
                }
            }
        }
        tracing::info!("bus: dispatcher loop exited");
        Ok(())
    }

    /// Dispatch a single event to the matching handler (or the fallback,
    /// or nack).
    async fn dispatch(&self, mut evt: Event) {
        let handler = self
            .handlers
            .get(&evt.kind)
            .cloned()
            .or_else(|| self.any.clone());

        let Some(handler) = handler else {
            tracing::warn!(kind = %evt.kind, id = %evt.id, "bus: no handler for event; nacking");
            let err = BlockError::Bus(format!("no handler for kind `{}`", evt.kind));
            if let Err(e) = evt.deliver_ack(Err(err)) {
                tracing::warn!(kind = %evt.kind, id = %evt.id, error = %e, "bus: failed to deliver nack");
            }
            return;
        };

        let kind = evt.kind.clone();
        let id = evt.id.clone();
        let payload = evt.payload.clone();
        let meta = evt.meta.clone();

        // Spawn the handler as its own task and await the `JoinHandle`.
        // A panic inside the handler surfaces as `JoinError::is_panic()`
        // and is converted into a `BlockError::Bus` ack — the dispatcher
        // loop itself never panics (panic-in-product policy).
        let join = tokio::spawn(async move { handler.call(kind, id, payload, meta).await });

        let result: AckResult = match join.await {
            Ok(ack) => ack,
            Err(join_err) => {
                let msg = if join_err.is_panic() {
                    panic_message(join_err.into_panic())
                } else {
                    format!("handler task error: {join_err}")
                };
                tracing::error!(
                    kind = %evt.kind,
                    id = %evt.id,
                    "bus: handler panicked: {}",
                    msg
                );
                Err(BlockError::Bus(format!("handler panic: {msg}")))
            }
        };

        if let Err(ref e) = result {
            tracing::warn!(kind = %evt.kind, id = %evt.id, error = %e, "bus: handler returned error");
        }

        if let Err(e) = evt.deliver_ack(result) {
            tracing::warn!(kind = %evt.kind, id = %evt.id, error = %e, "bus: ack delivery failed");
        }
    }
}

/// Best-effort extraction of a human-readable message from a panic
/// payload. Returns `"<non-string panic payload>"` when the panic value
/// is neither `&str` nor `String`.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use tokio::sync::oneshot;

    /// Test handler that records invocations and returns a fixed value.
    struct RecordingHandler {
        label: &'static str,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Handler for RecordingHandler {
        async fn call(
            &self,
            _kind: String,
            _id: String,
            _payload: Value,
            _meta: Value,
        ) -> AckResult {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Value::String(self.label.to_string()))
        }
    }

    struct PanickingHandler;

    #[async_trait]
    impl Handler for PanickingHandler {
        async fn call(
            &self,
            _kind: String,
            _id: String,
            _payload: Value,
            _meta: Value,
        ) -> AckResult {
            panic!("boom");
        }
    }

    fn send_event(tx: &mpsc::Sender<Event>, kind: &str, id: &str) -> oneshot::Receiver<AckResult> {
        let (evt, rx) = Event::with_ack(kind, id, json!({"hello": "world"}), Value::Null);
        tx.try_send(evt).expect("mpsc send");
        rx
    }

    #[tokio::test]
    async fn kind_specific_dispatch_hits_specialized_handler() {
        let (tx, rx) = mpsc::channel::<Event>(4);
        let mut bus = EventBus::new(rx);
        let mesh_calls = Arc::new(AtomicUsize::new(0));
        let any_calls = Arc::new(AtomicUsize::new(0));
        bus.on(
            "mesh",
            Arc::new(RecordingHandler {
                label: "mesh",
                calls: mesh_calls.clone(),
            }),
        )
        .unwrap();
        bus.on_any(Arc::new(RecordingHandler {
            label: "any",
            calls: any_calls.clone(),
        }))
        .unwrap();

        let token = CancellationToken::new();
        let token_clone = token.clone();
        let handle = tokio::spawn(async move { bus.run(token_clone).await });

        let ack = send_event(&tx, "mesh", "e1");
        let got = ack.await.unwrap().unwrap();
        assert_eq!(got, Value::String("mesh".into()));
        assert_eq!(mesh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(any_calls.load(Ordering::SeqCst), 0);

        token.cancel();
        drop(tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn on_any_fallback_fires_only_when_no_match() {
        let (tx, rx) = mpsc::channel::<Event>(4);
        let mut bus = EventBus::new(rx);
        let any_calls = Arc::new(AtomicUsize::new(0));
        bus.on_any(Arc::new(RecordingHandler {
            label: "any",
            calls: any_calls.clone(),
        }))
        .unwrap();

        let token = CancellationToken::new();
        let token_clone = token.clone();
        let handle = tokio::spawn(async move { bus.run(token_clone).await });

        let ack = send_event(&tx, "unknown_kind", "e1");
        let got = ack.await.unwrap().unwrap();
        assert_eq!(got, Value::String("any".into()));
        assert_eq!(any_calls.load(Ordering::SeqCst), 1);

        token.cancel();
        drop(tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn no_handler_produces_nack() {
        let (tx, rx) = mpsc::channel::<Event>(4);
        let mut bus = EventBus::new(rx);
        // no handlers registered

        let token = CancellationToken::new();
        let token_clone = token.clone();
        let handle = tokio::spawn(async move { bus.run(token_clone).await });

        let ack = send_event(&tx, "mesh", "e1");
        let got = ack.await.unwrap();
        match got {
            Err(BlockError::Bus(msg)) => {
                assert!(msg.contains("no handler"), "unexpected msg: {msg}");
            }
            other => panic!("expected Bus err, got {other:?}"),
        }

        token.cancel();
        drop(tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn shutdown_token_breaks_loop() {
        let (tx, rx) = mpsc::channel::<Event>(4);
        let mut bus = EventBus::new(rx);
        bus.on_any(Arc::new(RecordingHandler {
            label: "any",
            calls: Arc::new(AtomicUsize::new(0)),
        }))
        .unwrap();

        let token = CancellationToken::new();
        let token_clone = token.clone();
        let handle = tokio::spawn(async move { bus.run(token_clone).await });

        token.cancel();
        // Expect the task to exit promptly.
        let res = tokio::time::timeout(Duration::from_millis(500), handle)
            .await
            .expect("timeout");
        res.unwrap().unwrap();
        drop(tx);
    }

    #[tokio::test]
    async fn handler_panic_is_isolated_and_loop_continues() {
        let (tx, rx) = mpsc::channel::<Event>(4);
        let mut bus = EventBus::new(rx);
        let ok_calls = Arc::new(AtomicUsize::new(0));
        bus.on("boom", Arc::new(PanickingHandler)).unwrap();
        bus.on(
            "ok",
            Arc::new(RecordingHandler {
                label: "ok",
                calls: ok_calls.clone(),
            }),
        )
        .unwrap();

        let token = CancellationToken::new();
        let token_clone = token.clone();
        let handle = tokio::spawn(async move { bus.run(token_clone).await });

        // First event: handler panics. Expect a Bus err ack.
        let ack = send_event(&tx, "boom", "e1");
        let got = ack.await.unwrap();
        match got {
            Err(BlockError::Bus(msg)) => {
                assert!(
                    msg.contains("panic") || msg.contains("boom"),
                    "unexpected msg: {msg}"
                );
            }
            other => panic!("expected Bus err, got {other:?}"),
        }

        // Second event after the panic: should still be handled.
        let ack = send_event(&tx, "ok", "e2");
        let got = ack.await.unwrap().unwrap();
        assert_eq!(got, Value::String("ok".into()));
        assert_eq!(ok_calls.load(Ordering::SeqCst), 1);

        token.cancel();
        drop(tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn bounded_mpsc_applies_backpressure_not_drop() {
        // Capacity-1 channel; fill it, then a second send must wait (not
        // drop). We verify by asserting try_send fails with Full while the
        // dispatcher is paused.
        let (tx, rx) = mpsc::channel::<Event>(1);
        let mut bus = EventBus::new(rx);
        bus.on(
            "slow",
            Arc::new(RecordingHandler {
                label: "slow",
                calls: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .unwrap();

        // Do NOT start the dispatcher yet — we want the channel to fill.
        let (evt1, _ack1_rx) = Event::with_ack("slow", "e1", json!({}), Value::Null);
        tx.try_send(evt1).expect("first send fits capacity 1");

        let (evt2, _ack2_rx) = Event::with_ack("slow", "e2", json!({}), Value::Null);
        let err = tx.try_send(evt2).unwrap_err();
        assert!(
            matches!(err, mpsc::error::TrySendError::Full(_)),
            "expected Full, got {err:?}"
        );

        // Now drain it to prove the receiver actually reads them.
        let token = CancellationToken::new();
        let token_clone = token.clone();
        let handle = tokio::spawn(async move { bus.run(token_clone).await });
        // Give dispatcher time to drain one event, then cancel.
        tokio::time::sleep(Duration::from_millis(50)).await;
        token.cancel();
        drop(tx);
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn on_after_running_returns_err() {
        let (_tx, rx) = mpsc::channel::<Event>(1);
        let mut bus = EventBus::new(rx);
        // Simulate "running" state without actually running the loop.
        bus.running = true;
        let err = bus
            .on(
                "mesh",
                Arc::new(RecordingHandler {
                    label: "x",
                    calls: Arc::new(AtomicUsize::new(0)),
                }),
            )
            .unwrap_err();
        match err {
            BlockError::Bus(msg) => assert!(msg.contains("bus.on")),
            other => panic!("expected Bus err, got {other:?}"),
        }
        let err = bus
            .on_any(Arc::new(RecordingHandler {
                label: "x",
                calls: Arc::new(AtomicUsize::new(0)),
            }))
            .unwrap_err();
        match err {
            BlockError::Bus(msg) => assert!(msg.contains("bus.on_any")),
            other => panic!("expected Bus err, got {other:?}"),
        }
        assert_eq!(bus.handler_count(), 0);
    }

    #[tokio::test]
    async fn duplicate_on_is_last_write_wins() {
        let (tx, rx) = mpsc::channel::<Event>(2);
        let mut bus = EventBus::new(rx);
        let first_calls = Arc::new(AtomicUsize::new(0));
        let second_calls = Arc::new(AtomicUsize::new(0));
        bus.on(
            "mesh",
            Arc::new(RecordingHandler {
                label: "first",
                calls: first_calls.clone(),
            }),
        )
        .unwrap();
        bus.on(
            "mesh",
            Arc::new(RecordingHandler {
                label: "second",
                calls: second_calls.clone(),
            }),
        )
        .unwrap();
        assert_eq!(bus.handler_count(), 1);

        let token = CancellationToken::new();
        let token_clone = token.clone();
        let handle = tokio::spawn(async move { bus.run(token_clone).await });

        let ack = send_event(&tx, "mesh", "e1");
        let got = ack.await.unwrap().unwrap();
        assert_eq!(got, Value::String("second".into()));
        assert_eq!(first_calls.load(Ordering::SeqCst), 0);
        assert_eq!(second_calls.load(Ordering::SeqCst), 1);

        token.cancel();
        drop(tx);
        handle.await.unwrap().unwrap();
    }
}
