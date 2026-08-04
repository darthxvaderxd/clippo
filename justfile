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

# The build half of what CI runs, in the same order.
#
# CI runs one more thing — `just deny` — which is left out of here because it
# needs a tool that is not part of a Rust install and a network round trip to
# the advisory database, neither of which belongs in the command you run before
# every commit.
check:
    cargo fmt --check
    cargo clippy --workspace --all-targets -- -D warnings
    dbus-run-session -- cargo test --workspace

# The dependency tree against the RustSec advisory database. See deny.toml.
#
# Needs `cargo install --locked cargo-deny` (or the binary from its releases)
# and network access; it reads Cargo.lock rather than building anything, so it
# does not need the workspace to compile.
deny:
    cargo deny check advisories

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

# An XDG variable is honoured only when it holds an *absolute* path; anything
# else — unset, empty, or relative — falls back to $HOME. That is what the XDG
# spec requires, and it is the rule `clippo-core::paths` already implements
# (`resolve_from`, and the `an_unset_or_relative_xdg_value_falls_back_to_home`
# test beside it). It has to be the same rule in both places: the daemon reads
# its history from wherever `paths` says, so a justfile that resolved a relative
# `XDG_DATA_HOME=share` against the working directory would install the unit and
# the .desktop into the checkout, and — worse — point `purge-data`'s `rm -rf` at
# a directory that is not the user's history while reporting that it was.
#
# Empty is the case a shell profile actually produces, and `"" / "applications"`
# is `/applications`, a path at the root of the filesystem. Relative is the case
# a `just`-from-the-wrong-place produces. `=~ '^/'` covers both.
_data_env := env_var_or_default("XDG_DATA_HOME", "")
_config_env := env_var_or_default("XDG_CONFIG_HOME", "")
_xdg_data := if _data_env =~ '^/' { _data_env } else { _home / ".local/share" }
_xdg_config := if _config_env =~ '^/' { _config_env } else { _home / ".config" }

# Where the daemon keeps the encrypted history — `clippo-core::paths::data_dir`,
# resolved by the rule above. Never under `prefix` or `destdir`: it is the
# user's data, not part of the installation, and `uninstall` says so while
# `purge-data` is the only thing that touches it.
_data_dir := _xdg_data / "clippo"

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

    # The .desktop ships `Exec=clippo-applet`, a bare name, and gets the
    # installed path written in. `.desktop` has no `%h`, and the PATH that
    # matters is not the one you ran `just` from — it is cosmic-panel's, which
    # for a session started by the display manager may well not have
    # ~/.local/bin on it. An absolute Exec is the difference between the applet
    # starting and the panel silently having a gap where it should be.
    mkdir -p "$apps"
    sed "s|^Exec=.*|Exec={{ bindir }}/clippo-applet|" res/com.nilfactor.Clippo.desktop \
        > "$apps/com.nilfactor.Clippo.desktop"
    chmod 644 "$apps/com.nilfactor.Clippo.desktop"

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
    #
    # Not guarded on `prefix`: `$prefix/lib/systemd/user` is in the user
    # manager's search path too, so a prefixed install needs the reload for the
    # same reason a user-local one does. (`destdir` has already exited above —
    # a staging tree is nothing to this machine's systemd.)
    if command -v systemctl >/dev/null 2>&1; then
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
    echo "  just add-to-panel                        # put the icon in the panel"
    echo "  journalctl --user -u clippod -f          # watch it"
    echo
    echo "Neither of the first two happens on its own: starting a daemon that owns"
    echo "your clipboard, and rearranging your panel, are not things to do to"
    echo "somebody as a side effect of copying files. \`add-to-panel\` puts clippo at"
    echo "the front of the right wing; Settings -> Desktop -> Panel -> Configure"
    echo "panel applets is the same job by hand, and moves it afterwards."
    echo
    echo "Run all of this from a host terminal, not a Flatpak;"
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
    #
    # Guarded on `destdir` only, not on `prefix`. A staging tree has no running
    # daemon to stop, but a prefixed install very much does — the unit is a
    # *user* unit at every prefix — and removing the binary out from under it
    # empties the clipboard, which is the one consequence this recipe is meant
    # to avoid causing by surprise.
    if [ -z "{{ destdir }}" ] && command -v systemctl >/dev/null 2>&1; then
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

    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user daemon-reload 2>/dev/null || true
    fi

    # `{{ _data_dir }}`, not a second `${XDG_DATA_HOME:-…}` expansion: `:-`
    # honours a relative value where the daemon ignores it, so resolving it here
    # a second way is how the two drift apart.
    data="{{ _data_dir }}"
    echo
    echo "clippo removed."
    echo
    if [ -e "$data/history.db" ]; then
        echo "Your clipboard history was NOT deleted. It is still at"
        echo "  $data/history.db"
        echo "and its key is still in the keyring. Uninstalling a program is not a"
        echo "reason to throw away what it was keeping for you — if you do want it"
        echo "gone, that is \`just purge-data\`, or delete the directory by hand."
    else
        echo "There was no clipboard history at $data to keep."
    fi

# Deliberately not part of `uninstall`: this is the only recipe here that
# destroys anything, so it has to be asked for by name, and it asks again
# before it does it.

# Delete the clipboard history and its key. Asks first.
purge-data:
    #!/usr/bin/env bash
    set -euo pipefail

    # The same one resolution as everywhere else. This is the recipe that runs
    # `rm -rf`, so it is the one where getting the directory wrong costs most:
    # a path resolved by a different rule than the daemon's deletes something
    # that is not the history and reports that the history is gone.
    data="{{ _data_dir }}"

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

# ---------------------------------------------------------------------------
# Panel placement
# ---------------------------------------------------------------------------

# The name cosmic-panel knows an applet by: its `.desktop`'s basename, without
# the extension.
_applet_id := "com.nilfactor.Clippo"

# cosmic-panel's layout for the top panel — a RON `Some((left, right))` naming
# the applets in each of the two wings, either side of the centre.
#
# Editing this file is the *only* way to say where clippo sits, because nothing
# declarative can. cosmic-panel reads exactly three keys out of an applet's
# `.desktop` — `X-CosmicShrinkable`, `X-CosmicHoverPopup` and
# `X-NotificationsAppletClients` — and cosmic-settings reads one,
# `X-CosmicApplet`, which is what makes clippo *offerable* and says nothing
# about position. There is no key for a wing or an index, so `res/` cannot
# carry a preferred placement however much we would like it to. Somebody has to
# write this file, and these two recipes are that somebody.
#
# Which is also why this is its own recipe rather than part of `install`, for
# the same reason `install` does not `systemctl enable` the daemon:
# rearranging somebody's panel is not a thing to do to them as a side effect of
# copying files. `install` suggests it; you run it.
_wings := _xdg_config / "cosmic/com.system76.CosmicPanel.Panel/v1/plugins_wings"

# `just remove-from-panel` takes it off again, leaving the rest of the panel be.

# Put the clippo icon in the panel's right wing. Idempotent.
add-to-panel: (_panel "add")

# Take the clippo icon off the panel. Idempotent.
remove-from-panel: (_panel "remove")

_panel MODE:
    #!/usr/bin/env bash
    set -euo pipefail

    mode="{{ MODE }}"
    id="{{ _applet_id }}"
    wings="{{ _wings }}"

    case "$mode" in add|remove) ;; *) echo "_panel: bad mode $mode" >&2; exit 2 ;; esac

    # cosmic-config layers your file over a system default, so a missing or
    # `None` user file means *the distro's layout*, not an empty panel. Reading
    # that default in those cases is the whole of what stops `add-to-panel`
    # from writing a panel with clippo on it and nothing else — which is to say
    # from silently removing every other applet you have.
    seed=""
    IFS=: read -r -a data_dirs <<< "${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
    for d in "${data_dirs[@]}"; do
        [ -n "$d" ] || continue
        cand="$d/cosmic/com.system76.CosmicPanel.Panel/v1/plugins_wings"
        if [ -r "$cand" ]; then seed="$cand"; break; fi
    done

    if [ -s "$wings" ] && [ "$(tr -d ' \t\r\n' < "$wings")" != "None" ]; then
        src="$wings"
    elif [ "$mode" = "remove" ]; then
        # Nothing of your own to edit means nothing of yours has clippo in it.
        # Materialising the default just to take an absent applet out of it
        # would turn a no-op into a permanent override of a file you never had.
        echo "no panel layout of your own — nothing to remove"
        exit 0
    elif [ -n "$seed" ]; then
        src="$seed"
        echo "no panel layout of your own yet — starting from $seed" >&2
    else
        echo "cannot find a panel layout to edit." >&2
        echo "  yours:   $wings" >&2
        echo "  default: <XDG_DATA_DIRS>/cosmic/com.system76.CosmicPanel.Panel/v1/plugins_wings" >&2
        echo >&2
        echo "Add clippo in Settings -> Desktop -> Panel -> Configure panel applets" >&2
        echo "instead; that writes the file, after which this recipe can move it." >&2
        exit 1
    fi

    # cosmic-settings holds this file in memory and writes the whole thing back
    # when you touch anything on its Panel page, so an edit made underneath a
    # running one lasts until the next click and no longer.
    if pgrep -x cosmic-settings >/dev/null 2>&1; then
        echo "warning: cosmic-settings is running. It will overwrite this edit with" >&2
        echo "         its own copy if you change anything on its Panel page." >&2
    fi

    mkdir -p "$(dirname "$wings")"
    tmp="$(mktemp "$(dirname "$wings")/.plugins_wings.XXXXXX")"
    trap 'rm -f "$tmp"' EXIT

    # Written to match cosmic-settings' own formatting byte for byte, trailing
    # newline included — it does not write one — so that the compare below can
    # tell "already in place" from "changed" without a diff that is only
    # whitespace.
    awk -v id="$id" -v mode="$mode" '
        # An applet ID never contains a bracket, so the two bracketed runs in
        # the file are exactly the two wings and can be found by position
        # without parsing RON properly.
        function ids(s, arr,   n, i, m, part, t) {
            n = split(s, part, ",")
            m = 0
            for (i = 1; i <= n; i++) {
                t = part[i]
                gsub(/^[ \t\r\n"]+/, "", t)
                gsub(/[ \t\r\n"]+$/, "", t)
                if (t != "") arr[++m] = t
            }
            return m
        }
        function emit(arr, n,   i, out) {
            out = "[\n"
            for (i = 1; i <= n; i++) out = out "    \"" arr[i] "\",\n"
            return out "]"
        }
        { doc = doc " " $0 }
        END {
            if (gsub(/\[/, "[", doc) != 2 || gsub(/\]/, "]", doc) != 2) {
                print "plugins_wings is not the expected Some(([..], [..])) shape;" > "/dev/stderr"
                print "refusing to guess at it. Edit it by hand." > "/dev/stderr"
                exit 1
            }
            a = index(doc, "[")
            b = a + index(substr(doc, a + 1), "]")
            c = b + index(substr(doc, b + 1), "[")
            d = c + index(substr(doc, c + 1), "]")
            nl = ids(substr(doc, a + 1, b - a - 1), L)
            nr = ids(substr(doc, c + 1, d - c - 1), R)

            # Taken out of both wings first, so `add` after a placement made by
            # hand moves the icon instead of giving you two of them.
            ol = 0; for (i = 1; i <= nl; i++) if (L[i] != id) OL[++ol] = L[i]
            orr = 0; for (i = 1; i <= nr; i++) if (R[i] != id) OR[++orr] = R[i]

            if (mode == "add") {
                # Front of the right wing: the inner edge of the right-hand
                # group, next to the centre and ahead of the status applets,
                # which is where a thing you open on purpose wants to be rather
                # than lost among the indicators.
                for (i = orr; i >= 1; i--) OR[i + 1] = OR[i]
                OR[1] = id
                orr++
            }

            printf "Some((%s, %s))", emit(OL, ol), emit(OR, orr)
        }
    ' "$src" > "$tmp"

    if [ -e "$wings" ] && cmp -s "$tmp" "$wings"; then
        rm -f "$tmp"; trap - EXIT
        if [ "$mode" = "add" ]; then
            echo "clippo is already at the front of the panel's right wing — nothing changed"
        else
            echo "clippo is not on the panel — nothing changed"
        fi
        exit 0
    fi

    # Match the mode of the file being replaced rather than imposing one: it is
    # cosmic-settings' file, and whatever umask it was written under is not
    # ours to have an opinion about. 644 only for one we are creating.
    if [ -e "$wings" ]; then
        chmod --reference="$wings" "$tmp"
    else
        chmod 644 "$tmp"
    fi
    mv -f "$tmp" "$wings"
    trap - EXIT

    echo
    if [ "$mode" = "add" ]; then
        echo "clippo added to the front of the panel's right wing."
    else
        echo "clippo removed from the panel."
    fi
    echo "  $wings"

    if [ "$mode" = "add" ] && [ ! -x "{{ bindir }}/clippo-applet" ]; then
        echo
        echo "note: {{ bindir }}/clippo-applet is not there, so the panel will show a" >&2
        echo "      gap until \`just install\` has run." >&2
    fi

    echo
    echo "cosmic-panel watches that file and should redraw within a second or two."
    echo "If it does not, \`pkill cosmic-panel\` — the session brings it straight back."
