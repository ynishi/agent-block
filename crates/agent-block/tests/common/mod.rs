pub mod openai_mock;

use assert_cmd::Command;

/// Build and return a Command pointing at the `agent-block` binary.
///
/// The project root is not set here. A test that needs one of its own passes
/// `-p` itself; the rest run with the binary's default (the current
/// directory, which under `cargo test` is this crate), and the binary reads
/// `AGENT_BLOCK_PROJECT` for a caller who wants every fixture resolved
/// against another project's `.agent-block/lib/` — the vendored copies a
/// project has edited, checked by the same fixtures that check the embedded
/// modules. An explicit `-p` still wins over the variable.
#[allow(dead_code)] // used by some integration tests, not all
pub fn agent_block_cmd() -> Command {
    Command::cargo_bin("agent-block").expect("agent-block binary should exist")
}

/// Path to a test fixture Lua script.
#[allow(dead_code)] // used by some integration tests, not all
pub fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Build a `std::process::Command` pointing at the `agent-block` binary.
///
/// `assert_cmd::Command` does not expose the spawned child's PID, which we
/// need to deliver a POSIX signal in `tests/e2e_bus.rs`. This helper returns
/// a plain `std::process::Command` aimed at the same cargo-built binary.
#[allow(dead_code)] // only used by e2e_bus on unix
pub fn agent_block_std_cmd() -> std::process::Command {
    let path = assert_cmd::cargo::cargo_bin("agent-block");
    std::process::Command::new(path)
}

/// Absolute path to an `examples/` script in the workspace.
#[allow(dead_code)] // only used by e2e_bus
pub fn example(name: &str) -> String {
    format!("{}/examples/{name}", env!("CARGO_MANIFEST_DIR"))
}
