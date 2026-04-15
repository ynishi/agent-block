//! Task spawning plumbing.
//!
//! [`spawn_into`] is the single entry point that both `std.task.spawn` and
//! `ScopeHandle::spawn` route through.  It installs the task-local stack
//! (`TASK_TOKEN` / `TASK_INFO` / `LOCAL_SCOPE`), wraps the user function
//! with a [`tracing::info_span!`], and hands the resulting future to
//! `tokio::task::spawn_local` via the panic-catching [`Catch`] adapter.
//!
//! [`Driver`] selects how the user function is driven: `AsyncFn` uses
//! `Function::call_async` (mlua's async bridge), `Coroutine` drives a raw
//! Lua thread via `Thread::resume`.  The coroutine driver is kept here
//! because it shares the cancel-aware sleep plumbing with `spawn_into`
//! and has no other caller.
//!
//! [`Handle`] is the `UserData` surface returned by `spawn`; [`Catch`] is
//! the panic-safe adapter that ensures a panicking task reports
//! `"task panicked"` rather than silently degrading to the abort path.

use std::cell::RefCell;
use std::future::Future;
use std::panic;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Instant;

use mlua::prelude::*;
use mlua::{
    Function, MultiValue, ThreadStatus, UserData, UserDataMethods, UserDataRegistry, Value,
};
use tokio::sync::oneshot;
use tokio::task::AbortHandle;
use tracing::{info_span, Instrument};

use super::cancel::race_sleep;
use super::scope::Scope;
use super::{
    duration_to_ms, lua_to_string, ms_to_duration, TaskInfo, LOCAL_SCOPE, TASK_INFO, TASK_TOKEN,
};

#[derive(Clone, Copy, Debug)]
pub(super) enum Driver {
    AsyncFn,
    Coroutine,
}

pub(super) fn parse_opts(opts: Option<&LuaTable>) -> LuaResult<(Option<String>, Option<Driver>)> {
    let Some(t) = opts else {
        return Ok((None, None));
    };

    // Single-pass: walk pairs once, dispatch per known key, reject unknowns.
    // Typos (`drivr = ...`) fail loudly via the catch-all arm.
    let mut name: Option<String> = None;
    let mut driver: Option<Driver> = None;
    for pair in t.clone().pairs::<LuaValue, LuaValue>() {
        let (k, v) = pair?;
        let key = match &k {
            LuaValue::String(s) => s.to_str()?.to_string(),
            other => {
                return Err(LuaError::external(format!(
                    "std.task: opts keys must be strings, got {}",
                    other.type_name()
                )));
            }
        };
        match key.as_str() {
            "name" => {
                name = Some(lua_to_string(&v, "std.task: opts.name")?);
            }
            "driver" => {
                let s = lua_to_string(&v, "std.task: opts.driver")?;
                driver = Some(match s.as_str() {
                    "coroutine" => Driver::Coroutine,
                    "async_fn" | "async" => Driver::AsyncFn,
                    other => {
                        return Err(LuaError::external(format!(
                            "std.task: unknown driver '{other}' (expected 'async_fn' or 'coroutine')"
                        )));
                    }
                });
            }
            other => {
                return Err(LuaError::external(format!(
                    "std.task: unknown opts key '{other}' (expected 'name' or 'driver')"
                )));
            }
        }
    }
    Ok((name, driver))
}

pub(super) fn default_driver() -> Driver {
    match std::env::var("AGENT_BLOCK_TASK_DRIVER").ok().as_deref() {
        Some("coroutine") => Driver::Coroutine,
        _ => Driver::AsyncFn,
    }
}

pub(super) enum JoinState {
    Pending(oneshot::Receiver<LuaResult<Value>>),
    Taken,
}

pub(super) struct Handle {
    pub(super) id: String,
    pub(super) name: Option<String>,
    pub(super) abort: AbortHandle,
    pub(super) state: JoinState,
    pub(super) started_at: Instant,
}

impl UserData for Handle {
    fn register(reg: &mut UserDataRegistry<Self>) {
        reg.add_field_method_get("id", |_, this| Ok(this.id.clone()));
        reg.add_field_method_get("name", |_, this| Ok(this.name.clone()));

        reg.add_method("is_finished", |_, this, ()| Ok(this.abort.is_finished()));

        reg.add_method("elapsed", |_, this, ()| {
            Ok(duration_to_ms(this.started_at.elapsed()))
        });

        reg.add_method("abort", |_, this, ()| {
            this.abort.abort();
            Ok(())
        });

        // Non-capturing closure: the UserData `mut` borrow is released when
        // this closure returns, before the returned future is polled — so
        // concurrent `is_finished` / `abort` calls on the same handle do
        // not hit a `UserDataBorrowMut` error.
        reg.add_async_method_mut("join", |_, mut this, ()| {
            let state = std::mem::replace(&mut this.state, JoinState::Taken);
            async move {
                match state {
                    JoinState::Pending(rx) => match rx.await {
                        Ok(res) => res,
                        Err(_) => Err(LuaError::external("task aborted")),
                    },
                    JoinState::Taken => Err(LuaError::external("task already joined")),
                }
            }
        });
    }
}

pub(super) fn spawn_into(
    lua: &Lua,
    scope: &Rc<RefCell<Scope>>,
    func: Function,
    opts: Option<LuaTable>,
) -> LuaResult<Handle> {
    let (name, driver_opt) = parse_opts(opts.as_ref())?;
    let driver = driver_opt.unwrap_or_else(default_driver);
    let token = scope.borrow().token.clone();

    let (tx, rx) = oneshot::channel::<LuaResult<Value>>();

    // Shared id for tracing span and TaskInfo.  Thread-local counter; the
    // contract (§`register`) is single-thread LocalSet, so ids are unique
    // per VM.
    let id = format!("t{}", TASK_SEQ.with(|s| s.next()));
    let info = TaskInfo {
        id: id.clone(),
        name: name.clone(),
    };

    let lua_for_cr = lua.clone();
    let user_fut: Pin<Box<dyn Future<Output = LuaResult<Value>>>> = match driver {
        Driver::AsyncFn => {
            Box::pin(async move { func.call_async::<Value>(MultiValue::new()).await })
        }
        Driver::Coroutine => Box::pin(async move { run_coroutine(&lua_for_cr, func).await }),
    };

    // Wrap with tracing span first so it observes task_local enter/exit.
    let span = info_span!(
        "task",
        id = %id,
        name = name.as_deref().unwrap_or(""),
        driver = ?driver,
    );
    let traced = user_fut.instrument(span);
    let with_info = TASK_INFO.scope(info, traced);
    let with_token = TASK_TOKEN.scope(token, with_info);
    let with_scope = LOCAL_SCOPE.scope(scope.clone(), with_token);

    // Catch wraps the whole stack and owns `tx` — so a panic is reported as
    // `task panicked`, distinct from `task aborted` (which the receiver sees
    // when the sender is dropped because the task was aborted).
    let catching = Catch {
        inner: Box::pin(with_scope),
        tx: Some(tx),
    };
    let join_handle = tokio::task::spawn_local(catching);
    let abort = join_handle.abort_handle();

    scope.borrow_mut().attach(join_handle);

    Ok(Handle {
        id,
        name,
        abort,
        state: JoinState::Pending(rx),
        started_at: Instant::now(),
    })
}

thread_local! {
    static TASK_SEQ: SeqGen = SeqGen::default();
}

#[derive(Default)]
struct SeqGen(std::cell::Cell<u64>);
impl SeqGen {
    fn next(&self) -> u64 {
        let v = self.0.get().wrapping_add(1);
        self.0.set(v);
        v
    }
}

/// Drive `func` as a raw Lua coroutine.  The Lua body uses:
///
/// - `coroutine.yield()` / `coroutine.yield(nil)` — cooperative yield
/// - `coroutine.yield(ms)` where ms is a number — sleep `ms` milliseconds
///
/// Between resumes, we check the task-local cancellation token and, if set,
/// raise `task cancelled`.  During a yield-sleep we race the sleep against
/// the token's `Notify`, so `scope:cancel()` breaks out immediately rather
/// than waiting for the sleep to elapse.
///
/// Return-value contract: `Handle::join()` yields `LuaResult<Value>`, a single
/// value, so this driver exposes only the first of a Lua `return a, b, c`.
/// This matches the `async_fn` driver.  Wrap multi-value results in a table
/// if the caller needs to observe them.
async fn run_coroutine(lua: &Lua, func: Function) -> LuaResult<Value> {
    let thread = lua.create_thread(func)?;
    loop {
        if TASK_TOKEN.try_with(|t| t.is_cancelled()).unwrap_or(false) {
            return Err(LuaError::external("task cancelled"));
        }

        let yielded: MultiValue = thread.resume(MultiValue::new())?;

        match thread.status() {
            ThreadStatus::Finished => {
                // Contract: the coroutine driver returns the first return value
                // only, mirroring the async_fn driver (whose signature is
                // `LuaResult<Value>`).  `return a, b, c` in the Lua body drops
                // `b, c`; wrap multi-value results in a table if the caller
                // needs to observe them.
                return Ok(yielded.into_iter().next().unwrap_or(Value::Nil));
            }
            ThreadStatus::Resumable => {
                let ctrl = yielded.into_iter().next().unwrap_or(Value::Nil);
                match ctrl {
                    Value::Nil => tokio::task::yield_now().await,
                    Value::Integer(ms) => {
                        // Route through ms_to_duration so Integer and Number
                        // paths reject negative / non-finite values identically;
                        // `coroutine.yield(-1)` must not silently become 0ms.
                        let dur = ms_to_duration(ms as f64, "coroutine yield")?;
                        race_sleep(dur).await?;
                    }
                    Value::Number(ms) => {
                        let dur = ms_to_duration(ms, "coroutine yield")?;
                        race_sleep(dur).await?;
                    }
                    other => {
                        return Err(LuaError::external(format!(
                            "coroutine yield: unsupported value type '{}' (expected nil or number)",
                            other.type_name()
                        )));
                    }
                }
            }
            ThreadStatus::Running => {
                return Err(LuaError::external(
                    "coroutine in Running state after resume (impossible)",
                ));
            }
            ThreadStatus::Error => {
                return Err(LuaError::external("coroutine entered Error state"));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Panic-catching adapter.  Owns the result channel so a panicking task
// reports `task panicked` rather than silently degrading to the `RecvError`
// path (which is reserved for legitimate aborts).
// ---------------------------------------------------------------------------

struct Catch<F> {
    inner: Pin<Box<F>>,
    tx: Option<oneshot::Sender<LuaResult<Value>>>,
}

impl<F: Future<Output = LuaResult<Value>>> Future for Catch<F> {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Self is Unpin: `Pin<Box<F>>` is Unpin and `Option<oneshot::Sender>`
        // is Unpin, so we can freely project through `get_mut`.
        let this = self.as_mut().get_mut();
        match panic::catch_unwind(panic::AssertUnwindSafe(|| this.inner.as_mut().poll(cx))) {
            Ok(Poll::Ready(result)) => {
                if let Some(tx) = this.tx.take() {
                    let _ = tx.send(result);
                }
                Poll::Ready(())
            }
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => {
                if let Some(tx) = this.tx.take() {
                    let _ = tx.send(Err(LuaError::external("task panicked")));
                }
                Poll::Ready(())
            }
        }
    }
}
