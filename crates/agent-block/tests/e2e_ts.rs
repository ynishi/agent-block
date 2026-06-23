mod common;

use predicates::prelude::*;
use tempfile::tempdir;

/// E2E test for `std.ts.*` — exercises the full Lua surface (append / query /
/// last) against a real SQLite file on disk via `AGENT_BLOCK_HOME`.
///
/// Assertions map directly to `tests/fixtures/ts_roundtrip.lua` print output:
///
/// - **C1 dual-type** (`raw_count`, `num_value`, `tbl_value_x`): number 42
///   and table `{x=1,y="ok"}` are appended and retrieved losslessly.
/// - **C2 tag AND** (`and_filter_count`): three rows with overlapping tags;
///   a two-key AND filter returns exactly 2.
/// - **C3 agg × bucket** (`agg_count`, `agg_sum`, `agg_last`, `bucket_count`):
///   single-aggregate and time-bucketed modes produce correct results.
/// - **limit / offset** (`limited_count`, `offset_count`, `offset_first_value`):
///   pagination over a 5-row series.
#[test]
fn ts_roundtrip() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("ts_roundtrip.lua")])
        .assert()
        .success()
        .stdout(
            predicate::str::contains("raw_count=2")
                .and(predicate::str::contains("num_value=42"))
                .and(predicate::str::contains("tbl_value_x=1"))
                .and(predicate::str::contains("and_filter_count=2"))
                .and(predicate::str::contains("agg_count=3"))
                .and(predicate::str::contains("agg_sum=6"))
                .and(predicate::str::contains("agg_last=3"))
                .and(predicate::str::contains("bucket_count=3"))
                .and(predicate::str::contains("limited_count=2"))
                .and(predicate::str::contains("offset_count=2"))
                .and(predicate::str::contains("offset_first_value=2")),
        );
}

/// Smoke test for the `:memory:` SQLite path.
///
/// Uses `AGENT_BLOCK_TS_PATH=:memory:` so no disk file is created.
/// Verifies that append + last round-trip through an in-memory database.
#[test]
fn ts_memory() {
    common::agent_block_cmd()
        .env("AGENT_BLOCK_TS_PATH", ":memory:")
        .args(["-s", &common::fixture("ts_memory.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("mem_last_value=99"));
}
