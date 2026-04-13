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
