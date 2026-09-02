# task-mcp justfile (rust)

# [allow-agent]
# Everything a commit has to pass: format, lint, Rust tests, Lua specs.
# Run this — not a hand-assembled cargo line — so the gate is the same every
# time and the Lua side is never the part that gets skipped.
check: lint test test-lua

# [allow-agent]
# Build only
build:
    cargo build --workspace

# [allow-agent]
# Rust tests, one crate at a time.
#
# Deliberately not `--workspace`: that links every test binary at once, one mold
# per binary at roughly 2.5GB each, and this is a shared machine. Doing it in
# parallel is what exhausted memory and stalled the box for every user on
# 2026-08-15. Sequential per-crate keeps one linker resident at a time.
test:
    cargo test -p agent-block-types
    cargo test -p agent-block-mcp
    cargo test -p agent-block-core
    cargo test -p agent-block
    cargo test -p agent-block-testkit

# [allow-agent]
# Run the Lua spec fixtures (mlua-lspec) in crates/agent-block/tests/fixtures/.
# `cargo test` cannot host these: mlua-lspec needs mlua's `send` feature, which
# mlua-batteries does not compile under, so the runner is its own crate outside
# the workspace. Rationale: crates/lua-spec-runner/src/main.rs.
# Optional argument filters fixtures by filename substring.
test-lua filter="":
    cargo run --quiet --manifest-path crates/lua-spec-runner/Cargo.toml -- {{ filter }}

# [allow-agent]
# Format and lint
lint:
    cargo fmt --all
    cargo clippy --workspace --no-deps -- -D warnings

# [allow-agent]
# Run structured LLM meta-log demo example.
# Requires ANTHROPIC_API_KEY.
demo-llm-meta:
    AGENT_BLOCK_LLM_DUMP=meta \
    AGENT_BLOCK_TRACE_ID=${AGENT_BLOCK_TRACE_ID:-maint-trace-001} \
    AGENT_BLOCK_AGENT_ID=${AGENT_BLOCK_AGENT_ID:-maint-agent-01} \
    AGENT_BLOCK_AGENT_NAME=${AGENT_BLOCK_AGENT_NAME:-maintainer} \
    AGENT_BLOCK_RUN_ID=${AGENT_BLOCK_RUN_ID:-maint-run-001} \
    cargo run -p agent-block -- --script crates/agent-block/examples/test_agent_log_meta.lua

# [allow-agent]
# Run ignored E2E for structured meta logs.
# Requires ANTHROPIC_API_KEY.
e2e-llm-meta:
    cargo test -p agent-block --test e2e_agent agent_run_emits_structured_meta_logs -- --ignored
