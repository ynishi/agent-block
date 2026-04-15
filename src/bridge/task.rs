//! `std.task` — structured async task primitives for Lua scripts.
//!
//! Phase 1 surface:
//!
//! - `std.task.spawn(fn, opts?) -> Handle`
//! - `std.task.sleep(ms)`
//! - `std.task.yield()`
//! - `Handle`: `:join()`, `:abort()`, `:is_finished()`, `:elapsed()`, `.id`, `.name`
//!
//! # Runtime contract
//!
//! Must be called from within a `tokio::task::LocalSet` driven by a
//! current-thread runtime.  `mlua-isle::AsyncIsle` satisfies this.  `spawn`
//! uses `tokio::task::spawn_local`, so the task inherits the LocalSet and
//! runs on the same OS thread as the Lua VM (no `Send` bound on captures).
//!
//! # Root scope / VM lifetime
//!
//! Every spawn is registered with a VM-scoped [`RootScope`] held in Lua app
//! data.  When the Lua VM drops (AsyncIsle thread tears down), the root
//! scope drops with it and any outstanding `AbortHandle`s are triggered so
//! fire-and-forget tasks do not leak past the VM.
//!
//! # Driver selection
//!
//! Phase 1 only implements the `async_fn` driver (`Function::call_async`).
//! `AGENT_BLOCK_TASK_DRIVER=coroutine` and `opts.driver = "coroutine"` are
//! parsed and stored but not yet honoured; Phase 3 will add the
//! `create_thread` path.

use std::cell::RefCell;
use std::panic;
use std::rc::Rc;
use std::time::{Duration, Instant};

use mlua::prelude::*;
use mlua::{Function, MultiValue, UserData, UserDataMethods, UserDataRegistry, Value};
use tokio::sync::oneshot;
use tokio::task::AbortHandle;

// ---------------------------------------------------------------------------
// Root scope — VM-global registry of outstanding AbortHandles
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RootScope {
    /// Abort handles for tasks that have not yet signalled completion.
    /// GC'd opportunistically at each spawn.
    handles: Vec<AbortHandle>,
}

impl RootScope {
    fn attach(&mut self, handle: AbortHandle) {
        // Opportunistic GC: drop handles whose task already finished.
        self.handles.retain(|h| !h.is_finished());
        self.handles.push(handle);
    }
}

impl Drop for RootScope {
    fn drop(&mut self) {
        for h in self.handles.drain(..) {
            h.abort();
        }
    }
}

fn with_root_scope<F, R>(lua: &Lua, f: F) -> LuaResult<R>
where
    F: FnOnce(&mut RootScope) -> R,
{
    let cell = lua
        .app_data_ref::<Rc<RefCell<RootScope>>>()
        .ok_or_else(|| LuaError::external("std.task root scope not initialised"))?;
    let mut borrow = cell.borrow_mut();
    Ok(f(&mut borrow))
}

// ---------------------------------------------------------------------------
// Handle — UserData returned from std.task.spawn
// ---------------------------------------------------------------------------

enum JoinState {
    Pending(oneshot::Receiver<LuaResult<Value>>),
    Taken,
}

struct Handle {
    id: String,
    name: Option<String>,
    abort: AbortHandle,
    state: JoinState,
    started_at: Instant,
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

        reg.add_async_method_mut("join", |_, mut this, ()| async move {
            let state = std::mem::replace(&mut this.state, JoinState::Taken);
            let out = match state {
                JoinState::Pending(rx) => match rx.await {
                    Ok(res) => res,
                    Err(_) => Err(LuaError::external("task cancelled before completion")),
                },
                JoinState::Taken => Err(LuaError::external("task already joined")),
            };
            out
        });
    }
}

fn duration_to_ms(d: Duration) -> f64 {
    (d.as_nanos() as f64) / 1_000_000.0
}

// ---------------------------------------------------------------------------
// spawn
// ---------------------------------------------------------------------------

fn parse_name(opts: Option<&LuaTable>) -> LuaResult<Option<String>> {
    match opts {
        Some(t) => match t.get::<Option<String>>("name")? {
            Some(s) => Ok(Some(s)),
            None => Ok(None),
        },
        None => Ok(None),
    }
}

fn spawn(lua: &Lua, (func, opts): (Function, Option<LuaTable>)) -> LuaResult<Handle> {
    let name = parse_name(opts.as_ref())?;

    let (tx, rx) = oneshot::channel::<LuaResult<Value>>();

    // Drive the Lua function via its async adapter (feature=async gives us
    // Pending ↔ coroutine.yield bridging automatically).
    let fut = async move {
        let result: LuaResult<Value> = func.call_async::<Value>(MultiValue::new()).await;
        let _ = tx.send(result);
    };

    let join_handle = tokio::task::spawn_local(async move {
        // Catch panics so an errant Lua script does not tear down the
        // LocalSet-driving thread.
        let wrapped = panic::AssertUnwindSafe(fut);
        use std::future::Future;
        use std::pin::Pin;
        use std::task::{Context, Poll};
        // futures::FutureExt::catch_unwind requires futures crate; avoid it
        // with a tiny manual adapter.
        struct Catch<F>(Pin<Box<F>>);
        impl<F: Future<Output = ()>> Future for Catch<F> {
            type Output = Result<(), Box<dyn std::any::Any + Send>>;
            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.0.as_mut().poll(cx)
                })) {
                    Ok(Poll::Ready(())) => Poll::Ready(Ok(())),
                    Ok(Poll::Pending) => Poll::Pending,
                    Err(p) => Poll::Ready(Err(p)),
                }
            }
        }
        let _ = Catch(Box::pin(wrapped.0)).await;
    });

    let abort = join_handle.abort_handle();
    with_root_scope(lua, |s| s.attach(abort.clone()))?;

    let id = join_handle_id(&join_handle);

    Ok(Handle {
        id,
        name,
        abort,
        state: JoinState::Pending(rx),
        started_at: Instant::now(),
    })
}

fn join_handle_id<T>(h: &tokio::task::JoinHandle<T>) -> String {
    h.id().to_string()
}

// ---------------------------------------------------------------------------
// sleep / yield
// ---------------------------------------------------------------------------

async fn sleep(_: Lua, ms: f64) -> LuaResult<()> {
    if ms.is_nan() || ms < 0.0 {
        return Err(LuaError::external(format!(
            "std.task.sleep: invalid duration (ms={ms})"
        )));
    }
    let dur = Duration::from_nanos((ms * 1_000_000.0) as u64);
    tokio::time::sleep(dur).await;
    Ok(())
}

async fn yield_now(_: Lua, _: ()) -> LuaResult<()> {
    tokio::task::yield_now().await;
    Ok(())
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(lua: &Lua) -> LuaResult<()> {
    // Install the root scope into Lua app data so Handles can register
    // their AbortHandles and VM drop triggers cleanup.
    let root: Rc<RefCell<RootScope>> = Rc::new(RefCell::new(RootScope::default()));
    lua.set_app_data(root);

    let task = lua.create_table()?;
    task.set("spawn", lua.create_function(spawn)?)?;
    task.set("sleep", lua.create_async_function(sleep)?)?;
    task.set("yield", lua.create_async_function(yield_now)?)?;

    let std_ns: LuaTable = lua.globals().get("std")?;
    std_ns.set("task", task)?;
    Ok(())
}
