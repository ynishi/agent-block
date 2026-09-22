# task-mcp justfile (rust)

# [allow-agent]
# Everything a commit has to pass: format, lint, Rust tests, Lua specs.
# Run this — not a hand-assembled cargo line — so the gate is the same every
# time and the Lua side is never the part that gets skipped.
check: lint test test-lua check-tl

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
# Builds the agent-block binary first: an embedded module written in Teal has
# no `.lua` in the tree, and the runner takes the Lua the binary embeds for it
# (`agent-block vendor`, see the runner's `vendor_teal_modules`).
# `cargo test` cannot host these: mlua-lspec needs mlua's `send` feature, which
# mlua-batteries does not compile under, so the runner is its own crate outside
# the workspace. Rationale: crates/lua-spec-runner/src/main.rs.
# Optional argument filters fixtures by filename substring.
#
# LSHAPE_CHECK=1 turns on the lshape boundary contracts, which are otherwise
# inert. A contract nothing runs is a comment, so the specs are where they get
# enforced; production stays unchecked and pays nothing.
test-lua filter="":
    cargo build --quiet -p agent-block
    LSHAPE_CHECK=1 cargo run --quiet --manifest-path crates/lua-spec-runner/Cargo.toml -- {{ filter }}

# [allow-agent]
# Run a project's vendored specs against its vendored modules: what
# `agent-block vendor <name>` wrote under <dir>/.agent-block/lib/<name>/spec/,
# with <dir>/.agent-block/lib/ first on the require path. The other half of
# vendoring — a copy the project can edit is a copy that needs checking.
test-lua-project dir filter="":
    cargo build --quiet -p agent-block
    LSHAPE_CHECK=1 cargo run --quiet --manifest-path crates/lua-spec-runner/Cargo.toml -- --project {{ dir }} {{ filter }}

# [allow-agent]
# Type-check the Teal side of the embedded modules: every `.tl` under
# `blocks/lib`, strict (a warning is an error), with `crates/agent-block-core/
# htl.toml` saying where the checker resolves requires from. `htl` is the CLI
# (`cargo install htl-cli`; mise pins it), on the same terms as stylua. Zero
# `.tl` files is a pass — the gate is in `check` from before the first module
# moves, so the day one does, nothing has to be wired.
check-tl:
    htl check --strict crates/agent-block-core/blocks/lib

# [allow-agent]
# Run the Teal specs: `*_test.tl` under `blocks/lib`, one state per file.
# Not in `check` yet — `htl test` exits 1 when it finds no test file, and
# there is none until the first module moves; it joins `check` with that one.
test-tl filter="":
    htl test crates/agent-block-core/blocks/lib {{ if filter == "" { "" } else { "--filter " + filter } }}

# [allow-agent]
# Format and lint. Lua goes through stylua (`cargo install stylua`; config in
# .stylua.toml, exclusions in .styluaignore) and Teal through `htl fmt` (the
# width is in crates/agent-block-core/htl.toml) on the same terms as `cargo fmt`:
# the tree is formatted in place, and `check` runs this first so a commit made
# after `just check` is one stylua would not touch.
lint:
    cargo fmt --all
    stylua .
    htl fmt crates/agent-block-core/blocks/lib
    cargo clippy --workspace --no-deps -- -D warnings

# [allow-agent]
# Build the crates.io package of every publishable crate, locally, in publish
# order. Nothing is uploaded. This is the one check that walks the files
# `cargo publish` will archive — the git-tracked tree of each crate directory —
# so it is what catches a tracked path that no build reads: the 0.36.0 publish
# stopped at the binary crate on a dangling `blocks` symlink that every test,
# lint and `cargo package --list` had walked past. Run it before a bump, after
# `check`.
#
# One invocation, not one per crate: cargo resolves the crates named together
# against their local copies, so a crate that uses a sibling's API works before
# that sibling is on crates.io. One `-p` at a time resolves the sibling from the
# registry instead — the 0.37.0 bump failed both ways round (the published
# 0.36.0 core lacked the new symbol before the bump; `^0.37.0` did not exist on
# the registry after it). Verification still runs one crate at a time, in
# dependency order.
package-check:
    cargo package -p agent-block-types -p agent-block-mcp -p agent-block-core -p agent-block-testkit -p agent-block

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
