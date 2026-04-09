use assert_cmd::Command;

/// Build and return a Command pointing at the `agent-block` binary.
pub fn agent_block_cmd() -> Command {
    Command::cargo_bin("agent-block").expect("agent-block binary should exist")
}

/// Path to a test fixture Lua script.
pub fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}
