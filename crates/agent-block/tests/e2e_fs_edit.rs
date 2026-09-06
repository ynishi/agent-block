mod common;

use predicates::prelude::*;
use tempfile::tempdir;

/// The `std.fs` editing primitives, driven through a real Isle.
///
/// The fixture prints one `[FS] <label> = <bool>` line per property and this
/// test asserts that none of them printed `false` — an assertion per label
/// would restate the fixture without checking anything more, while a single
/// `= false` search catches whichever property broke.
#[test]
fn fs_edit_primitives_hold_their_contract() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("fs_edit.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("[FS] done"))
        .stdout(predicate::str::contains("= false").not());
}

/// The `search_replace` tool spec: a snippet named by its text is carried
/// out as the line-addressed edit, and every refusal of that edit still
/// applies. Same one-line-per-property shape as the fixture above.
#[test]
fn fs_search_replace_translates_to_the_edit_contract() {
    let tmp = tempdir().expect("tempdir");
    common::agent_block_cmd()
        .env("AGENT_BLOCK_HOME", tmp.path())
        .args(["-s", &common::fixture("fs_search_replace.lua")])
        .assert()
        .success()
        .stdout(predicate::str::contains("[SR] done"))
        .stdout(predicate::str::contains("= false").not());
}
