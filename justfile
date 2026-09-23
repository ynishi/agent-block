# task-mcp justfile (rust)

# Every file in the tree a generator owns, written by `gen` and by nothing
# else. One list, because two consumers read it — `gen` regenerates these and
# the pre-commit hook checks that doing so changed nothing — and a second copy
# is a copy that goes stale. Each is also in `.styluaignore`, for the reason
# written there.
generated := "crates/agent-block-core/blocks/lib/knl_types.d.tl crates/agent-block-core/blocks/lib/knl_types.lua"

# [allow-agent]
# Everything a commit has to pass: regenerate, format, lint, Rust tests, Lua
# specs. Run this — not a hand-assembled cargo line — so the gate is the same
# every time and the Lua side is never the part that gets skipped.
#
# The order is load-bearing and is the whole reason `gen` exists as a step.
# `gen` writes the generated files from their generator; `lint` then formats
# the tree; `test` then verifies that what is in the tree is byte-for-byte what
# the generator renders. Run the other way round, the gate can only ever
# REPORT that a generated file is stale — the generator is not part of `check`,
# so nothing in the run can settle it, and the person is left to remember a
# `cargo test` incantation. With `gen` first the run converges from any
# starting state: a stale file, a hand-edited one, or a clean tree all end at
# the same bytes, and a second `just check` changes nothing.
#
# It only converges because the formatters leave these files alone
# (`.styluaignore`); a formatter that rewrote one after `gen` had written it
# would put the tree and the generator back at odds every run, which is the
# state this ordering exists to end. Do not reorder.
check: gen lint test test-lua check-tl test-tl

# [allow-agent]
# Regenerate every file in the tree a generator owns (`generated`, above).
#
# These are rendered from Rust — the kernel's syscall types, as the Teal
# declaration the checker reads and as the lshape module the two runners with
# no host load — by the test that also pins them, which writes instead of
# comparing when `AGENT_BLOCK_WRITE_DTS` is set. Generating and checking from
# one place is deliberate: a separate generator binary would be a second
# renderer to keep in step with the first.
#
# Idempotent, and safe to run on a clean tree: it writes the same bytes the
# tests then assert. Run it after the Rust types move, or just run `check`.
gen:
    AGENT_BLOCK_WRITE_DTS=1 cargo test -p agent-block-core knl_types_declaration_in_the_tree_is_current

# [allow-agent]
# Point this repo's git at the tracked hooks in `.githooks/`.
#
# NOT run by `check` and not installed for you: `core.hooksPath` is a setting
# in your own clone, and a repo that reaches into your git config without being
# asked is a repo that surprises you. Run it once if you want the guarantee.
#
# What it buys: `.githooks/pre-commit` runs `gen` and refuses the commit when
# that changed a generated file, so a commit can never carry a generated file
# that disagrees with its generator. `git commit --no-verify` still goes
# through, which is the escape hatch for a commit that is fixing the generator
# itself.
hooks:
    git config core.hooksPath .githooks
    @echo "core.hooksPath -> .githooks (undo with: git config --unset core.hooksPath)"

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
# Run the Teal specs: `*_test.tl` under `blocks/lib` (beside the module, in
# its `spec/`, where the Lua specs are), one state per file. A test declares
# the host's globals with a value — `std = { … }`, against the declaration in
# `host_types.d.tl` — which is the fake; a module never assigns one.
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
