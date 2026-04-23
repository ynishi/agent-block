# task-mcp justfile (rust)

# [allow-agent]
# Pre-commit quality check (fmt → clippy → test)
check:
    cargo fmt
    cargo clippy -- -D warnings
    cargo test

# [allow-agent]
# Build only
build:
    cargo build

# [allow-agent]
# Run tests only
test:
    cargo test

# [allow-agent]
# Format and lint
lint:
    cargo fmt
    cargo clippy -- -D warnings

# [allow-agent]
# Run structured LLM meta-log demo example.
# Requires ANTHROPIC_API_KEY.
demo-llm-meta:
    AGENT_BLOCK_LLM_DUMP=meta \
    AGENT_BLOCK_TRACE_ID=${AGENT_BLOCK_TRACE_ID:-maint-trace-001} \
    AGENT_BLOCK_AGENT_ID=${AGENT_BLOCK_AGENT_ID:-maint-agent-01} \
    AGENT_BLOCK_AGENT_NAME=${AGENT_BLOCK_AGENT_NAME:-maintainer} \
    AGENT_BLOCK_RUN_ID=${AGENT_BLOCK_RUN_ID:-maint-run-001} \
    cargo run -- --script examples/test_agent_log_meta.lua

# [allow-agent]
# Run ignored E2E for structured meta logs.
# Requires ANTHROPIC_API_KEY.
e2e-llm-meta:
    cargo test --test e2e_agent agent_run_emits_structured_meta_logs -- --ignored
