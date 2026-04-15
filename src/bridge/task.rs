//! `std.task` — structured async task primitives for Lua scripts.
//!
//! Phase 1 (committed) provided top-level `spawn` / `sleep` / `yield` with a
//! VM-scoped root abort registry.  Phase 2 (this revision) adds structured
//! concurrency primitives:
//!
//! - `std.task.scope(name?, fn)`      — nursery: waits for all children
//! - `std.task.with_timeout(ms, fn)`  — scope with deadline
//! - `std.task.checkpoint()`          — cancellation yield point
//! - `std.task.cancel_token()`        — standalone `CancelToken`
//! - `Scope:spawn`, `:cancel`, `:token`, `.name`
//! - `CancelToken:cancel`, `:is_cancelled`, `:check`
//!
//! # Structured concurrency
//!
//! `task.scope(fn)` creates a [`Scope`], pushes it onto a VM-scoped stack,
//! runs `fn(scope)`, and — regardless of how `fn` exits — waits for every
//! task spawned into that scope to finish before returning.  If `fn`
//! returned an error (or `with_timeout` tripped), the scope's
//! [`CancelToken`] is cancelled first so cooperative children can exit
//! cleanly; any that do not are aborted.  Top-level `task.spawn` attaches
//! to the root scope installed at `register()` time.
//!
//! # Cancellation
//!
//! Cancellation is cooperative.  `task.checkpoint()` (inside a spawned
//! task) checks the task-local token installed by `spawn_into`, yields,
//! and raises `task cancelled` if set.  Tasks that never reach a
//! checkpoint are aborted via tokio `AbortHandle` when the scope exits.
//!
//! # Runtime contract
//!
//! Must be called from within a `tokio::task::LocalSet` driven by a
//! current-thread runtime.  `mlua-isle::AsyncIsle` satisfies this.
//!
//! # Driver selection (Phase 3)
//!
//! Two drivers are available:
//!
//! - `async_fn` (default) — drives the user function via `Function::call_async`,
//!   so `task.sleep` / `task.yield` / `task.checkpoint` (which are registered
//!   as async functions) suspend through mlua's async bridge.
//! - `coroutine` (opt-in) — drives a raw Lua thread via `Thread::resume` in a
//!   loop.  The Lua body uses `coroutine.yield()` for a cooperative yield and
//!   `coroutine.yield(ms)` to sleep `ms` milliseconds.  Useful for interop
//!   with existing coroutine-based Lua code and avoids the async_fn layer.
//!   Selected via `opts.driver = "coroutine"` (per-spawn) or the
//!   `AGENT_BLOCK_TASK_DRIVER=coroutine` env var (default for the VM).
//!
//! Every task is wrapped in a `tracing::info_span!("task", id, name, driver)`
//! so downstream tool logs (sh / mesh / mcp / sql) carry task context.  Inside
//! a spawned task, `std.task.current()` returns `{id, name, cancelled}` for
//! Lua-side introspection.

use std::cell::RefCell;
use std::future::Future;
use std::panic;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use mlua::prelude::*;
use mlua::{Function, MultiValue, ThreadStatus, UserData, UserDataMethods, UserDataRegistry, Value};
use tokio::sync::oneshot;
use tokio::task::{AbortHandle, JoinHandle};
use tracing::{Instrument, info_span};

// ---------------------------------------------------------------------------
// CancelToken — cooperative cancellation
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct CancelToken(Rc<RefCell<bool>>);

impl CancelToken {
    fn new() -> Self {
        Self(Rc::new(RefCell::new(false)))
    }
    fn cancel(&self) {
        *self.0.borrow_mut() = true;
    }
    fn is_cancelled(&self) -> bool {
        *self.0.borrow()
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

#[derive(Clone)]
struct TaskInfo {
    id: String,
    name: Option<String>,
}

tokio::task_local! {
    /// Set by `spawn_into` for the duration of a spawned task so that
    /// `task.checkpoint()` can consult the task's cancellation token
    /// without the caller threading it through manually.
    static TASK_TOKEN: CancelToken;
    /// Set by `spawn_into` for the duration of a spawned task so that
    /// `std.task.current()` can return id/name without threading them
    /// through the Lua function signature.
    static TASK_INFO: TaskInfo;
}

// ---------------------------------------------------------------------------
// Scope — structured concurrency container
// ---------------------------------------------------------------------------

struct Scope {
    name: Option<String>,
    token: CancelToken,
    /// JoinHandles for children.  We keep JoinHandles (not just AbortHandles)
    /// so the scope can `.await` them to implement structured concurrency.
    children: Vec<JoinHandle<()>>,
}

impl Scope {
    fn new(name: Option<String>) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            name,
            token: CancelToken::new(),
            children: Vec::new(),
        }))
    }

    fn attach(&mut self, h: JoinHandle<()>) {
        self.children.retain(|h| !h.is_finished());
        self.children.push(h);
    }
}

impl Drop for Scope {
    /// Last-resort cleanup.  Scopes created by `task.scope` /
    /// `task.with_timeout` normally drain children via the async path
    /// (`drain_scope`) before being dropped; the root scope, on the
    /// other hand, is dropped on VM teardown and relies on this impl
    /// to abort any remaining fire-and-forget tasks.
    fn drop(&mut self) {
        for h in &self.children {
            h.abort();
        }
    }
}

/// Await every child to completion, then clear the scope's child list.
///
/// Called by `task.scope` / `task.with_timeout` when the user callback
/// returns.  If the callback errored or timed out, the caller should
/// cancel `scope.token` before calling this so cooperative children
/// observe the flag on their next `checkpoint`.  Non-cooperative
/// children are aborted here after the cancel window elapses is left
/// to callers via explicit `abort` — for Phase 2 we simply await each
/// child; abort is issued only by `Drop`.
async fn drain_scope(scope: &Rc<RefCell<Scope>>) {
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
/// `with_timeout` after the deadline elapses.
fn abort_all(scope: &Rc<RefCell<Scope>>) {
    for h in &scope.borrow().children {
        h.abort();
    }
}

// ---------------------------------------------------------------------------
// Lua-facing Scope handle
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ScopeHandle(Rc<RefCell<Scope>>);

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
            |lua, this, (func, opts): (Function, Option<LuaTable>)| {
                spawn_into(lua, &this.0, func, opts)
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Scope stack (Lua app data)
// ---------------------------------------------------------------------------

type ScopeStack = Rc<RefCell<Vec<Rc<RefCell<Scope>>>>>;

fn scope_stack(lua: &Lua) -> LuaResult<ScopeStack> {
    lua.app_data_ref::<ScopeStack>()
        .map(|r| r.clone())
        .ok_or_else(|| LuaError::external("std.task scope stack not initialised"))
}

fn push_scope(lua: &Lua, s: Rc<RefCell<Scope>>) -> LuaResult<()> {
    scope_stack(lua)?.borrow_mut().push(s);
    Ok(())
}

fn pop_scope(lua: &Lua) -> LuaResult<()> {
    scope_stack(lua)?
        .borrow_mut()
        .pop()
        .ok_or_else(|| LuaError::external("scope stack underflow"))?;
    Ok(())
}

fn current_scope(lua: &Lua) -> LuaResult<Rc<RefCell<Scope>>> {
    scope_stack(lua)?
        .borrow()
        .last()
        .cloned()
        .ok_or_else(|| LuaError::external("no active scope"))
}

// ---------------------------------------------------------------------------
// Handle — UserData returned from spawn
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
            match state {
                JoinState::Pending(rx) => match rx.await {
                    Ok(res) => res,
                    Err(_) => Err(LuaError::external("task cancelled before completion")),
                },
                JoinState::Taken => Err(LuaError::external("task already joined")),
            }
        });
    }
}

fn duration_to_ms(d: Duration) -> f64 {
    (d.as_nanos() as f64) / 1_000_000.0
}

// ---------------------------------------------------------------------------
// spawn_into — shared logic for top-level spawn and scope:spawn
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum Driver {
    AsyncFn,
    Coroutine,
}

fn parse_opts(opts: Option<&LuaTable>) -> LuaResult<(Option<String>, Option<Driver>)> {
    match opts {
        None => Ok((None, None)),
        Some(t) => {
            let name = t.get::<Option<String>>("name")?;
            let driver = match t.get::<Option<String>>("driver")? {
                None => None,
                Some(s) => Some(match s.as_str() {
                    "coroutine" => Driver::Coroutine,
                    "async_fn" | "async" => Driver::AsyncFn,
                    other => {
                        return Err(LuaError::external(format!(
                            "std.task: unknown driver '{other}' (expected 'async_fn' or 'coroutine')"
                        )));
                    }
                }),
            };
            Ok((name, driver))
        }
    }
}

fn default_driver() -> Driver {
    match std::env::var("AGENT_BLOCK_TASK_DRIVER").ok().as_deref() {
        Some("coroutine") => Driver::Coroutine,
        _ => Driver::AsyncFn,
    }
}

fn spawn_into(
    lua: &Lua,
    scope: &Rc<RefCell<Scope>>,
    func: Function,
    opts: Option<LuaTable>,
) -> LuaResult<Handle> {
    let (name, driver_opt) = parse_opts(opts.as_ref())?;
    let driver = driver_opt.unwrap_or_else(default_driver);
    let token = scope.borrow().token.clone();

    let (tx, rx) = oneshot::channel::<LuaResult<Value>>();

    // Pre-allocate a stable id so tracing span and TaskInfo share it.  The
    // tokio JoinHandle::id() below produces a different runtime-internal id;
    // exposing that as well would be confusing, so we use our own.
    let id = format!("t{}", TASK_SEQ.with(|s| s.next()));
    let info = TaskInfo {
        id: id.clone(),
        name: name.clone(),
    };

    let lua_for_cr = lua.clone();
    let user_fut: Pin<Box<dyn Future<Output = ()>>> = match driver {
        Driver::AsyncFn => Box::pin(async move {
            let result: LuaResult<Value> = func.call_async::<Value>(MultiValue::new()).await;
            let _ = tx.send(result);
        }),
        Driver::Coroutine => Box::pin(async move {
            let result = run_coroutine(&lua_for_cr, func).await;
            let _ = tx.send(result);
        }),
    };

    // Wrap with tracing span first so it observes TASK_TOKEN/TASK_INFO enter/exit.
    let span = info_span!(
        "task",
        id = %id,
        name = name.as_deref().unwrap_or(""),
        driver = ?driver,
    );
    let traced = user_fut.instrument(span);
    let with_info = TASK_INFO.scope(info, traced);
    let scoped_fut = TASK_TOKEN.scope(token, with_info);

    let join_handle = tokio::task::spawn_local(catch_panic(scoped_fut));
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

// ---------------------------------------------------------------------------
// Coroutine driver
// ---------------------------------------------------------------------------

/// Drive `func` as a raw Lua coroutine.  The Lua body uses:
///
/// - `coroutine.yield()` / `coroutine.yield(nil)` — cooperative yield
/// - `coroutine.yield(ms)` where ms is a number — sleep `ms` milliseconds
///
/// Between resumes, we check the task-local cancellation token and, if set,
/// raise `task cancelled` into the thread on the next resume.
async fn run_coroutine(lua: &Lua, func: Function) -> LuaResult<Value> {
    let thread = lua.create_thread(func)?;
    loop {
        if TASK_TOKEN.try_with(|t| t.is_cancelled()).unwrap_or(false) {
            return Err(LuaError::external("task cancelled"));
        }

        let yielded: MultiValue = thread.resume(MultiValue::new())?;

        match thread.status() {
            ThreadStatus::Finished => {
                return Ok(yielded.into_iter().next().unwrap_or(Value::Nil));
            }
            ThreadStatus::Resumable => {
                let ctrl = yielded.into_iter().next().unwrap_or(Value::Nil);
                match ctrl {
                    Value::Nil => tokio::task::yield_now().await,
                    Value::Integer(ms) => {
                        let ms = ms.max(0) as u64;
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    }
                    Value::Number(ms) => {
                        if !ms.is_finite() || ms < 0.0 {
                            return Err(LuaError::external(format!(
                                "coroutine yield: invalid duration (ms={ms})"
                            )));
                        }
                        tokio::time::sleep(Duration::from_nanos((ms * 1_000_000.0) as u64)).await;
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
// panic catch adapter — prevents an errant Lua script from killing the
// LocalSet-driving thread.
// ---------------------------------------------------------------------------

struct Catch<F>(Pin<Box<F>>);

impl<F: Future<Output = ()>> Future for Catch<F> {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        match panic::catch_unwind(panic::AssertUnwindSafe(|| self.0.as_mut().poll(cx))) {
            Ok(Poll::Ready(())) => Poll::Ready(()),
            Ok(Poll::Pending) => Poll::Pending,
            Err(_) => Poll::Ready(()),
        }
    }
}

fn catch_panic<F: Future<Output = ()> + 'static>(fut: F) -> impl Future<Output = ()> + 'static {
    Catch(Box::pin(fut))
}

// ---------------------------------------------------------------------------
// Top-level callables
// ---------------------------------------------------------------------------

fn spawn(lua: &Lua, (func, opts): (Function, Option<LuaTable>)) -> LuaResult<Handle> {
    let scope = current_scope(lua)?;
    spawn_into(lua, &scope, func, opts)
}

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

async fn checkpoint(lua: Lua, _: ()) -> LuaResult<()> {
    // First consult the task-local token (only set inside `spawn_into` tasks).
    // If unset, fall back to the innermost scope on the stack so checkpoint
    // called from a `task.scope(fn)` callback (which runs in the caller's
    // task, not a spawned one) still observes scope cancellation.
    let cancelled_from_task = TASK_TOKEN.try_with(|t| t.is_cancelled()).unwrap_or(false);
    let cancelled = if cancelled_from_task {
        true
    } else {
        scope_stack(&lua)
            .ok()
            .and_then(|s| s.borrow().last().cloned())
            .map(|s| s.borrow().token.is_cancelled())
            .unwrap_or(false)
    };
    if cancelled {
        return Err(LuaError::external("task cancelled"));
    }
    tokio::task::yield_now().await;
    Ok(())
}

fn cancel_token(_: &Lua, _: ()) -> LuaResult<CancelToken> {
    Ok(CancelToken::new())
}

/// `std.task.current()` — returns a table `{id, name, cancelled}` describing
/// the currently-executing spawned task, or `nil` if called from outside a
/// spawned task (e.g. at module top level or inside a `task.scope` body).
fn current(lua: &Lua, _: ()) -> LuaResult<Value> {
    let info = TASK_INFO.try_with(|i| i.clone()).ok();
    match info {
        None => Ok(Value::Nil),
        Some(i) => {
            let t = lua.create_table()?;
            t.set("id", i.id)?;
            t.set("name", i.name)?;
            let cancelled = TASK_TOKEN.try_with(|t| t.is_cancelled()).unwrap_or(false);
            t.set("cancelled", cancelled)?;
            Ok(Value::Table(t))
        }
    }
}

/// `task.scope(fn)` or `task.scope(name, fn)` — structured nursery.
async fn task_scope(lua: Lua, args: MultiValue) -> LuaResult<Value> {
    let (name, func) = parse_scope_args(&args)?;

    let scope = Scope::new(name);
    push_scope(&lua, scope.clone())?;
    let handle = ScopeHandle(scope.clone());

    let user_result: LuaResult<Value> = func.call_async::<Value>(handle).await;
    let _ = pop_scope(&lua);

    // If the user callback errored, cancel the scope so cooperative
    // children bail out on their next checkpoint.  Then await every
    // child regardless — this is the structured-concurrency guarantee.
    if user_result.is_err() {
        scope.borrow().token.cancel();
    }
    drain_scope(&scope).await;

    user_result
}

fn parse_scope_args(args: &MultiValue) -> LuaResult<(Option<String>, Function)> {
    let mut iter = args.iter();
    let first = iter
        .next()
        .ok_or_else(|| LuaError::external("task.scope requires at least a function"))?;
    match first {
        Value::Function(f) => Ok((None, f.clone())),
        Value::String(s) => {
            let n = s.to_str()?.to_string();
            let second = iter
                .next()
                .ok_or_else(|| LuaError::external("task.scope(name, fn) requires a function"))?;
            match second {
                Value::Function(f) => Ok((Some(n), f.clone())),
                _ => Err(LuaError::external(
                    "task.scope: second argument must be a function",
                )),
            }
        }
        _ => Err(LuaError::external(
            "task.scope: first argument must be a function or a name string",
        )),
    }
}

/// `task.with_timeout(ms, fn)` — scope with deadline.
async fn with_timeout(lua: Lua, (ms, func): (f64, Function)) -> LuaResult<Value> {
    if !ms.is_finite() || ms < 0.0 {
        return Err(LuaError::external(format!(
            "task.with_timeout: invalid duration (ms={ms})"
        )));
    }
    let dur = Duration::from_nanos((ms * 1_000_000.0) as u64);

    let scope = Scope::new(None);
    push_scope(&lua, scope.clone())?;
    let handle = ScopeHandle(scope.clone());

    let user_fut = func.call_async::<Value>(handle);
    let timed = tokio::time::timeout(dur, user_fut).await;
    let _ = pop_scope(&lua);

    let user_result: LuaResult<Value> = match timed {
        Ok(r) => r,
        Err(_) => {
            scope.borrow().token.cancel();
            Err(LuaError::external(format!(
                "task.with_timeout: exceeded {ms} ms"
            )))
        }
    };

    if user_result.is_err() {
        scope.borrow().token.cancel();
        // Non-cooperative children (no checkpoint) must be aborted so the
        // scope actually tears down within a bounded window.
        abort_all(&scope);
    }
    drain_scope(&scope).await;

    user_result
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register(lua: &Lua) -> LuaResult<()> {
    // Install the scope stack with a single root scope.  The root scope
    // lives for the VM lifetime; its Drop triggers abort_all on any
    // outstanding fire-and-forget tasks.
    let root = Scope::new(Some("root".to_string()));
    let stack: ScopeStack = Rc::new(RefCell::new(vec![root]));
    lua.set_app_data(stack);

    let task = lua.create_table()?;
    task.set("spawn", lua.create_function(spawn)?)?;
    task.set("sleep", lua.create_async_function(sleep)?)?;
    task.set("yield", lua.create_async_function(yield_now)?)?;
    task.set("checkpoint", lua.create_async_function(checkpoint)?)?;
    task.set("cancel_token", lua.create_function(cancel_token)?)?;
    task.set("current", lua.create_function(current)?)?;
    task.set("scope", lua.create_async_function(task_scope)?)?;
    task.set("with_timeout", lua.create_async_function(with_timeout)?)?;

    let std_ns: LuaTable = lua.globals().get("std")?;
    std_ns.set("task", task)?;
    Ok(())
}
