//! Structured concurrency container.
//!
//! [`Scope`] owns a set of [`JoinHandle`]s and a shared [`CancelToken`].
//! [`ScopeGuard`] is the RAII handle that aborts orphaned children if a
//! scope future is dropped mid-await; [`drain_scope`] is the cooperative
//! reap loop and [`abort_all`] is the hard-abort escape used by
//! `task.with_timeout`'s grace-expiry stage.
//!
//! [`ScopeHandle`] is the Lua-facing `UserData` surface (the value bound
//! to `scope` in `std.task.scope(function(scope) … end)`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use mlua::prelude::*;
use mlua::{Function, UserData, UserDataMethods, UserDataRegistry};
use tokio::task::JoinHandle;

use super::cancel::CancelToken;
use super::driver::{spawn_into, Handle};
use super::LOCAL_SCOPE;

pub(super) struct Scope {
    pub(super) name: Option<String>,
    pub(super) token: CancelToken,
    /// JoinHandles for children.  We keep JoinHandles (not just AbortHandles)
    /// so the scope can `.await` them to implement structured concurrency.
    pub(super) children: Vec<JoinHandle<()>>,
}

impl Scope {
    pub(super) fn new(name: Option<String>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            name,
            token: CancelToken::new(),
            children: Vec::new(),
        }))
    }

    pub(super) fn attach(&mut self, h: JoinHandle<()>) {
        // Amortized GC: only sweep finished children when the list has grown
        // past a threshold, so every spawn isn't O(n) in live children.
        if self.children.len() >= 32 {
            self.children.retain(|h| !h.is_finished());
        }
        self.children.push(h);
    }
}

impl Drop for Scope {
    /// Last-resort cleanup for the root scope (dropped on VM teardown) and
    /// any leaked clone.  Nested scopes created by `task.scope` /
    /// `task.with_timeout` rely on `ScopeGuard` (below) to abort children
    /// eagerly on drop, because children themselves hold `Rc` clones of
    /// the scope via `LOCAL_SCOPE` and would otherwise keep the refcount
    /// above zero until they finish running.
    fn drop(&mut self) {
        for h in &self.children {
            h.abort();
        }
    }
}

/// RAII guard that aborts a scope's children when dropped.
///
/// Installed by `task.scope` / `task.with_timeout` at the top of their
/// async body.  The normal-completion path (after `drain_scope` returns)
/// calls [`ScopeGuard::disarm`], so the guard becomes a no-op on drop and
/// the scope's [`CancelToken`] stays in its body-final state (`cancel()`
/// on error/timeout propagation, untouched on success).  If the body is
/// dropped mid-await (outer abort, VM teardown), the guard is still armed
/// and cancels the token + aborts orphaned children so they do not outlive
/// the scope.
///
/// Modelled on the `scopeguard` crate's `ScopeGuard::forget`-style disarm
/// pattern.
pub(super) struct ScopeGuard {
    scope: Rc<RefCell<Scope>>,
    armed: Cell<bool>,
}

impl ScopeGuard {
    pub(super) fn new(scope: Rc<RefCell<Scope>>) -> Self {
        Self {
            scope,
            armed: Cell::new(true),
        }
    }

    /// Mark the guard as completed so `Drop` performs no action.  Called
    /// after `drain_scope` returns on the normal exit path.
    pub(super) fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for ScopeGuard {
    fn drop(&mut self) {
        if !self.armed.get() {
            return;
        }
        let s = self.scope.borrow();
        s.token.cancel();
        for h in &s.children {
            h.abort();
        }
    }
}

/// Await every child to completion, then clear the scope's child list.
///
/// Called by `task.scope` / `task.with_timeout` when the user callback
/// returns.  On the error/timeout path the caller cancels `scope.token`
/// and invokes `abort_all` first, so non-cooperative children do not
/// block the drain.  Cooperative children observe the token on their
/// next `checkpoint` and return on their own.
pub(super) async fn drain_scope(scope: &Rc<RefCell<Scope>>) {
    loop {
        let next = { scope.borrow_mut().children.pop() };
        match next {
            Some(h) => {
                let _ = h.await;
            }
            None => break,
        }
    }
}

/// Abort all children immediately (non-cooperative).  Used by
/// `task.scope` / `with_timeout` on their error/timeout paths so the
/// following `drain_scope` does not wait on children that never reach a
/// `checkpoint`.
pub(super) fn abort_all(scope: &Rc<RefCell<Scope>>) {
    for h in &scope.borrow().children {
        h.abort();
    }
}

#[derive(Clone)]
pub(super) struct ScopeHandle(pub(super) Rc<RefCell<Scope>>);

impl UserData for ScopeHandle {
    fn register(reg: &mut UserDataRegistry<Self>) {
        reg.add_field_method_get("name", |_, this| Ok(this.0.borrow().name.clone()));

        reg.add_method("token", |_, this, ()| Ok(this.0.borrow().token.clone()));

        reg.add_method("cancel", |_, this, ()| {
            this.0.borrow().token.cancel();
            Ok(())
        });

        reg.add_method(
            "spawn",
            |lua, this, (func, opts): (Function, Option<LuaTable>)| -> LuaResult<Handle> {
                spawn_into(lua, &this.0, func, opts)
            },
        );
    }
}

pub(super) fn root_scope(lua: &Lua) -> LuaResult<Rc<RefCell<Scope>>> {
    lua.app_data_ref::<Rc<RefCell<Scope>>>()
        .map(|r| r.clone())
        .ok_or_else(|| LuaError::external("std.task not initialised"))
}

pub(super) fn current_scope(lua: &Lua) -> LuaResult<Rc<RefCell<Scope>>> {
    if let Ok(s) = LOCAL_SCOPE.try_with(|s| s.clone()) {
        return Ok(s);
    }
    root_scope(lua)
}
