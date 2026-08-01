# clippo

A clipboard-history manager for the COSMIC desktop, written in Rust — with first-class
handling of the thing most clipboard managers get wrong: secrets. History is encrypted at
rest, and a suspected password or token is never rendered in full in the UI, while still
pasting the real value.

- [docs/DESIGN.md](docs/DESIGN.md) — architecture, components, and the decisions on record.
- [docs/ROADMAP.md](docs/ROADMAP.md) — build order (M0–M6) and how each stage is verified.

> **Status:** M0. The workspace, lint config, `justfile` and CI exist; every crate is still
> a placeholder. See the roadmap for what lands when.

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
