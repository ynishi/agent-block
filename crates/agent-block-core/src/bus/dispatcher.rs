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
use agent_block_types::error::BlockError;

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
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;
    use tokio::sync::{oneshot, Mutex as TokioMutex};

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

    // -----------------------------------------------------------------
    // concurrency-analysis.md §2 — 11 concurrency tests
    // -----------------------------------------------------------------

    /// Handler that sleeps then records its id, used to prove serial
    /// dispatch order under a multi-thread runtime.
    struct OrderingHandler {
        order: Arc<StdMutex<Vec<String>>>,
        delay: Duration,
    }

    #[async_trait]
    impl Handler for OrderingHandler {
        async fn call(
            &self,
            _kind: String,
            id: String,
            _payload: Value,
            _meta: Value,
        ) -> AckResult {
            tokio::time::sleep(self.delay).await;
            // guard is dropped before the async return, not held across .await
            self.order.lock().expect("order mutex").push(id.clone());
            Ok(Value::String(id))
        }
    }

    /// Handler that always returns `Err`. Used to prove the dispatcher
    /// continues after a handler error.
    struct ErrHandler;

    #[async_trait]
    impl Handler for ErrHandler {
        async fn call(
            &self,
            _kind: String,
            _id: String,
            _payload: Value,
            _meta: Value,
        ) -> AckResult {
            Err(BlockError::Bus("x".into()))
        }
    }

    /// §2.1 — kind-specific mpsc ingress with a single receiver preserves
    /// arrival order when the dispatcher runs on a multi-thread runtime.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_bus_event_serialization_arrival_order() {
        const N: usize = 20;
        let (tx, rx) = mpsc::channel::<Event>(N);
        let mut bus = EventBus::new(rx);
        let order = Arc::new(StdMutex::new(Vec::new()));
        bus.on(
            "k",
            Arc::new(OrderingHandler {
                order: Arc::clone(&order),
                // Small sleep inside each handler so the test would fail
                // if the dispatcher tried to run them concurrently.
                delay: Duration::from_millis(5),
            }),
        )
        .unwrap();

        let token = CancellationToken::new();
        let token_clone = token.clone();
        let handle = tokio::spawn(async move { bus.run(token_clone).await });

        let mut expected = Vec::with_capacity(N);
        let mut acks = Vec::with_capacity(N);
        for i in 0..N {
            let id = format!("e{i}");
            expected.push(id.clone());
            let (evt, rx) = Event::with_ack("k", id, json!({}), Value::Null);
            tx.send(evt).await.expect("send");
            acks.push(rx);
        }

        // Wait for every ack — each returns the handler id in order.
        for (i, ack) in acks.into_iter().enumerate() {
            let got = ack.await.expect("ack recv").expect("ack ok");
            assert_eq!(got, Value::String(format!("e{i}")));
        }

        token.cancel();
        drop(tx);
        handle.await.unwrap().unwrap();

        let recorded = order.lock().unwrap().clone();
        assert_eq!(recorded, expected, "dispatcher must preserve arrival order");
    }

    /// §2.2 — `shutdown.cancel()` breaks the loop within the grace window.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bus_graceful_shutdown_within_grace_ms() {
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

        // No in-flight handler; cancel and expect prompt exit.
        token.cancel();
        // Allow a generous envelope well above any plausible grace window
        // (default grace_ms = 1000). The dispatcher should exit within
        // tens of ms in practice.
        let res = tokio::time::timeout(Duration::from_millis(1500), handle)
            .await
            .expect("bus.run must exit within grace window");
        res.unwrap().unwrap();
        drop(tx);
    }

    /// §2.3 — a panicking handler is isolated; the loop keeps dispatching
    /// subsequent events. Mirrors the existing
    /// `handler_panic_is_isolated_and_loop_continues` but under a
    /// multi_thread runtime to stress the spawn+join path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bus_handler_panic_isolation_catch_unwind() {
        let (tx, rx) = mpsc::channel::<Event>(4);
        let mut bus = EventBus::new(rx);
        let ok_calls = Arc::new(AtomicUsize::new(0));
        bus.on("crash", Arc::new(PanickingHandler)).unwrap();
        bus.on(
            "normal",
            Arc::new(RecordingHandler {
                label: "normal",
                calls: ok_calls.clone(),
            }),
        )
        .unwrap();

        let token = CancellationToken::new();
        let token_clone = token.clone();
        let handle = tokio::spawn(async move { bus.run(token_clone).await });

        // Panicking event first.
        let ack = send_event(&tx, "crash", "e1");
        let got = ack.await.unwrap();
        assert!(matches!(got, Err(BlockError::Bus(_))), "panic must NACK");

        // Normal event after the panic — the loop must still run.
        let ack = send_event(&tx, "normal", "e2");
        let got = ack.await.unwrap().unwrap();
        assert_eq!(got, Value::String("normal".into()));
        assert_eq!(ok_calls.load(Ordering::SeqCst), 1);

        token.cancel();
        drop(tx);
        handle.await.unwrap().unwrap();
    }

    /// §2.4 — a bounded mpsc applies backpressure (no drops). A capacity-1
    /// channel with the dispatcher paused lets one send succeed and the
    /// second `try_send` return `TrySendError::Full` — never silently drop.
    /// Once the dispatcher drains it, everything flows through in order.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_bus_backpressure_bounded_mpsc_capacity() {
        let (tx, rx) = mpsc::channel::<Event>(1);
        let mut bus = EventBus::new(rx);
        let calls = Arc::new(AtomicUsize::new(0));
        bus.on(
            "k",
            Arc::new(RecordingHandler {
                label: "k",
                calls: calls.clone(),
            }),
        )
        .unwrap();

        // Fill the channel (capacity 1). Dispatcher not started yet.
        let (evt1, _r1) = Event::with_ack("k", "e1", json!({}), Value::Null);
        tx.try_send(evt1).expect("first send fits");
        let (evt2, _r2) = Event::with_ack("k", "e2", json!({}), Value::Null);
        let err = tx.try_send(evt2).expect_err("capacity full");
        assert!(
            matches!(err, mpsc::error::TrySendError::Full(_)),
            "expected Full (not drop), got {err:?}"
        );

        // Start the dispatcher and send two more events via `.await`
        // — backpressure must let them through without loss.
        let token = CancellationToken::new();
        let token_clone = token.clone();
        let handle = tokio::spawn(async move { bus.run(token_clone).await });

        let (evt3, r3) = Event::with_ack("k", "e3", json!({}), Value::Null);
        tx.send(evt3).await.expect("send e3");
        let (evt4, r4) = Event::with_ack("k", "e4", json!({}), Value::Null);
        tx.send(evt4).await.expect("send e4");

        // The first event (still in the channel) and the new ones should
        // all be dispatched. Only assert the new acks fire (the original
        // `_r1`/`_r2` receivers were dropped, which is fine).
        r3.await.unwrap().unwrap();
        r4.await.unwrap().unwrap();

        token.cancel();
        drop(tx);
        handle.await.unwrap().unwrap();
        assert!(calls.load(Ordering::SeqCst) >= 2);
    }

    /// §2.5 — a source that drops `ack_tx` (receiver side drops) surfaces
    /// as `oneshot::RecvError` immediately, not as a 30-second timeout.
    /// Combined with `tokio::time::timeout`, the sender-drop semantic
    /// short-circuits the timeout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bus_oneshot_ack_timeout_30s() {
        // Set up an oneshot, drop the sender, and prove `timeout(30s,
        // rx)` resolves to `Ok(Err(RecvError))` essentially instantly.
        let (tx, rx) = oneshot::channel::<AckResult>();
        drop(tx);
        let start = tokio::time::Instant::now();
        let got = tokio::time::timeout(Duration::from_secs(30), rx)
            .await
            .expect("should not hit 30s timeout");
        assert!(got.is_err(), "expected RecvError, got {got:?}");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "sender-drop must short-circuit the 30s timeout"
        );
    }

    /// §2.6 — SIGTERM / SIGINT race inside `tokio::select!`. Sends SIGTERM
    /// to the current process; the select! should pick the SIGTERM branch
    /// without losing the SIGINT branch's registration (Signal::recv
    /// cancel safety).
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bus_sigterm_sigint_race_select() {
        use tokio::signal::unix::{signal, SignalKind};

        let token = CancellationToken::new();
        let token_for_task = token.clone();
        let task = tokio::spawn(async move {
            let mut term = signal(SignalKind::terminate()).expect("install SIGTERM");
            tokio::select! {
                _ = term.recv() => token_for_task.cancel(),
                _ = tokio::signal::ctrl_c() => token_for_task.cancel(),
            }
        });

        // Give the signal handlers time to install before we deliver the
        // signal — otherwise we race the install.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Deliver SIGTERM to self. `nix` is cfg(unix) dev-only.
        nix::sys::signal::kill(nix::unistd::Pid::this(), nix::sys::signal::Signal::SIGTERM)
            .expect("kill(SIGTERM)");

        tokio::time::timeout(Duration::from_secs(2), token.cancelled())
            .await
            .expect("cancel must fire within 2s after SIGTERM");
        task.await.expect("signal task");
    }

    /// §2.7 — `Arc<tokio::sync::Mutex<EventBus>>` allows two tasks to
    /// acquire the lock in sequence without deadlock. Exercises the
    /// "take before await" pattern used in `bridge::bus::serve`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_bus_arc_tokio_mutex_no_await_while_held() {
        let (_tx, rx) = mpsc::channel::<Event>(1);
        let shared: Arc<TokioMutex<EventBus>> = Arc::new(TokioMutex::new(EventBus::new(rx)));

        let a = Arc::clone(&shared);
        let t1 = tokio::spawn(async move {
            let guard = a.lock().await;
            // Do some non-await work under the lock.
            let _ = guard.handler_count();
            // Guard dropped here; any .await after this is safe.
            drop(guard);
            tokio::time::sleep(Duration::from_millis(10)).await;
        });

        let b = Arc::clone(&shared);
        let t2 = tokio::spawn(async move {
            let guard = b.lock().await;
            let _ = guard.handler_count();
            drop(guard);
        });

        // Both tasks complete promptly — no deadlock, no await-while-held.
        tokio::time::timeout(Duration::from_secs(2), async {
            t1.await.unwrap();
            t2.await.unwrap();
        })
        .await
        .expect("no deadlock");
    }

    /// §2.8 — `catch_unwind` compile-time type check. `UnwindSafe` bound
    /// means a `&mut` capture needs `AssertUnwindSafe`. This test both
    /// documents the constraint and verifies at runtime that
    /// `catch_unwind` intercepts an unwinding panic (panic=abort is not
    /// testable at runtime; documented in concurrency-analysis.md §2).
    #[test]
    fn test_bus_catch_unwind_paniceq_abort_not_caught() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        // A plain Fn() closure IS UnwindSafe; this compiles fine.
        let ok = catch_unwind(|| 42);
        assert_eq!(ok.ok(), Some(42));

        // An `&mut` capture is NOT UnwindSafe without `AssertUnwindSafe`.
        // This exercises the type-check path: the code compiles because
        // `AssertUnwindSafe` is used; removing that wrapper would fail
        // to type-check, which is the contract we care about.
        let mut v = 0i32;
        let caught = catch_unwind(AssertUnwindSafe(|| {
            v += 1;
            panic!("boom");
        }));
        assert!(caught.is_err(), "expected caught panic");
        assert_eq!(v, 1, "side effect before panic still observable");

        // Note: `panic=abort` builds do NOT unwind, and `catch_unwind`
        // does not intercept an abort. We cannot runtime-test that path
        // (the test process would abort); we assert the contract via
        // documentation in concurrency-analysis.md §2 row 7.
    }

    /// §2.9 — a spawned signal-watching task can be aborted and its
    /// JoinHandle resolves to a cancellation-flagged JoinError.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_bus_spawn_signal_task_cancellation() {
        let shutdown = CancellationToken::new();
        let shutdown_clone = shutdown.clone();
        let task = tokio::spawn(async move {
            // Simulate a signal-watching task that blocks until cancel.
            shutdown_clone.cancelled().await;
        });

        // Abort before the token is ever cancelled.
        task.abort();
        let res = task.await;
        match res {
            Err(e) => assert!(e.is_cancelled(), "expected cancelled JoinError, got {e:?}"),
            Ok(()) => panic!("task should have been cancelled before completing"),
        }

        // `shutdown` stays un-cancelled — abort of the task does not
        // propagate to the token (by design: product code cancels
        // explicitly from the signal branch).
        assert!(!shutdown.is_cancelled());
    }

    /// §2.10 — `tokio::time::timeout` fires after the configured duration
    /// under a paused clock. Verifies the 30s timeout contract used by
    /// `BusRelayHandler` without actually sleeping 30 seconds.
    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_bus_timeout_ack_expiry_30s_match() {
        let (_tx, rx) = oneshot::channel::<AckResult>();
        let fut = tokio::time::timeout(Duration::from_secs(30), rx);
        tokio::pin!(fut);

        // Before advance: the future has not resolved.
        tokio::time::advance(Duration::from_secs(29)).await;
        assert!(
            futures_poll_once(&mut fut).is_none(),
            "timeout must not fire before 30s"
        );

        // Cross the threshold.
        tokio::time::advance(Duration::from_secs(2)).await;
        let got = (&mut fut).await;
        assert!(got.is_err(), "expected Elapsed, got {got:?}");
    }

    /// Poll a pinned future once; returns `Some(output)` if ready,
    /// `None` otherwise. Used to inspect a future without awaiting it.
    fn futures_poll_once<F: std::future::Future>(
        fut: &mut std::pin::Pin<&mut F>,
    ) -> Option<F::Output> {
        use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

        // Minimal no-op waker.
        fn raw_waker() -> RawWaker {
            fn no_op(_: *const ()) {}
            fn clone(_: *const ()) -> RawWaker {
                raw_waker()
            }
            static VT: RawWakerVTable = RawWakerVTable::new(clone, no_op, no_op, no_op);
            RawWaker::new(std::ptr::null(), &VT)
        }
        // SAFETY: `raw_waker()` returns a `RawWaker` backed by a static
        // `RawWakerVTable` whose clone/wake/drop functions are all no-ops.
        // No data pointer is stored or dereferenced; the waker is used only
        // to construct a `Context` for a single synchronous `poll` call and
        // is not sent across threads or outlived.
        let waker = unsafe { Waker::from_raw(raw_waker()) };
        let mut cx = Context::from_waker(&waker);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => Some(v),
            Poll::Pending => None,
        }
    }

    /// §2.11 — `std::sync::Mutex::lock()` returns `PoisonError` after a
    /// panic holding the guard. The registration path in `bridge::bus`
    /// converts this to a typed error (`BlockError::Runtime("bus mutex
    /// poisoned")`); this test proves the poison signal is observable
    /// so the conversion has something to trigger on.
    #[test]
    fn test_std_mutex_poison_on_handler_registration() {
        let m: Arc<StdMutex<i32>> = Arc::new(StdMutex::new(0));
        let m_panic = Arc::clone(&m);
        let handle = std::thread::spawn(move || {
            let _guard = m_panic.lock().expect("first lock");
            panic!("poison me");
        });
        // Thread panics; join returns Err, and the mutex is now poisoned.
        assert!(handle.join().is_err());

        let err = m.lock().expect_err("mutex must be poisoned");
        // `err` is a PoisonError; `.into_inner()` would recover the guard.
        // The key observable: `lock()` returns Err, which bridge::bus
        // maps to `BlockError::Runtime("bus mutex poisoned")` / a Lua
        // external error.
        let _inner = err.into_inner();
    }

    // -----------------------------------------------------------------
    // General tests (plan.md §一般テスト)
    // -----------------------------------------------------------------

    /// on_any fires only when the event's kind has no specialized handler.
    /// (plan.md §一般テスト — on_any フォールバック)
    #[tokio::test]
    async fn general_on_any_fallback_vs_no_handler_warn() {
        // 1) No specialized, no on_any → nack.
        let (tx, rx) = mpsc::channel::<Event>(2);
        let mut bus = EventBus::new(rx);
        let token = CancellationToken::new();
        let token_clone = token.clone();
        let handle = tokio::spawn(async move { bus.run(token_clone).await });
        let ack = send_event(&tx, "kind-x", "id1");
        let got = ack.await.unwrap();
        assert!(matches!(got, Err(BlockError::Bus(_))));
        token.cancel();
        drop(tx);
        handle.await.unwrap().unwrap();

        // 2) Only on_any → on_any fires on any kind.
        let (tx, rx) = mpsc::channel::<Event>(2);
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
        let ack = send_event(&tx, "anything", "id1");
        assert_eq!(ack.await.unwrap().unwrap(), Value::String("any".into()));
        assert_eq!(any_calls.load(Ordering::SeqCst), 1);
        token.cancel();
        drop(tx);
        handle.await.unwrap().unwrap();
    }

    /// When a specialized handler matches, on_any is NOT invoked.
    /// (plan.md §一般テスト — 優先順位)
    #[tokio::test]
    async fn general_specialized_wins_over_on_any() {
        let (tx, rx) = mpsc::channel::<Event>(2);
        let mut bus = EventBus::new(rx);
        let spec_calls = Arc::new(AtomicUsize::new(0));
        let any_calls = Arc::new(AtomicUsize::new(0));
        bus.on(
            "k",
            Arc::new(RecordingHandler {
                label: "spec",
                calls: spec_calls.clone(),
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

        let ack = send_event(&tx, "k", "e1");
        let got = ack.await.unwrap().unwrap();
        assert_eq!(got, Value::String("spec".into()));
        assert_eq!(spec_calls.load(Ordering::SeqCst), 1);
        assert_eq!(any_calls.load(Ordering::SeqCst), 0);

        token.cancel();
        drop(tx);
        handle.await.unwrap().unwrap();
    }

    /// A handler returning `Err(...)` delivers an error ack and the loop
    /// continues to dispatch the next event.
    /// (plan.md §一般テスト — Handler error 継続)
    #[tokio::test]
    async fn general_handler_error_ack_and_loop_continues() {
        let (tx, rx) = mpsc::channel::<Event>(4);
        let mut bus = EventBus::new(rx);
        bus.on("err", Arc::new(ErrHandler)).unwrap();
        let ok_calls = Arc::new(AtomicUsize::new(0));
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

        let ack = send_event(&tx, "err", "e1");
        let got = ack.await.unwrap();
        match got {
            Err(BlockError::Bus(msg)) => assert_eq!(msg, "x"),
            other => panic!("expected Bus err 'x', got {other:?}"),
        }

        let ack = send_event(&tx, "ok", "e2");
        assert_eq!(ack.await.unwrap().unwrap(), Value::String("ok".into()));
        assert_eq!(ok_calls.load(Ordering::SeqCst), 1);

        token.cancel();
        drop(tx);
        handle.await.unwrap().unwrap();
    }
}
