//! Cooperative cancellation primitives.
//!
//! [`CancelToken`] is the Rc-shared boolean + [`tokio::sync::Notify`] pair
//! that powers every cancel checkpoint in the task bridge.  [`effective_token`]
//! resolves "the token that applies to the currently-running code" by walking
//! the task-local stack (`TASK_TOKEN` first, falling back to
//! `LOCAL_SCOPE.token`).  [`race_sleep`] and [`race_yield`] are the two
//! suspension primitives every public `std.task.*` sleep/yield routes
//! through — they observe the effective token on every wake.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use mlua::prelude::*;
use mlua::{UserData, UserDataMethods, UserDataRegistry};
use tokio::sync::Notify;

use super::{LOCAL_SCOPE, TASK_TOKEN};

pub(crate) struct CancelTokenInner {
    cancelled: Cell<bool>,
    notify: Notify,
}

#[derive(Clone)]
pub(crate) struct CancelToken(Rc<CancelTokenInner>);

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelToken {
    pub(crate) fn new() -> Self {
        Self(Rc::new(CancelTokenInner {
            cancelled: Cell::new(false),
            notify: Notify::new(),
        }))
    }
    pub(crate) fn cancel(&self) {
        self.0.cancelled.set(true);
        self.0.notify.notify_waiters();
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.cancelled.get()
    }
    /// Resolve when `cancel()` is called (or immediately if already cancelled).
    ///
    /// Uses `Notified::enable()` to register as a waiter before the final
    /// `is_cancelled` check, so a `cancel()` racing with this call cannot
    /// be missed — either the flag check sees it, or `notify_waiters()`
    /// wakes the already-queued `Notified`.
    pub(crate) async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.0.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

impl UserData for CancelToken {
    fn register(reg: &mut UserDataRegistry<Self>) {
        reg.add_method("is_cancelled", |_, this, ()| Ok(this.is_cancelled()));
        reg.add_method("cancel", |_, this, ()| {
            this.cancel();
            Ok(())
        });
        reg.add_method("check", |_, this, ()| {
            if this.is_cancelled() {
                Err(LuaError::external("task cancelled"))
            } else {
                Ok(())
            }
        });
    }
}

/// Pick the cancel token that applies to the currently-running code.
///
/// - Inside a task spawned by `spawn_into`, returns the scope's token
///   installed as `TASK_TOKEN` (same `Rc` as `LOCAL_SCOPE.token`).
/// - Inside a `task.scope` / `task.with_timeout` body running in the
///   caller's task, `TASK_TOKEN` is unset; falls back to
///   `LOCAL_SCOPE.token` so the body observes scope cancellation.
/// - Outside any scope, returns `None` (primitive runs uncancellable).
pub(crate) fn effective_token() -> Option<CancelToken> {
    if let Ok(t) = TASK_TOKEN.try_with(|t| t.clone()) {
        return Some(t);
    }
    LOCAL_SCOPE.try_with(|s| s.borrow().token.clone()).ok()
}

/// Sleep for `dur`, but short-circuit with `Err("task cancelled")` if the
/// effective cancel token (see [`effective_token`]) fires during the wait.
/// Falls back to a plain sleep when no token is installed.
pub(super) async fn race_sleep(dur: Duration) -> LuaResult<()> {
    match effective_token() {
        None => {
            tokio::time::sleep(dur).await;
            Ok(())
        }
        Some(t) => {
            tokio::select! {
                biased;
                _ = t.cancelled() => {
                    Err(LuaError::external("task cancelled"))
                }
                _ = tokio::time::sleep(dur) => {
                    if t.is_cancelled() {
                        Err(LuaError::external("task cancelled"))
                    } else {
                        Ok(())
                    }
                }
            }
        }
    }
}

/// Yield to the scheduler once and then observe the effective cancel token.
/// Mirrors Trio's "every checkpoint can raise Cancelled" contract.
pub(super) async fn race_yield() -> LuaResult<()> {
    tokio::task::yield_now().await;
    if let Some(t) = effective_token() {
        if t.is_cancelled() {
            return Err(LuaError::external("task cancelled"));
        }
    }
    Ok(())
}
