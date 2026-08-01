# clippo — thin convenience wrapper over cargo.
#
# Build and run from a HOST terminal (cosmic-term), not RustRover's Flatpak:
# the sandboxed Wayland socket filters out the data-control protocol.
# See docs/DESIGN.md, "Environment constraints".

default:
    @just --list

# Debug build of the whole workspace.
build:
    cargo build --workspace

# Release build of the whole workspace.
build-release:
    cargo build --workspace --release

# Run the daemon in the foreground with debug logging.
run-daemon:
    RUST_LOG=${RUST_LOG:-clippod=debug,clippo_wayland=debug} cargo run -p clippod

# Run the CLI: `just run-cli list`
run-cli *ARGS:
    cargo run -p clippo-cli -- {{ARGS}}

# Run the applet.
run-applet:
    cargo run -p clippo-applet

# Debug watcher from M1: print every selection and its flavors.
watch:
    cargo run -p clippo-wayland --bin clippo-watch

test:
    cargo test --workspace

fmt:
    cargo fmt --all

# Everything CI runs, in the same order.
check:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace

clean:
    cargo clean

# Install binaries, the systemd user unit, the .desktop entry and icons.
install:
    @echo "just install: implemented in M6 (packaging) — res/ does not exist yet." >&2
    @echo "See docs/ROADMAP.md, milestone M6." >&2
    @exit 1

# Remove everything `just install` puts in place.
uninstall:
    @echo "just uninstall: implemented in M6 (packaging) — res/ does not exist yet." >&2
    @echo "See docs/ROADMAP.md, milestone M6." >&2
    @exit 1
