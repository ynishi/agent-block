mod common;

use predicates::prelude::*;
use tempfile::tempdir;

/// The async overrides are installed by the real host wiring.
///
/// `host.rs` calls `mlua_batteries::async_overrides::register_by_name` right
/// after `register_all`, which is what keeps a script's `std.time.sleep` /
/// `std.http.*` / `std.fs.*` / `std.proc.pipeline` from parking the VM thread
/// and every sibling coroutine with it. Nothing about the Lua API changes when
/// that call is there, so the only way to see it is behavioural: the fixture
/// sleeps while a sibling task ticks, and reports how many ticks landed.
///
/// Without the overrides `std.time.sleep` is `std::thread::sleep` on the VM
/// thread, the sibling gets no turn, and `ticks_during` is 0.
#[test]
fn the_host_installs_the_async_overrides() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("async_overrides.lua")])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("[OVR] overrides_active=true")
                .and(predicate::str::contains("[OVR] done")),
        );
}
