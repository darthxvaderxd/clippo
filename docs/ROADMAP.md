# clippo — Roadmap

> Companion to [DESIGN.md](DESIGN.md), which covers the *what* and *why*. This file covers the
> order of work and how we know each stage actually works.

## Milestones

Each milestone ends at something runnable on the host — not at a layer that only compiles.

### M0 — scaffold

Cargo workspace, `justfile` (`build`, `run-daemon`, `install`, `uninstall`), `rustfmt.toml`,
`clippy.toml`, and a GitHub Actions workflow running fmt + clippy + test.

### M1 — capture

`clippo-wayland`, plus a debug binary `clippo-watch` that prints every selection and its
flavors.

This comes first deliberately: the data-control client is the one component where protocol
surprises can invalidate the design, so it should fail early if it's going to fail.

- [x] Bind a data-control manager, preferring `ext_data_control_v1` and falling back to
      `zwlr_data_control_v1`, and report which one bound.
- [x] Fail with a message naming both protocols and the live `$WAYLAND_DISPLAY` when neither
      binds, so the Flatpak-socket cause is diagnosable from the output alone.
- [x] Watch the seat's selection, collect each offer's advertised MIME types, and receive every
      interesting flavor into one atomic `Selection`.
- [x] Cap each flavor and drop rather than truncate what exceeds it, carrying the reason
      through to the caller instead of dropping it silently.
- [x] `clippo-watch` prints one block per selection: advertised types, which were fetched with
      their byte sizes and a preview, which were dropped and why, and which were skipped.

> **Gate:** run `clippo-watch` from a host terminal, copy text / an image / a file, and see all
> flavors printed for each. This is [verification 1](#1-protocol-availability) below, and it is
> manual — there is no compositor in CI.

### M2 — storage

`clippo-store` with SQLCipher, keyring-backed key, dedup, retention, images and thumbnails.
Unit-tested against a temp DB.

### M3 — daemon + CLI

`clippod`, the D-Bus interface, the `clippo` CLI, and the copy-back offer path including the
self-echo guard.

- [x] `clippo-ipc` defines the interface once — the served side and the proxy both generated
      from the same member list, so a signature cannot drift between the daemon and a frontend.
- [x] `clippod` owns the store and the Wayland watcher, records every capture, and serves
      `com.nilfactor.Clippo` on the session bus.
- [x] `clippo list|search|copy|pin|rm|clear|pause|reveal`, each one proxy call, with a short
      typeable id, a terminal-safe table, `--json`, and one clear message when the daemon is
      not running.
- [x] The copy-back offer path and the self-echo guard. `Copy` owns a data-control source
      advertising every stored flavor except the derived thumbnail, answers each paste on a
      non-blocking fd, and bumps the entry. The guard is armed for one capture, so the
      copy-back does not re-enter the history and a deliberate re-copy still does.

> **Gate:** `clippo list` and `clippo copy <id>` work end to end, and pasting into another app
> yields the right content. This is [verification 2](#2-round-trip) below, and it is manual —
> no CI runner has a compositor.

### M4 — secrets

Detection, masking, `Reveal`, and the fixture corpus with its tests.

### M5 — applet

libcosmic UI: search, pins, images, live updates via `HistoryChanged`, and `Toggle()`.

### M6 — packaging

systemd user unit, `.desktop`, metainfo, icons, `just install`, and a README — including the
"run from a host terminal, not from a Flatpak" warning.

## Verification

**Build and run on the host, not inside RustRover's Flatpak.** This is the single most
important testing note; see the environment constraints in [DESIGN.md](DESIGN.md).

```sh
# one-time, on the host
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# confirm you are NOT on the proxied socket — must print wayland-0, not wayland-1
echo $WAYLAND_DISPLAY
```

### 1. Protocol availability

Run `cargo run -p clippo-wayland --bin clippo-watch` from `cosmic-term`. It prints which
protocol it bound, then one block per copy. Each should print its full flavor list:

- [ ] Plain text from `cosmic-term` — `text/plain;charset=utf-8` and `text/plain`.
- [ ] Formatted text from a browser — `text/html` alongside the plain-text flavors.
- [ ] A file selected in `cosmic-files` — `text/uri-list`.
- [ ] A screenshot — `image/png`, reported as binary with its size rather than dumped.
- [ ] A password copied from KeePassXC — the `x-kde-passwordManagerHint` line appears.
- [ ] `--max-bytes 1024`, then copy a screenshot — `image/png` is listed under `dropped:`
      naming the cap, not silently missing.

If it reports no data-control manager, you are on the Flatpak-proxied socket; the error names
both protocols and prints the `$WAYLAND_DISPLAY` it saw.

### 2. Round trip

With `clippod` running in one host terminal and `clippo` in another:

- [ ] Copy three things — `clippo list` shows three entries, newest first.
- [ ] `clippo copy 2`, then `Ctrl+V` in `cosmic-edit` — entry 2 pastes verbatim.
- [ ] Copy the same text twice → still one entry, and its `AGE` column resets rather than a
      second row appearing.
- [ ] `clippo reveal 2 | wc -c` counts the whole value, with no newline added.
- [ ] Stop `clippod` → every subcommand says clippod is not running and exits non-zero, **and
      the clipboard is empty**: the daemon was the selection owner. Expected Wayland behaviour,
      documented in the README.

Every box here is manual and stays unchecked in the repository — there is no compositor in this
environment or in CI, so nobody can tick them from a test run. The copy-back path they exercise
is implemented as of M3c; what is automated of it is the self-echo integration test in
`clippod` (verification 7 below), against an in-process stand-in for the compositor.

### 3. Encryption

```sh
strings ~/.local/share/clippo/history.db | grep '<known-copied-string>'   # must find nothing
sqlite3 ~/.local/share/clippo/history.db .tables                         # must fail: not a database
```

### 4. Secrets

Copy a KeePassXC password, an `sk-`-prefixed token, and a JWT. Each should show as
`ab••••••••yz` in both `clippo list` and the applet, but paste in full.

Then the false-positive check: copy a git SHA, a UUID, and a paragraph of prose, and confirm
none are flagged.

### 5. Pins and retention

Pin an entry, set `max_entries = 5`, copy ten things. The pinned entry survives, and five
unpinned remain.

### 6. Restart resilience

`systemctl --user restart clippod` → `clippo list` still returns the history. Kill and restart
`cosmic-panel` → the applet reconnects.

### 7. Automated

`cargo test --workspace`, covering:

- `clippo-core` — detection and masking against the fixture corpus
- `clippo-store` — dedup, retention, and pin-exemption against a temp DB
- `clippo-ipc` — the proxy and the interface over a real bus, which needs
  `dbus-run-session -- cargo test --workspace` (what `just test` and CI run)
- `clippo-cli` — argument parsing, id resolution, and the table, JSON and escaping against
  fixture `EntrySummary` values
- `clippod` — the **self-echo loop**, which DESIGN.md's risk table asks for by name. `Copy`
  goes to a fake clipboard, what it was handed comes back as a capture, and the test asserts
  both directions: the copy-back adds no second entry, and copying the same content by hand
  afterwards still bumps the existing one.

The Wayland protocol and UI layers stay manual, and so does the CLI's live round trip against a
running daemon — that is verification 2 above. What the automated tests cover of the offer half
is everything either side of the protocol: the flavor list, the thumbnail exclusion, the
non-blocking write against a real pipe, and the guard.
