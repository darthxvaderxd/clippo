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

- [x] `clippo-core::secrets` computes `sensitive` at capture from all three of DESIGN.md's
      signals, each a module of its own with its own entry point rather than one fused
      predicate: the `x-kde-passwordManagerHint` marker, the provider-token shapes, and the
      entropy heuristic. `detect` reports *which* rule fired, so a false positive can be
      answered by name.
- [x] Shape regexes for every provider DESIGN.md lists — `sk-…`, `ghp_`/`gho_`/`github_pat_`,
      `AKIA…`, `xox[baprs]-`, JWT, `-----BEGIN … PRIVATE KEY-----` and `postgres://user:pass@`
      — each widened to the rest of its family and each with a corpus fixture.
- [x] The entropy rule gated on a single token, 8–128 characters, three or more character
      classes and above 3.5 bits/char, disableable through `[secrets] entropy_rule` while the
      MIME and shape rules keep working.
- [x] A password-manager copy is **masked, not skipped** — stored, listed and pasteable like
      any other entry.
- [x] `mask()` renders `ab••••••••yz`: configurable first and last counts, a fixed-width bullet
      run that does not leak the value's length, and no panic or leak on a short value, on
      multi-byte UTF-8, or on a grapheme cluster boundary.
- [x] Masking is display-only. The preview is masked before it is stored, so `List` and
      `Search` have no unmasked one to return; `Reveal(id)` is the only member that returns a
      whole value; and `Copy` puts the real bytes on the clipboard. Entries captured before
      this milestone keep their old preview — there is no migration pass — until the next
      copy of the same value, which re-detects it and replaces the preview with a mask.
- [x] A fixture corpus in both directions at `crates/clippo-core/tests/corpus.toml` — tokens,
      high-entropy secrets and a hinted password against git SHAs, UUIDs, base64 blobs,
      minified JS, prose, URLs and paths — with a test asserting every fixture's
      classification, failing loudest on a missed secret, and refusing a shape rule that has no
      fixture.
- [x] The entropy threshold's tuning rationale written down next to the constant, with the
      measured figures for the fixtures either side of it asserted by a test.

### M5 — applet

libcosmic UI: search, pins, images, live updates via `HistoryChanged`, and `Toggle()`.

- [x] `clippo-applet` is a `libcosmic` applet on the `applet` feature, run through
      `cosmic::applet::run`: a panel icon opening a popup with an auto-focused search field
      above a scrollable list.
- [x] Keyboard-first, every binding DESIGN.md names and none of them needing the mouse — type
      to filter, `↑`/`↓` to move, `Enter` to copy and close, `Delete` to remove, `Ctrl+P` to
      pin, `Ctrl+R` to reveal, `Escape` to dismiss. The keys are read from the runtime's event
      stream rather than from the list widget, because the search field holds focus the whole
      time the picker is open — which is what makes typing filter without a mouse.
- [x] Filtering is the daemon's `Search`, not a local filter, so the applet and `clippo search`
      rank one query identically. The model asserts it hands back the daemon's order verbatim.
- [x] Sensitive rows draw the daemon's mask plus a lock badge, and `Ctrl+R` reveals one row in
      place through `Reveal(id)`. The revealed value is **never cached**: it is answered only
      while its own row is still selected, so moving the highlight stops it being drawn without
      anything having to remember to clear it, and it is dropped when the popup closes —
      whether the applet closed it or the compositor did. It is held in a `Zeroizing<String>`
      from the moment it arrives on the bus, so dropping it wipes the memory rather than
      freeing it with the secret still in place; zbus's own deserialisation buffer is the one
      hop clippo does not own and does not wipe. The event carrying it has a hand-written
      `Debug` that prints a length, so a `debug!` added later cannot put it in the journal.
- [x] Image rows draw the stored `image/png;clippo-thumb` flavor, fetched with the new
      `Thumbnail(id)` member. The applet never asks for a full-size blob — and neither does the
      daemon serving it, which reaches the thumbnail with a targeted read rather than through
      `Store::get`, so the full-size PNG beside it is never read or decrypted. A row whose
      thumbnail is missing draws the generic image icon rather than falling back to the real
      image, and each entry is asked at most once *that the request was actually queued*, so a
      history of oversized screenshots is not re-fetched per keystroke and a list with more
      image rows than the request channel holds still finishes fetching. The cache is keyed on
      `(id, created_at)`: SQLite reissues a deleted id to the next insert, so an id alone would
      eventually draw a deleted screenshot beside the entry that inherited its id.
- [x] Live updates by subscription, with nothing polled. `HistoryChanged` refreshes the list
      while the popup is open, and `NameOwnerChanged` filtered to the daemon's name is how the
      applet notices `clippod` stopping and starting. *While the popup is open* is literal: the
      signal fires on every copy anyone makes and the picker is closed for almost all of them,
      so a refresh with nothing on screen would spend a ranked `Search`, a list of previews and
      a `Thumbnail` round trip per copied screenshot for no one. Nothing is lost by waiting —
      opening the picker refreshes as it opens.
- [x] The applet reconnects with no reconnection code — a zbus signal stream is a match rule
      held by the bus and a proxy is a name and a path, so both outlive the daemon they refer
      to — and shows an explicit "clippod is not running" panel rather than an empty list that
      would read as a lost history.
- [x] `Toggle()` on `com.nilfactor.ClippoApplet`, a second interface served by the applet and
      called by the new `clippo show`. libcosmic supports opening the picker programmatically;
      what it cannot do is give an `xdg_popup` keyboard focus without an input serial, so the
      picker is a layer surface with `KeyboardInteractivity::Exclusive`. The reasoning is in
      [DESIGN.md](DESIGN.md#decisions-on-record), and the surface-hosting layer being one file
      is what made the swap cheap when the gate below failed.
- [x] Every action goes through the same D-Bus members the CLI uses — `Copy`, `Delete`, `Pin`,
      `Search`, `Reveal`. There is no second code path and no direct store access: the applet
      has no `clippo-store` dependency and cannot reach the history any other way.

> **Gate:** the applet survives `cosmic-panel` being killed and restarted, and its picker takes
> keyboard focus when opened by `clippo show`. This is
> [verification 6](#6-restart-resilience) below, and it is manual — the picker's keyboard focus
> in particular cannot be settled by reading libcosmic's source, only by a compositor. Run on a
> host, it failed as an `xdg_popup` and the picker was moved to a layer surface; see DESIGN.md's
> decisions section.

### M6 — packaging

systemd user unit, `.desktop`, metainfo, icons, `just install`, and a README — including the
"run from a host terminal, not from a Flatpak" warning.

- [x] `res/clippod.service` is a systemd **user** unit with `WantedBy=cosmic-session.target`
      and `Restart=on-failure`. `PartOf` the same target, so logging out stops it rather than
      leaving a daemon holding the history open against a compositor that is gone, and
      `Type=exec` so a missing binary is reported as a failed start rather than as a start
      that succeeded and immediately died.
- [x] `res/com.nilfactor.Clippo.desktop` carries `X-CosmicApplet=true`, without which clippo
      is not offered in COSMIC's panel configuration at all, and `NoDisplay=true`, because an
      applet started from the app library puts a second panel icon nowhere useful. It passes
      `desktop-file-validate` with no output.
- [x] `res/com.nilfactor.Clippo.metainfo.xml` passes `appstreamcli validate`, with the
      GPL-3.0-only project licence, a summary, a description and the project URL. The one
      remaining `--pedantic` note is `cid-contains-uppercase-letter`, which is inherent: the
      component id has to match the `.desktop` name, and that is COSMIC's convention.
- [x] A scalable icon at `res/icons/hicolor/scalable/apps/com.nilfactor.Clippo.svg` and a
      symbolic one at `res/icons/hicolor/symbolic/apps/com.nilfactor.Clippo-symbolic.svg` —
      the second being what the panel actually draws. `clippo-applet` asks for its own icon
      by name now, with `edit-paste-symbolic` as a fallback so a working copy run without
      installing still gets a glyph rather than a hole.
- [x] `just install` builds release and installs the three binaries and every resource to the
      XDG user locations, refreshes the desktop and icon caches, and needs no root.
      `clippo-watch` is deliberately not installed: it prints clipboard contents unredacted.
- [x] `just uninstall` stops and disables the unit first, then removes exactly what `install`
      placed. It does **not** delete `history.db` or the keyring entry — `just purge-data` is
      the separate, named, confirmed recipe for that, and it is the only one here that
      destroys anything.
- [x] Both are idempotent: `install` twice overwrites, `uninstall` with nothing installed
      exits 0 quietly.
- [x] `prefix` and `destdir` overrides, by variable or environment, for a system-wide or
      staged packaged install, with the unit's `ExecStart` rewritten to match and the default
      user-local behaviour unchanged.
- [x] The README covers what clippo is, secret masking, install and uninstall, where the
      history lives so it can be removed by hand, the `Super+V` RON snippet,
      `journalctl --user -u clippod -f`, the clipboard emptying when `clippod` dies, and — at
      the top, with the `echo $WAYLAND_DISPLAY` check — the host-terminal-not-Flatpak warning.

> **Gate:** `systemctl --user enable --now clippod` on the target host starts the daemon on
> login, and clippo appears in the panel applet list. This is
> [verification 6](#6-restart-resilience) below, and it is manual — this environment has no
> COSMIC session and no running systemd user manager, so the unit is verified only as far as
> `systemd-analyze verify --user` accepting it.

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

- [ ] A KeePassXC password shows as `ab••••••••yz` with `s` in the `FL` column.
- [ ] An `sk-`-prefixed token and a JWT do the same.
- [ ] `clippo copy <id>` on each, then `Ctrl+V` in `cosmic-edit` — each pastes **in full**. A
      mask reaching the clipboard is the highest-severity bug this feature can have.
- [ ] `clippo reveal <id>` prints each value whole.
- [ ] A git SHA, a UUID and a paragraph of prose are not flagged: no `s`, no bullets.
- [ ] The applet renders the same masks with a lock badge, and `Ctrl+R` on one of those rows
      shows the whole value until the highlight moves off it.

Manual for the usual reason: the first two need a compositor and a password manager, and the
third needs somewhere to paste. They stay unchecked in the repository. The automated half is
the fixture corpus and the daemon tests — `crates/clippo-core/tests/corpus.toml` covers both
directions of detection, and `clippod` asserts that no sensitive value appears in a `List` or
`Search` payload and that `Copy` offers the stored bytes rather than the mask. What no test
here can cover is the compositor actually handing a paste the bytes clippo wrote.

The README's [Secrets](../README.md#secrets) section documents this list for users, along with
what each rule fires on and how to turn the entropy one off.

### 5. Pins and retention

Pin an entry, set `max_entries = 5`, copy ten things. The pinned entry survives, and five
unpinned remain.

### 6. Restart resilience

- [ ] `just install`, then `systemctl --user enable --now clippod` → the daemon starts, and
      starts again on the next login. This is M6's gate.
- [ ] `systemctl --user restart clippod` → `clippo list` still returns the history.
- [ ] With the popup open, `systemctl --user stop clippod` → the picker says "clippod is not
      running" rather than going blank. Start it again → the list comes back on its own, with
      nothing reopened and no keypress.
- [ ] Kill and restart `cosmic-panel` → the applet comes back and its popup still opens. This
      is M5's gate.
- [ ] Copy something in another window with the popup open → the new entry appears at the top
      without the popup being reopened, and `clippo rm <id>` in a terminal removes that row.
- [ ] `clippo show` from a terminal opens the picker, and a second `clippo show` closes it.
      Then bind it to `Super+V` as the README's "Global shortcut" section describes and repeat
      — **the picker must take keyboard focus**, i.e. typing filters the list *and* `↑`/`↓`
      move the highlight, with no click first. This was run and it **failed** as a popup: no
      input serial from a global shortcut means no `xdg_popup` grab, and cosmic-comp gives an
      ungrabbed popup no keyboard. The blinking caret was misleading — `text_input::focus` sets
      the widget's own focus state, which is what the caret blinks off, not the compositor's.
      The fallback was taken: the picker is now a `zwlr_layer_shell_v1` surface with
      `KeyboardInteractivity::Exclusive`, which asks for focus on map. This was rerun on a real
      session and **passes**: typing filters, `↑`/`↓` move the highlight, and `Escape` closes,
      all with no click first. The three things the popup's grab used to do for free were
      checked with it — `Escape` closes, the panel icon still toggles, and a second
      `clippo show` still closes — because **a click outside the picker no longer dismisses
      it**.

      Getting there needed one thing that is not in the protocol. `cosmic-panel` proxies an
      applet's layer surface out to `cosmic-comp` and forwards the reply back *only when the
      size it returns differs from the size we asked for*
      (`xdg_shell_wrapper/client/handlers/layer_shell.rs`, where `send_configure` is guarded by
      `requested_size != configure.new_size`). Ask for a fully-specified size, get it granted
      verbatim, and no configure is ever delivered — so iced never renders and never attaches a
      buffer, while the compositor has already handed the surface the exclusive keyboard focus.
      The symptom is a picker that is invisible and swallows every keystroke, with nothing
      logged anywhere. The surface therefore asks for no size at all — anchored to all four
      edges, both axes left to the compositor — which is what makes the two differ. It comes back
      as the whole output, and the picker is centred inside it. If a future `cosmic-panel`
      forwards the configure unconditionally, `surface.rs` can go back to asking for a size.
- [ ] The picker is **centred on screen** and picker-sized, not stretched across the output and
      not parked in a corner. A corner means something has shrunk the surface back to its
      contents — `Context::popup_container` did exactly that, via the `Autosize` it returns, and
      is why the frame in `surface.rs` is built by hand.
- [ ] With clippo *not* on the panel, `clippo show` says the applet is not running and names
      the panel setting — not a raw bus error, and not the daemon's message.

Manual, and unchecked in the repository for the usual reason: this section needs a running
COSMIC session, which neither this development environment nor CI has. The first box needs a
running systemd user manager as well, which a container does not have either — what is
checked of the unit here is that `systemd-analyze verify --user` accepts it with its
`ExecStart` pointing at a real binary, and that `just install` / `just uninstall` place and
remove it correctly against a scratch `$HOME`.

### 7. Automated

`cargo test --workspace`, covering:

- `clippo-core` — detection and masking against the fixture corpus
- `clippo-store` — dedup, retention, and pin-exemption against a temp DB
- `clippo-ipc` — the proxy and the interface over a real bus, which needs
  `dbus-run-session -- cargo test --workspace` (what `just test` and CI run)
- `clippo-cli` — argument parsing, id resolution, and the table, JSON and escaping against
  fixture `EntrySummary` values
- `clippo-applet` — the selection model and the key bindings, which are the two halves of the
  applet a compositor is *not* needed to test: where the highlight lands when the history
  changes underneath it, when a revealed value stops being answered, and which chord maps to
  which action. Drawing and the popup itself stay manual.
- `clippod` — the **self-echo loop**, which DESIGN.md's risk table asks for by name. `Copy`
  goes to a fake clipboard, what it was handed comes back as a capture, and the test asserts
  both directions: the copy-back adds no second entry, and copying the same content by hand
  afterwards still bumps the existing one.

The Wayland protocol and UI layers stay manual, and so does the CLI's live round trip against a
running daemon — that is verification 2 above. What the automated tests cover of the offer half
is everything either side of the protocol: the flavor list, the thumbnail exclusion, the
non-blocking write against a real pipe, and the guard.
