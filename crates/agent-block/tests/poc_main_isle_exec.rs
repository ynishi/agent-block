//! POC: `AsyncIsle::exec` can invoke a Lua closure with upvalues on the main
//! Isle thread.
//!
//! **Goal**: Prove that dispatching from a Rust tokio task into main Isle via
//! `exec(|lua| { … })` correctly calls the Lua closure stored in
//! `__user_cbs["test"]` *and* that the closure's captured upvalue (`counter`)
//! is preserved across multiple calls — i.e. bytecode-dump is not involved and
//! upvalue identity is intact.
//!
//! rmcp / handler_isle are not touched.

use mlua_isle::AsyncIsle;

/// Lua script that registers an upvalue-capturing closure into `__user_cbs`.
///
/// `counter` is a local variable captured by the closure; it is intentionally
/// NOT placed in `_G` before the closure is defined — only the accessor
/// `_G.get_counter` is exported so Rust can read the final value.
const SETUP_SCRIPT: &str = r#"
__user_cbs = {}
local counter = 0
__user_cbs["test"] = function(ev)
    counter = counter + 1
    return counter
end
-- Export a reader so Rust can inspect the upvalue after exec calls.
_G.get_counter = function() return counter end
"#;

/// Run `__user_cbs["test"]` on the main Isle via `exec`, passing an empty
/// table as the event argument.  Returns the value returned by the Lua closure
/// (the current counter value after increment).
///
/// `exec` returns `Result<String, IsleError>`, so the counter is serialised to
/// a decimal string and parsed back by the caller.
async fn call_user_cb(isle: &AsyncIsle) -> i64 {
    let s = isle
        .exec(|lua| {
            let tbl: mlua::Table = lua
                .globals()
                .get("__user_cbs")
                .map_err(|e| mlua_isle::IsleError::Lua(format!("get __user_cbs: {e}")))?;
            let cb: mlua::Function = tbl
                .get("test")
                .map_err(|e| mlua_isle::IsleError::Lua(format!("get __user_cbs[\"test\"]: {e}")))?;
            // Pass an empty table as the `ev` argument — the POC closure ignores it.
            let ev = lua
                .create_table()
                .map_err(|e| mlua_isle::IsleError::Lua(format!("create ev table: {e}")))?;
            let n: i64 = cb.call(ev).map_err(|e| {
                mlua_isle::IsleError::Lua(format!("call __user_cbs[\"test\"]: {e}"))
            })?;
            Ok(n.to_string())
        })
        .await
        .expect("exec should not fail");
    s.parse::<i64>().expect("counter must be a valid integer")
}

/// Read `_G.get_counter()` from the main Isle to verify the upvalue state.
async fn read_counter(isle: &AsyncIsle) -> i64 {
    let s = isle
        .exec(|lua| {
            let get_counter: mlua::Function = lua
                .globals()
                .get("get_counter")
                .map_err(|e| mlua_isle::IsleError::Lua(format!("get get_counter: {e}")))?;
            let n: i64 = get_counter
                .call(())
                .map_err(|e| mlua_isle::IsleError::Lua(format!("call get_counter: {e}")))?;
            Ok(n.to_string())
        })
        .await
        .expect("exec should not fail");
    s.parse::<i64>().expect("counter must be a valid integer")
}

#[tokio::test]
async fn main_isle_exec_preserves_upvalue() {
    // ── Spawn a minimal main Isle (no bridges needed for the POC) ──────
    let (isle, driver) = AsyncIsle::spawn(|_lua: &mlua::Lua| Ok(()))
        .await
        .expect("AsyncIsle::spawn should succeed");

    // ── Load the setup script that registers the upvalue-capturing closure ──
    isle.exec(|lua| {
        lua.load(SETUP_SCRIPT)
            .exec()
            .map_err(|e| mlua_isle::IsleError::Lua(format!("setup script: {e}")))?;
        Ok(String::new())
    })
    .await
    .expect("setup script exec should succeed");

    // ── Call the closure 3 times from the Rust tokio context ──────────
    let r1 = call_user_cb(&isle).await;
    println!("call 1: counter = {r1}");
    assert_eq!(r1, 1, "after 1st call counter should be 1");

    let r2 = call_user_cb(&isle).await;
    println!("call 2: counter = {r2}");
    assert_eq!(r2, 2, "after 2nd call counter should be 2");

    let r3 = call_user_cb(&isle).await;
    println!("call 3: counter = {r3}");
    assert_eq!(r3, 3, "after 3rd call counter should be 3");

    // ── Verify via the exported reader that the upvalue is truly shared ─
    let final_counter = read_counter(&isle).await;
    println!("get_counter() = {final_counter}");
    assert_eq!(
        final_counter, 3,
        "get_counter() must return 3 — upvalue is shared between closure and reader"
    );

    // ── Shutdown ──────────────────────────────────────────────────────
    driver
        .shutdown()
        .await
        .expect("driver shutdown should succeed");
}
