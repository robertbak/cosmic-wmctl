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

# --- Install ---

prefix := env_var_or_default("PREFIX", env_var("HOME") + "/.local")

## Install binaries, desktop entry and systemd unit to ~/.local (override: prefix=/usr)
install: release
    install -Dm755 target/release/cosmic-wmctl {{prefix}}/bin/cosmic-wmctl
    install -Dm755 target/release/cosmic-wmctl-config {{prefix}}/bin/cosmic-wmctl-config
    install -Dm644 cosmic-wmctl-config/cosmic-wmctl-config.desktop {{prefix}}/share/applications/cosmic-wmctl-config.desktop
    install -Dm644 dist/cosmic-wmctl.service {{prefix}}/share/systemd/user/cosmic-wmctl.service
    update-desktop-database {{prefix}}/share/applications 2>/dev/null || true
    @echo "Installed. Enable the daemon with:"
    @echo "  systemctl --user enable --now cosmic-wmctl"

## Uninstall binaries, desktop entry and systemd unit
uninstall:
    rm -f {{prefix}}/bin/cosmic-wmctl {{prefix}}/bin/cosmic-wmctl-config
    rm -f {{prefix}}/share/applications/cosmic-wmctl-config.desktop
    rm -f {{prefix}}/share/systemd/user/cosmic-wmctl.service
    update-desktop-database {{prefix}}/share/applications 2>/dev/null || true

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
