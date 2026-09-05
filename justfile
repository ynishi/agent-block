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
# LSHAPE_CHECK=1 for the same reason as test-lua, and for a reason only this
# side can serve: the e2e tests drive the blocks through the real bridges, so
# the contracts on values the host produces get checked against what the host
# actually produces rather than against a fixture's stub of it. The variable
# reaches the spawned agent-block process, which is where the assert runs.
test:
    LSHAPE_CHECK=1 cargo test -p agent-block-types
    LSHAPE_CHECK=1 cargo test -p agent-block-mcp
    LSHAPE_CHECK=1 cargo test -p agent-block-core
    LSHAPE_CHECK=1 cargo test -p agent-block
    LSHAPE_CHECK=1 cargo test -p agent-block-testkit

# [allow-agent]
# Run the Lua spec fixtures (mlua-lspec) in crates/agent-block/tests/fixtures/.
# `cargo test` cannot host these: mlua-lspec needs mlua's `send` feature, which
# mlua-batteries does not compile under, so the runner is its own crate outside
# the workspace. Rationale: crates/lua-spec-runner/src/main.rs.
# Optional argument filters fixtures by filename substring.
#
# LSHAPE_CHECK=1 turns on the lshape boundary contracts, which are otherwise
# inert. A contract nothing runs is a comment, so the specs are where they get
# enforced; production stays unchecked and pays nothing.
test-lua filter="":
    LSHAPE_CHECK=1 cargo run --quiet --manifest-path crates/lua-spec-runner/Cargo.toml -- {{ filter }}

# [allow-agent]
# Format and lint
lint:
    cargo fmt --all
    cargo clippy --workspace --no-deps -- -D warnings

# [allow-agent]
# Build the crates.io package of every publishable crate, locally, in publish
# order. Nothing is uploaded. This is the one check that walks the files
# `cargo publish` will archive — the git-tracked tree of each crate directory —
# so it is what catches a tracked path that no build reads: the 0.36.0 publish
# stopped at the binary crate on a dangling `blocks` symlink that every test,
# lint and `cargo package --list` had walked past. Run it before a bump, after
# `check`; sequential for the same reason `test` is.
package-check:
    cargo package -p agent-block-types
    cargo package -p agent-block-mcp
    cargo package -p agent-block-core
    cargo package -p agent-block-testkit
    cargo package -p agent-block

# [allow-agent]
# Run the correlation-id demo: the ab.obs http_request / http_response
# lines carry the four ids below on every model call. Requires ANTHROPIC_API_KEY.
demo-llm-meta:
    RUST_LOG=${RUST_LOG:-info} \
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
