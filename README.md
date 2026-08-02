# clippo

A clipboard-history manager for the COSMIC desktop, written in Rust — with first-class
handling of the thing most clipboard managers get wrong: secrets. History is encrypted at
rest, and a suspected password or token is never rendered in full in the UI, while still
pasting the real value.

- [docs/DESIGN.md](docs/DESIGN.md) — architecture, components, and the decisions on record.
- [docs/ROADMAP.md](docs/ROADMAP.md) — build order (M0–M6) and how each stage is verified.

> **Status:** M1. The data-control client captures selections and `clippo-watch` prints them;
> storage, the daemon, the CLI and the applet are still placeholders. See the roadmap for what
> lands when.

## ⚠️ Build and run from a host terminal, not RustRover's Flatpak

RustRover runs in a Flatpak sandbox where `WAYLAND_DISPLAY=wayland-1` resolves to Flatpak's
proxied socket, which **filters out privileged protocols including data-control**. Launched
from RustRover's terminal or run configurations, `clippod` will find no clipboard-manager
protocol at all.

Use a host terminal (`cosmic-term`), and check the socket first — it must print `wayland-0`:

```sh
echo $WAYLAND_DISPLAY
```

Whenever capture "silently stops working", check this before anything else. Details in
[DESIGN.md](docs/DESIGN.md), "Environment constraints".

## Building

The host needs a Rust toolchain (stable); RustRover's bundled one is for its own use only:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Then, from the repo root:

```sh
just build     # cargo build --workspace
just test      # cargo test --workspace
just check     # fmt + clippy + test, exactly what CI runs
```

`just` itself is optional — every recipe is a one-line `cargo` invocation you can run
directly. `just install` / `just uninstall` are stubs until M6 (packaging).

## Debugging capture

`clippo-watch` prints every clipboard selection the compositor hands us and every flavor of it.
It is the fastest way to answer "why did clippo not pick that up?", and the only way to see the
data-control protocol working, since no CI runner has a compositor to test against.

```sh
cargo run -p clippo-wayland --bin clippo-watch     # or: just watch
```

Copy something, and each selection prints a block:

```
clippo-watch: bound ext_data_control_v1 on WAYLAND_DISPLAY=wayland-0
              per-flavor cap 8388608 B, primary capture off. Copy something; Ctrl-C to stop.

── selection #1  clipboard  (ext_data_control_v1)
   advertised: text/plain;charset=utf-8, text/html, TIMESTAMP, TARGETS
   fetched:
     text/plain;charset=utf-8        22 B  "hello from cosmic-term"
     text/html                       51 B  "<meta charset=\"utf-8\"><p>hello from cosmic-term</p>"
   skipped (uninteresting): TIMESTAMP, TARGETS

── selection #2  clipboard  (ext_data_control_v1)
   advertised: image/png, TIMESTAMP
   fetched:
     image/png    184320 B  <binary, 184320 bytes, not printed>
   skipped (uninteresting): TIMESTAMP
```

A copy that got nothing back still prints, because "the clipboard changed and clippo kept none
of it" is the case you most need to see:

```
── selection #3  clipboard  (ext_data_control_v1)
   advertised: image/png, TIMESTAMP
   fetched: <nothing>
   dropped:
     image/png  exceeded the 1024 byte per-flavor cap
   skipped (uninteresting): TIMESTAMP
```

Reading a block:

- **advertised** is everything the source offered. **fetched** is the subset clippo wants (the
  list in DESIGN.md), each with its size and — for text — an escaped preview with newlines
  shown as `\n`. Binary flavors report their size and are never written to the terminal.
- **dropped** lists flavors clippo asked for and did not get, with the reason. A flavor over
  the size cap says so and names the cap; it is never silently missing.
- A **`⚠ x-kde-passwordManagerHint present`** line means the source tagged the copy as a
  credential. That flavor's *presence* is the signal, so it gets called out rather than left to
  be spotted in the advertised list. The warning is all it is — the flavors themselves are
  still printed in full, marker included.

Useful flags:

| Flag | Why |
|---|---|
| `--max-bytes 1024` | Exercise the drop path — copy a screenshot and watch `image/png` get rejected by the cap. |
| `--primary` | Also watch the middle-click primary selection, which is off by default. |
| `--preview 200` | Longer text previews. |

`RUST_LOG=clippo_wayland=trace` adds the library's own tracing on stderr; the blocks stay on
stdout, so `clippo-watch > capture.log` keeps them separate.

If it exits saying no data-control manager bound, check `$WAYLAND_DISPLAY` — that is the
Flatpak socket problem above, and the error prints the value it actually saw.

**Nothing is redacted.** Previews are real clipboard contents — passwords and tokens included,
since masking them would hide exactly what this tool exists to show. Anything you copy while it
is running ends up in your scrollback, and in `capture.log` if you redirect it. `--help` says
the same thing.

## Layout

| Crate | Role |
|---|---|
| `clippo-core` | Entry/Flavor types, config, secret detection + masking |
| `clippo-store` | SQLCipher-backed history, blobs, dedup, retention |
| `clippo-wayland` | data-control client, plus the `clippo-watch` debug binary |
| `clippo-ipc` | shared zbus proxy/interface definitions |
| `clippod` | the daemon |
| `clippo-cli` | the `clippo` CLI |
| `clippo-applet` | libcosmic panel applet |

## License

GPL-3.0-only. See [LICENSE](LICENSE).
