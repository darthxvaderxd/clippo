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
    └── icons/hicolor/scalable/apps/…
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
  hash. Without this, every copy-back re-enters the history.
- Runs on its own thread, talking to the daemon over `tokio::sync::mpsc`.
- Primary selection: config-gated, **off** by default.

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
   classes, Shannon entropy above 3.5 bits/char.

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
  | `Pin` | `(id, bool)` |
  | `Clear` | `(include_pinned)` |
  | `Reveal` | `(id) -> String` |
  | `SetPaused` / `Paused` | `(bool)` / `-> bool` |
  | `HistoryChanged` | signal, so the applet updates live |

- systemd user unit, `WantedBy=cosmic-session.target`, `Restart=on-failure`.
- `tracing` + `tracing-journald`; debuggable via `journalctl --user -u clippod -f`.

### `clippo-cli`

A thin `zbus` client over the same interface:
`clippo list|search|copy|pin|rm|clear|pause|reveal`.

Ship it before the GUI — it makes every layer below testable without touching a UI.

### `clippo-applet` — libcosmic panel applet

- `libcosmic` with the `applet` feature; `cosmic::applet::run`.
- Panel icon → popup with an auto-focused search field over a scrollable list. Keyboard-first:
  type to filter, `↑`/`↓` to move, `Enter` copy and close, `Delete` remove, `Ctrl+P` pin,
  `Ctrl+R` reveal a masked entry.
- Sensitive rows render the mask plus a small lock badge.
- Images render from the stored thumbnail flavor.
- `.desktop` carries `X-CosmicApplet=true` so it appears in COSMIC's panel configuration.
- **Global shortcut** (e.g. `Super+V`) — the applet registers `Toggle()` on D-Bus, and a custom
  COSMIC shortcut in `~/.config/cosmic/com.system76.CosmicSettings.Shortcuts` runs
  `clippo show`, which calls it. Whether libcosmic can open an applet popup programmatically
  needs verifying during implementation; **fallback** is a standalone floating picker window.
  The shortcut is manual setup for v1 — editing the user's shortcuts RON is out of scope.

## Known risks

| Risk | Mitigation |
|---|---|
| **Flatpak-proxied Wayland socket** hides data-control | Highest-probability source of lost time. Check `$WAYLAND_DISPLAY` whenever capture silently stops working. |
| **Applet popup may not be programmatically openable** in libcosmic | Design M5 so swapping to a standalone picker window is cheap. |
| **Self-echo loop** — a wrong hash guard re-enters every copy-back into history | Integration test at M3. |
| **Secret heuristics will need iteration** | Config escape hatch to disable the entropy rule while keeping the regex and MIME rules. |
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
