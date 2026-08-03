# clippo — Design

> Status: design only. No implementation yet. See [ROADMAP.md](ROADMAP.md) for the build order.

## Context

Pop!_OS 24.04 with the COSMIC desktop ships **no clipboard manager**. The host has every
`cosmic-applet-*` binary in `/usr/bin` except a clipboard one, and COSMIC has no built-in
history. The only existing option is [`cosmic-utils/clipboard-manager`][prior-art], which must
be built from source and whose architecture we don't control.

`clippo` fills that gap: a Rust clipboard-history manager built natively for COSMIC, with
first-class handling of the thing most clipboard managers get wrong — secrets. Passwords and
API tokens routinely pass through the clipboard, so clippo encrypts its history at rest and
**never renders a suspected secret in full** in the UI (only the first and last couple of
characters), while still pasting the real value.

[prior-art]: https://github.com/cosmic-utils/clipboard-manager

## Platform facts

Established by inspecting the target host, not assumed:

| Fact | Detail |
|---|---|
| Compositor | `cosmic-comp 0.1~1785355703~24.04`, smithay-based |
| Clipboard protocol | Advertises **both** `ext_data_control_v1` and `zwlr_data_control_v1` |
| Protocol gate | No `COSMIC_DATA_CONTROL_ENABLED` env gate in this build — available by default |
| Rust bindings | `wayland-protocols` `staging` feature → `ext::data_control`; `wayland-protocols-wlr` → `data_control` |
| Secret storage | `gnome-keyring` installed, `org.freedesktop.secrets` is D-Bus-activatable |
| Session target | `/usr/lib/systemd/user/cosmic-session.target` exists |
| Shortcut config | `~/.config/cosmic/com.system76.CosmicSettings.Shortcuts` (cosmic-config RON) |

## Environment constraints

Two things about this development setup will bite immediately.

### 1. RustRover runs in a Flatpak sandbox

Inside it, `WAYLAND_DISPLAY=wayland-1` resolves to `$XDG_RUNTIME_DIR/../../flatpak/wayland-1`
— Flatpak's proxied socket, which **filters out privileged protocols including data-control**.
`clippod` will find no clipboard-manager protocol when launched from RustRover's terminal or
run configurations.

**clippo must be built and run from a host terminal** (`cosmic-term`). This is not a bug in
clippo. When capture "silently stops working", check `$WAYLAND_DISPLAY` first — it must be
`wayland-0`.

### 2. No Rust toolchain on the host

Neither `/usr/bin/cargo` nor `~/.cargo/bin` exists; RustRover bundles a toolchain for its own
use only. Since the host is where clippo must actually run, step zero is:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Architecture

A **daemon + applet split**, chosen so history survives panel restarts and so a CLI and other
frontends are cheap to add.

```
                 ┌──────────────────────────────────────┐
   Wayland  ───▶ │ clippod  (systemd --user)            │
   data-control  │  ├─ clippo-wayland  watch + offer    │
                 │  ├─ clippo-core     detect/mask      │
                 │  └─ clippo-store    SQLCipher + blobs│
                 └──────────────┬───────────────────────┘
                                │ D-Bus  com.nilfactor.Clippo
                   ┌────────────┴────────────┐
                   ▼                         ▼
          clippo-applet (libcosmic)    clippo (CLI)
```

### Workspace layout

```
clippo/
├── Cargo.toml                  # workspace, shared deps, GPL-3.0
├── justfile                    # build / install / uninstall / dev
├── docs/
│   ├── DESIGN.md               # this file
│   └── ROADMAP.md
├── crates/
│   ├── clippo-core/            # Entry/Flavor types, secret detection + masking, config
│   ├── clippo-store/           # encrypted SQLite, blob handling, retention, dedup
│   ├── clippo-wayland/         # data-control client: watch selections + serve offers
│   ├── clippo-ipc/             # zbus proxy/interface definitions shared by all 3 binaries
│   ├── clippod/                # daemon binary
│   ├── clippo-cli/             # `clippo` CLI binary
│   └── clippo-applet/          # `clippo-applet` libcosmic panel applet
└── res/
    ├── com.nilfactor.Clippo.desktop        # applet entry, X-CosmicApplet=true
    ├── com.nilfactor.Clippo.metainfo.xml
    ├── clippod.service                     # WantedBy=cosmic-session.target
    └── icons/hicolor/
        ├── scalable/apps/com.nilfactor.Clippo.svg
        └── symbolic/apps/com.nilfactor.Clippo-symbolic.svg   # what the panel draws
```

## Components

### `clippo-wayland` — the hard part

A hand-rolled data-control client on `wayland-client` + `calloop`.

We deliberately do **not** use `wl-clipboard-rs`: its paste API fetches one MIME type per call
and its copy API is process-shaped, whereas clippo needs to capture *all* flavors of one
selection atomically and re-offer all of them from a long-lived process.

- **Bind** — prefer `ext_data_control_manager_v1`; fall back to `zwlr_data_control_manager_v1`
  if absent. Both sit behind one internal `DataControl` trait so the rest of the crate is
  protocol-agnostic. If neither binds, fail with an error naming both protocols.
- **Watch** — get a device for the seat, handle `data_offer` → collect advertised MIME types,
  then on `selection` receive each *interesting* flavor through a pipe. Interesting =
  `text/plain;charset=utf-8`, `text/plain`, `text/html`, `text/uri-list`, `image/png`,
  `image/jpeg`, plus the `x-kde-passwordManagerHint` marker. Reads are non-blocking inside the
  calloop loop, with a per-flavor size cap.
- **Offer** — own an `ext_data_control_source_v1`, advertise every stored flavor of the chosen
  entry, and write the blob to the fd on each `send` event. The daemon staying alive is what
  keeps the clipboard populated; this is standard Wayland behavior and must be documented.
- **Self-echo guard** — hash what we set, then ignore the next `selection` event matching that
  hash. Without this, every copy-back re-enters the history. *Implemented in `clippod` rather
  than here (M3c): the hash it matches on is `clippo-store`'s dedup hash — the exact identity a
  capture would be stored under — and this crate would have to depend on the encrypted store to
  compute it.*
- Runs on its own thread, talking to the daemon over `tokio::sync::mpsc`.
- Primary selection: config-gated, **off** by default.
- **Press** (M7) — a `zwp_virtual_keyboard_v1` keyboard, on its own connection, used by `Paste`
  to synthesise the user's paste shortcut into whatever holds keyboard focus. Wayland gives a
  client no way to put text into another application's surface, so this is the only route from
  "on the clipboard" to "in the document", and it is a privileged protocol for the obvious
  reason. It uploads its own `us` keymap, which is what makes `Ctrl+V` mean `Ctrl+V` whatever
  the user types on. Optional: a compositor without it leaves `Paste` equal to `Copy`.

### `clippo-store` — encrypted at rest

- `rusqlite` with the `bundled-sqlcipher-vendored-openssl` feature. Whole-database encryption,
  transparent to queries — no per-column crypto to get wrong.
- **Key management** — a 32-byte random key in the Secret Service via the `oo7` crate (pure
  Rust; gnome-keyring is present and D-Bus-activatable on the host). Fallback when no Secret
  Service is reachable: `~/.local/share/clippo/key`, mode `0600`, with a logged warning. The
  key is never derived from anything guessable.
- **Schema**
  - `entries(id, created_at, last_used_at, kind, preview, hash UNIQUE, pinned, sensitive)`
  - `flavors(entry_id, mime, data BLOB)` — one row per MIME flavor of the same copy
- **Dedup** — BLAKE3 over the canonical flavor. A repeat copy bumps `last_used_at` rather than
  inserting a duplicate.
- **Images** — stored as a flavor blob under a configurable cap (default 8 MB). A PNG thumbnail
  is generated at capture with the `image` crate and stored as a separate
  `image/png;clippo-thumb` flavor, so the applet never decodes full-size images.
- **Retention** — max entries (default 500) and max age (default 30 days). **Pinned entries are
  exempt from both**, and from `Clear()` unless explicitly included.
- **Search** — no FTS5. The daemon keeps previews in memory and matches with `nucleo-matcher`
  (the fuzzy matcher COSMIC's own launcher uses). Better UX than `LIKE`, and it sidesteps
  building FTS5 against SQLCipher.

### `clippo-core` — secret detection and masking

The differentiating feature. It gets its own module and its own test suite.

**Detection** produces a `sensitive: bool` at capture time from three signals:

1. *MIME hints* — presence of `x-kde-passwordManagerHint`, which KeePassXC and Bitwarden set.
   Note that clippo **masks rather than skips** these: the hint feeds detection, it does not
   discard the entry.
2. *Shape regexes* — `sk-…`, `ghp_`/`gho_`/`github_pat_`, `AKIA…`, `xox[baprs]-`, JWT (`eyJ`
   plus two dot-separated segments), `-----BEGIN … PRIVATE KEY-----`,
   `postgres://user:pass@`.
3. *Entropy heuristic* — single token, no whitespace, length 8–128, at least three character
   classes, Shannon entropy above 3.5 bits/char. *Two refinements came out of tuning against
   the corpus (M4), both in `clippo_core::secrets::entropy`: `-` and `_` are separators rather
   than a character class, without which every UUID is flagged; and a value that is a URL or a
   filesystem path is exempt, without which every link is. Both are documented where they are
   implemented, with the fixture that motivated them.*

**Masking is display-only.** `mask(s)` → `ab••••••••yz`: first two and last two characters
(both configurable), middle replaced by a fixed-width bullet run that does **not** leak the
true length. The full value never crosses D-Bus for list or search responses — only
`Reveal(id)` returns it, on explicit user action, and the applet never caches that result.
Copying a masked entry pastes the real value.

The entropy threshold must be tuned against a fixture corpus of real-world strings (git SHAs,
base64 blobs, UUIDs, minified JS, prose). False positives here are annoying; false negatives
are the actual risk.

### `clippod` — daemon

- Tokio runtime; owns the store; spawns the wayland thread.
- `zbus` service `com.nilfactor.Clippo` at `/com/nilfactor/Clippo` on the session bus:

  | Member | Signature |
  |---|---|
  | `List` | `(limit, offset) -> Vec<EntrySummary>` — previews already masked |
  | `Search` | `(query, limit) -> Vec<EntrySummary>` |
  | `Copy` / `Delete` | `(id)` |
  | `Paste` | `(id) -> bool` — `Copy`, then the user's `paste_shortcut` synthesised into whatever has focus; the answer is whether a key was pressed (M7) |
  | `Pin` | `(id, bool)` |
  | `Clear` | `(include_pinned)` |
  | `Reveal` | `(id) -> String` |
  | `Thumbnail` | `(id) -> Vec<u8>` — the stored `image/png;clippo-thumb` flavor (M5) |
  | `SetPaused` / `Paused` | `(bool)` / `-> bool` |
  | `HistoryChanged` | signal, so the applet updates live |

- systemd user unit, `WantedBy=cosmic-session.target`, `Restart=on-failure`.
- `tracing` + `tracing-journald`; debuggable via `journalctl --user -u clippod -f`.

### `clippo-cli`

A thin `zbus` client over the same interface:
`clippo list|search|copy|paste|pin|rm|clear|pause|reveal`, plus `clippo show` from M5, which is
the only one that calls the applet rather than the daemon.

Ship it before the GUI — it makes every layer below testable without touching a UI.

### `clippo-applet` — libcosmic panel applet

- `libcosmic` with the `applet` feature; `cosmic::applet::run`.
- Panel icon → picker with an auto-focused search field over a scrollable list. Keyboard-first:
  type to filter, `↑`/`↓` to move, `Enter` copy and close, `Delete` remove, `Ctrl+P` pin,
  `Ctrl+R` reveal a masked entry.
- Sensitive rows render the mask plus a small lock badge.
- Images render from the stored thumbnail flavor.
- `.desktop` carries `X-CosmicApplet=true` so it appears in COSMIC's panel configuration.
- **Global shortcut** (e.g. `Super+V`) — the applet registers `Toggle()` on D-Bus under its own
  bus name `com.nilfactor.ClippoApplet`, and a custom COSMIC shortcut in
  `~/.config/cosmic/com.system76.CosmicSettings.Shortcuts` runs `clippo show`, which calls it.
  libcosmic *can* open an applet surface programmatically on the pinned revision. It cannot
  give an `xdg_popup` keyboard focus without an input serial, which a global shortcut does not
  supply, so **the picker is a layer surface** — see the decisions section. The shortcut is manual setup for v1 —
  editing the user's shortcuts RON is out of scope, but the README carries the exact snippet.

## Known risks

| Risk | Mitigation |
|---|---|
| **Flatpak-proxied Wayland socket** hides data-control | Highest-probability source of lost time. Check `$WAYLAND_DISPLAY` whenever capture silently stops working. |
| **Applet popup may not be programmatically openable** in libcosmic | Design M5 so swapping to a standalone picker window is cheap. *Materialised, in the keyboard-focus form:* the popup opened but took no keys. The mitigation paid — see the decisions section. |
| **Self-echo loop** — a wrong hash guard re-enters every copy-back into history | Integration test at M3. |
| **Secret heuristics will need iteration** | Config escape hatch to disable the entropy rule while keeping the regex and MIME rules. |
| **Synthesised paste goes to whichever window has focus**, which nothing can address or predict | The applet closes the picker first and the daemon waits before pressing. Racy by construction — there is no event for "focus has settled" — so the wait is a constant, and being early means pasting into the picker. |
| **Synthesising input is a capability not everyone wants clippo to have** | `auto_paste`, on by default, off in one line. Off means the virtual keyboard is never created at all, so it is not a check that a later bug can skip. `Paste` is then exactly `Copy`. |
| **One `paste_shortcut` for every application**, and applications disagree | Config key, defaulting to `Ctrl+V`. Terminals mostly want `Ctrl+Shift+V`. `Paste` copies first regardless, so a wrong shortcut degrades to a manual paste rather than to nothing. |
| **Daemon owns the selection** — if `clippod` dies, the clipboard empties | Expected Wayland behavior. `Restart=on-failure` mitigates; document in the README. |
| **SQLCipher vendored build** adds noticeable first-compile cost | Accepted. The alternative — hand-rolled per-blob crypto — is worse. |

## Decisions on record

Choices made during planning that a future reader might otherwise re-litigate:

- **Built fresh rather than forking `cosmic-utils/clipboard-manager`** — full control over
  architecture and UX. Ideas borrowed, no code.
- **Daemon + applet, not a single applet binary** — history survives panel restarts, and the
  CLI and any future frontends come essentially free.
- **Masking, not skipping, for suspected secrets** — a skipped password is a silently missing
  clipboard entry, which is worse UX than a masked one that still pastes correctly.
- **Whole-DB encryption over per-field** — one place to get crypto right instead of many.
- **Fuzzy in-memory search over FTS5** — better matching, and avoids the FTS5-on-SQLCipher
  build.
- **The picker is a layer surface, not an `xdg_popup`** (M5, revised after
  [verification 6](ROADMAP.md#6-restart-resilience)). It started as a popup: reading the pinned
  libcosmic revision settled that `get_popup` is an ordinary `Task` returned from `update`, so
  the message that opens the picker can come from the D-Bus subscription just as well as from a
  click, and that a missing input serial is not fatal — `xdg_popup::grab` needs a serial from a
  recent input event, which a picker opened by `clippo show` has none of, and iced looks that
  serial up with `and_then` and skips the grab rather than refusing the popup. What reading the
  source could not settle was the *consequence*: without a grab the compositor is under no
  obligation to give the popup keyboard focus. Run on a real session, it does not. The picker
  came up with a focus ring and a blinking caret — `text_input::focus` sets the widget's own
  focus state and the caret blinks off that alone — and no keystroke reached it until the
  surface was clicked in. There is no second route to focus on a popup: `window::Action::GainFocus`
  is winit's `focus_window`, a no-op on Wayland, and `SctkPopupSettings` has no
  keyboard-interactivity field. `zwlr_layer_shell_v1` does, so the picker is now a layer surface
  with `KeyboardInteractivity::Exclusive`, which asks for focus on map with no serial and no
  click. An applet may do this despite being a `cosmic-panel` client rather than a direct one:
  the panel's embedded compositor advertises the layer-shell global to its applets and proxies
  their surfaces out to `cosmic-comp`, interactivity included. The mitigation held — the
  `surface` module was the only file that knew what kind of surface the picker was, and the swap
  cost that file plus three event arms in `app.rs`, because a layer surface is not dismissed by
  a click outside it the way a grabbed popup was.

  What the swap then cost was the size. A layer surface cannot be fitted to its contents the way
  a popup was, and — the part that took the longest to find — asking `cosmic-panel` for a
  *fully-specified* size means the surface is never drawn at all. The panel forwards the request
  to `cosmic-comp` and forwards the reply back only when the two differ, so a size granted
  verbatim produces no configure, and iced waits for a configure before it renders. The surface
  maps far enough to take the exclusive keyboard focus and never far enough to show anything:
  an invisible picker that swallows every keystroke, silent in every log. So the surface asks for
  no size at all — anchored to all four edges, both axes left to the compositor — and comes back
  as the whole output, which is both what makes the reply differ from the request and what gives
  `Picker::content` somewhere to centre the picker. This is written against a `cosmic-panel` bug
  rather than against the protocol, and `surface.rs` says so at the point that depends on it.

  Position followed from the same constraint. A popup hung under the applet's icon; a layer
  surface is placed by anchors and margins, which can say "this edge, that far in" but cannot say
  "the middle". A full-output surface can, because the middle is then an ordinary layout inside
  it — so the picker is centred on screen, where COSMIC's own keyboard-opened surfaces sit.

  `Context::popup_container` could not be used for the frame, which is why `surface.rs` builds
  its own with the same styling. It returns an `Autosize`, whose purpose is to resize the surface
  to its contents: correct for a popup, fatal here, because it shrinks the layer surface back down
  and a surface smaller than its anchors is dropped in a corner — no amount of wrapping outside it
  can centre what it has already collapsed. It also clamps the width to exactly 360, so the
  picker's own `WIDTH` never applied while it was in use.
- **`Thumbnail(id)` added to the daemon's interface** (M5), which the member table above did not
  originally list. Its absence was a gap rather than a decision: capture derives and stores a
  thumbnail expressly "so the applet never decodes full-size images", and with no member to
  fetch one the applet could only have reached it through the full-size blob — the thing the
  thumbnail exists to avoid. It returns derived bytes rather than stored content, so it does
  not widen what `Reveal` is the sole route for. The member reads the thumbnail flavor on its
  own rather than through `Store::get`, which returns *every* flavor: going through `get` would
  avoid the full-size decode and still read and decrypt the full-size PNG in order to hand back
  the small one beside it, which is the cost the derived thumbnail exists to remove.
- **`Paste(id)` added to the daemon's interface** (M7). `Copy` puts an entry on the clipboard,
  which is only ever half of what a user picking a row wanted: the other half is it appearing
  where their cursor is, and they were doing that half themselves. Wayland offers a client no
  way to write into another application's surface, by design, so the only route is to
  synthesise the keystroke they would have pressed — `zwp_virtual_keyboard_v1`, which
  `cosmic-comp` advertises and honours.

  It is a daemon member and not an applet capability because it could not be an applet one:
  `cosmic-panel`'s embedded compositor does not advertise the virtual-keyboard global to the
  applets it hosts, so the applet cannot bind it at all. `clippod` already holds a direct
  connection to `cosmic-comp` for data-control, which is where the keyboard goes too.

  Three things were settled by trying them on a real session rather than by reading:

  - **Announcing the modifier is not enough.** `zwp_virtual_keyboard_v1` has a `modifiers`
    request, and sending only that produces an unmodified `v`: the compositor recomputes
    modifier state from the keys it believes are held, so the announcement is overwritten by
    the very next keypress. The modifier has to be pressed as a key. Both are sent.
  - **The keymap is clippo's, not the user's.** The compositor reads our keycodes against the
    keymap we upload, so a compiled `us` layout makes `Ctrl+V` arrive as `Ctrl+V` on any
    physical layout. Uploading nothing is not an option — the protocol refuses keys without a
    keymap.
  - **Focus cannot be addressed or waited for.** The keystroke lands wherever focus is, and
    the picker is still on screen when `Paste` arrives, because the applet cannot send the
    request after destroying the surface it would be sending from. So the applet closes and
    the daemon waits a fixed 120 ms before pressing. There is no event on this side of the bus
    that says focus has returned; `clippod` has no surface and sees none of it.

  `Paste` fails only where `Copy` fails. A keystroke that was not sent — `auto_paste` off, no
  protocol, no seat, a compositor that took it away mid-run — comes back as `Paste`'s `bool`
  rather than as an error, because the entry really is on the clipboard and the user can finish
  by hand. Reporting an error would have a frontend say the whole thing failed when the half
  that matters succeeded. The `bool` is what lets `clippo paste` say which of the two happened
  instead of claiming a paste it did not make; the applet ignores it, having closed already and
  having nowhere to report it.

  **`auto_paste` is a capability switch, not a preference**, which is why it is one setting for
  every caller rather than "what `Enter` does". A user turning it off is saying clippo must not
  type into their windows, and `clippo paste` continuing to do so would make that false. With it
  off `clippod` never creates the virtual keyboard at all — the means, not just the intent, is
  absent, so no later change to `Paste` can press a key by accident. The knob is checked *after*
  the copy for the same reason: turning it off must change exactly one thing.
