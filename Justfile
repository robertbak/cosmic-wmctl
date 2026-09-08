# Build, test, and run commands for cosmic-wmctl
# See: https://just.systems

default: build

# --- Build ---

## Build the project
build:
    cargo build

## Build in release mode
release:
    cargo build --release

## Clean build artifacts
clean:
    cargo clean

# --- Test ---

## Run all tests
test:
    cargo test

## Run tests with output
test-verbose:
    cargo test -- --nocapture

# --- Lint ---

## Check for warnings
check:
    cargo check --all-targets --all-features

## Run clippy
clippy:
    cargo clippy -- -D warnings

# --- Run ---

## Run the CLI
run-cmd:
    cargo run

## Run with a subcommand
run-sub cmd:
    cargo run -- {{cmd}}

## Run windows command with JSON output
run-windows-json:
    COSMIC_WMCTL_OUTPUT=json cargo run -- windows

## Run workspaces command with JSON output
run-workspaces-json:
    COSMIC_WMCTL_OUTPUT=json cargo run -- workspaces

# --- Config GUI ---

## Build the cosmic-wmctl-config GUI
build-config:
    cargo build -p cosmic-wmctl-config

## Run the cosmic-wmctl-config GUI (release build)
run-config-release:
    cargo run --release -p cosmic-wmctl-config

## Run the cosmic-wmctl-config GUI
run-config:
    cargo run -p cosmic-wmctl-config

# --- Help ---

## Show this help
help:
    just --list --verbose
