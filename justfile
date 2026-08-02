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

# Takes the binary's flags: `just watch --max-bytes 1024`

# Debug watcher from M1: print every selection and its flavors.
watch *ARGS:
    cargo run -p clippo-wayland --bin clippo-watch -- {{ARGS}}

# `dbus-run-session` gives the suite a private session bus, so clippo-ipc's
# round-trip test runs for real instead of skipping. Everything else is
# unaffected by it.

# The whole test suite, on a private session bus.
test:
    dbus-run-session -- cargo test --workspace

fmt:
    cargo fmt --all

# Everything CI runs, in the same order.
check:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    dbus-run-session -- cargo test --workspace

clean:
    cargo clean

# ---------------------------------------------------------------------------
# Packaging
# ---------------------------------------------------------------------------

# Installation prefix. Empty — the default — is a user-local XDG install under
# ~/.local and ~/.config that needs no root and is what a person building
# clippo for themselves wants. Set it for a system-wide or packaged install:
#
#     just prefix=/usr/local install       # or: PREFIX=/usr/local just install
#     just prefix=/usr destdir=pkg install # staged, for a distro package
#
# The assignment goes *before* the recipe name — that is just's grammar, not a
# choice made here. `just install prefix=/usr/local` is read as a request for a
# recipe called `prefix=/usr/local` and fails. Use the environment variable if
# the trailing form is what your fingers do.
#
prefix := env_var_or_default("PREFIX", "")

# Staging root for packaging. Prepended to every path and to nothing else: with
# it set, nothing outside the staging tree is touched and no cache is refreshed,
# because that is the package manager's job on the installing machine.
destdir := env_var_or_default("DESTDIR", "")

_home := env_var_or_default("HOME", "")

# Set-but-empty is treated as unset, which `env_var_or_default` on its own does
# not do: it hands back the empty string, and `"" / "applications"` is
# `/applications` — a path at the root of the filesystem. Exporting an XDG
# variable empty is a normal enough thing for a shell profile to do that this is
# worth the two extra lines.
_data_env := env_var_or_default("XDG_DATA_HOME", "")
_config_env := env_var_or_default("XDG_CONFIG_HOME", "")
_xdg_data := if _data_env == "" { _home / ".local/share" } else { _data_env }
_xdg_config := if _config_env == "" { _home / ".config" } else { _config_env }

# The user-local layout is XDG's, so XDG_DATA_HOME and XDG_CONFIG_HOME are
# honoured. A prefixed install is FHS's and is not user-specific, so it is not.
# The unit stays a *user* unit either way — clippod needs the session's Wayland
# socket, its session bus and its keyring, none of which a system service has.
bindir := if prefix == "" { _home / ".local/bin" } else { prefix / "bin" }
datadir := if prefix == "" { _xdg_data } else { prefix / "share" }
unitdir := if prefix == "" { _xdg_config / "systemd/user" } else { prefix / "lib/systemd/user" }

# `just --list` shows the *last* comment line above a recipe, so the one-line
# summary goes last and any explanation goes above it.

# Binaries, unit, .desktop, metainfo and icons into ~/.local. Idempotent.
install: build-release
    #!/usr/bin/env bash
    set -euo pipefail

    bin="{{ destdir }}{{ bindir }}"
    apps="{{ destdir }}{{ datadir }}/applications"
    metainfo="{{ destdir }}{{ datadir }}/metainfo"
    icons="{{ destdir }}{{ datadir }}/icons/hicolor"
    units="{{ destdir }}{{ unitdir }}"

    # `install -D` makes the parent directories and overwrites an existing
    # file rather than refusing, which is the whole of what makes a second
    # `just install` a no-op instead of an error.
    #
    # Three binaries. `clippo-watch` is deliberately not among them: it is the
    # M1 capture debugger, it prints clipboard contents unredacted, and it is a
    # `cargo run` away for the one person who wants it.
    install -Dm755 target/release/clippod       "$bin/clippod"
    install -Dm755 target/release/clippo        "$bin/clippo"
    install -Dm755 target/release/clippo-applet "$bin/clippo-applet"

    install -Dm644 res/com.nilfactor.Clippo.desktop \
        "$apps/com.nilfactor.Clippo.desktop"
    install -Dm644 res/com.nilfactor.Clippo.metainfo.xml \
        "$metainfo/com.nilfactor.Clippo.metainfo.xml"
    install -Dm644 res/icons/hicolor/scalable/apps/com.nilfactor.Clippo.svg \
        "$icons/scalable/apps/com.nilfactor.Clippo.svg"
    install -Dm644 res/icons/hicolor/symbolic/apps/com.nilfactor.Clippo-symbolic.svg \
        "$icons/symbolic/apps/com.nilfactor.Clippo-symbolic.svg"

    # The shipped unit says `ExecStart=%h/.local/bin/clippod` — %h is systemd's
    # own $HOME expansion, so for the default install it is already right, and
    # copying it verbatim keeps it right if the home directory ever moves. Any
    # other bindir has to be written in.
    mkdir -p "$units"
    if [ "{{ bindir }}" = "$HOME/.local/bin" ]; then
        install -Dm644 res/clippod.service "$units/clippod.service"
    else
        sed "s|^ExecStart=.*|ExecStart={{ bindir }}/clippod|" res/clippod.service \
            > "$units/clippod.service"
        chmod 644 "$units/clippod.service"
    fi

    if [ -n "{{ destdir }}" ]; then
        echo "staged into {{ destdir }} — caches and systemd left alone" >&2
        exit 0
    fi

    # Cache refreshes are best-effort. Neither tool is guaranteed to exist, and
    # a stale cache costs an icon that takes a moment to appear — not an
    # install that should be reported as failed.
    command -v update-desktop-database >/dev/null 2>&1 \
        && update-desktop-database -q "$apps" || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 \
        && gtk-update-icon-cache -qtf "$icons" || true

    # `daemon-reload` so systemd sees the unit; enabling it is left to the
    # person installing, because starting a daemon that then owns the clipboard
    # is not a thing to do to somebody as a side effect of copying files.
    if [ -z "{{ prefix }}" ] && command -v systemctl >/dev/null 2>&1; then
        systemctl --user daemon-reload 2>/dev/null || true
    fi

    echo
    echo "clippo installed:"
    echo "  binaries   $bin"
    echo "  unit       $units/clippod.service"
    echo "  applet     $apps/com.nilfactor.Clippo.desktop"
    echo
    echo "Next:"
    echo "  systemctl --user enable --now clippod    # start it, and on every login"
    echo "  journalctl --user -u clippod -f          # watch it"
    echo
    echo "Then add clippo to the panel: Settings -> Desktop -> Panel ->"
    echo "Configure panel applets. Run this from a host terminal, not a Flatpak;"
    echo "\$WAYLAND_DISPLAY must be wayland-0. See the README."

# Leaves the clipboard history and the keyring entry alone — `just purge-data`
# is the one that deletes those.

# Remove everything `just install` put in place, keeping your history.
uninstall:
    #!/usr/bin/env bash
    set -euo pipefail

    bin="{{ destdir }}{{ bindir }}"
    apps="{{ destdir }}{{ datadir }}/applications"
    metainfo="{{ destdir }}{{ datadir }}/metainfo"
    icons="{{ destdir }}{{ datadir }}/icons/hicolor"
    units="{{ destdir }}{{ unitdir }}"

    # Stop it before removing the binary underneath it. `--now` stops and
    # disables in one go; both are ignorable, since "not installed" and "not
    # running" are exactly what `uninstall` is trying to arrive at.
    if [ -z "{{ destdir }}" ] && [ -z "{{ prefix }}" ] && command -v systemctl >/dev/null 2>&1; then
        systemctl --user disable --now clippod 2>/dev/null || true
    fi

    # Every `rm -f` succeeds on a file that is not there, which is what makes
    # `just uninstall` on a clean machine a no-op rather than an error.
    rm -f "$bin/clippod" "$bin/clippo" "$bin/clippo-applet"
    rm -f "$units/clippod.service"
    rm -f "$apps/com.nilfactor.Clippo.desktop"
    rm -f "$metainfo/com.nilfactor.Clippo.metainfo.xml"
    rm -f "$icons/scalable/apps/com.nilfactor.Clippo.svg"
    rm -f "$icons/symbolic/apps/com.nilfactor.Clippo-symbolic.svg"

    # Tidy up the icon directories clippo itself created, deepest first, and
    # only if they are empty — plain `rmdir` refuses otherwise, which is the
    # check. Not `rmdir -p`: that walks *upward* until something is non-empty,
    # so on a sparse home it would climb from ~/.local/share/metainfo through
    # ~/.local and take $HOME with it. Deleting a home directory is not a thing
    # `uninstall` gets to do.
    #
    # ~/.local/bin, ~/.local/share/applications and ~/.config/systemd/user are
    # left alone whether or not they are empty: they are shared with every
    # other program that installs into a home directory, and clippo does not
    # own them.
    rmdir "$icons/scalable/apps" "$icons/scalable" \
          "$icons/symbolic/apps" "$icons/symbolic" \
          "$metainfo" 2>/dev/null || true

    if [ -n "{{ destdir }}" ]; then
        exit 0
    fi

    command -v update-desktop-database >/dev/null 2>&1 \
        && update-desktop-database -q "$apps" 2>/dev/null || true
    command -v gtk-update-icon-cache >/dev/null 2>&1 \
        && gtk-update-icon-cache -qtf "$icons" 2>/dev/null || true

    if [ -z "{{ prefix }}" ] && command -v systemctl >/dev/null 2>&1; then
        systemctl --user daemon-reload 2>/dev/null || true
    fi

    data="${XDG_DATA_HOME:-$HOME/.local/share}/clippo"
    echo
    echo "clippo removed."
    echo
    echo "Your clipboard history was NOT deleted. It is still at"
    echo "  $data/history.db"
    echo "and its key is still in the keyring. Uninstalling a program is not a"
    echo "reason to throw away what it was keeping for you — if you do want it"
    echo "gone, that is \`just purge-data\`, or delete the directory by hand."

# Deliberately not part of `uninstall`: this is the only recipe here that
# destroys anything, so it has to be asked for by name, and it asks again
# before it does it.

# Delete the clipboard history and its key. Asks first.
purge-data:
    #!/usr/bin/env bash
    set -euo pipefail

    data="${XDG_DATA_HOME:-$HOME/.local/share}/clippo"

    if [ ! -e "$data" ]; then
        echo "nothing to purge: $data does not exist"
        exit 0
    fi

    echo "This deletes $data — the encrypted history and, if the keyring was"
    echo "unreachable when clippo first ran, the key file beside it."
    read -r -p "Delete it? [y/N] " reply
    case "$reply" in
        [yY]*) ;;
        *) echo "not deleted" >&2; exit 1 ;;
    esac

    rm -rf "$data"
    echo "deleted $data"
    echo
    echo "The keyring entry is separate and is still there. Remove it in"
    echo "Passwords and Keys (look for 'clippo clipboard history database key'),"
    echo "or with:"
    echo "  secret-tool clear xdg:schema com.nilfactor.Clippo.DatabaseKey \\"
    echo "                    application clippo purpose history-database-key"
