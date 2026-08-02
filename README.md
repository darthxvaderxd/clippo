# clippo

A clipboard-history manager for the COSMIC desktop, written in Rust — with first-class
handling of the thing most clipboard managers get wrong: secrets. History is encrypted at
rest, and a suspected password or token is never rendered in full in the UI, while still
pasting the real value.

- [docs/DESIGN.md](docs/DESIGN.md) — architecture, components, and the decisions on record.
- [docs/ROADMAP.md](docs/ROADMAP.md) — build order (M0–M6) and how each stage is verified.

> **Status:** M3 complete. Capture, encrypted storage, the daemon, the `clippo` CLI and the
> copy-back path are in: `clippod` records every copy, serves `com.nilfactor.Clippo` on the
> session bus, and `clippo copy <id>` puts an entry back on the clipboard for any application
> to paste. Still to come — masking of suspected secrets, and the applet. See the roadmap for
> what lands when.

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

## Running the daemon

```sh
just run-daemon        # cargo run -p clippod, with debug logging
```

From a host terminal, for the reason above. It reads the config, opens the encrypted history,
takes the name `com.nilfactor.Clippo` on the **session** bus and starts recording. A second
`clippod` exits non-zero saying the name is taken rather than running as a silent duplicate.

Talk to it with the `clippo` CLI below, or poke the interface directly with `busctl`:

```sh
busctl --user introspect com.nilfactor.Clippo /com/nilfactor/Clippo com.nilfactor.Clippo
```

Logging is `tracing`. Under systemd it goes to the journal with real priorities
(`journalctl --user -u clippod -f`); run by hand it goes to stderr. `CLIPPO_LOG` sets the
verbosity, falling back to `RUST_LOG`, defaulting to `info`:

```sh
CLIPPO_LOG=clippod=debug,clippo_wayland=debug just run-daemon
```

### Killing the daemon empties the clipboard

After `clippo copy 2`, **`clippod` is the clipboard.** Wayland has no clipboard of its own that
outlives whoever put something on it: the daemon owns the selection, and the bytes are written
out of its memory down a pipe every time an application pastes. So if `clippod` stops — you
Ctrl-C it, `systemctl --user stop clippod`, it crashes — whatever it had put there is simply
gone, and the next Ctrl+V pastes nothing.

This is standard Wayland behaviour and every clipboard manager on it works the same way. It is
not a clippo bug and there is no fix, only a mitigation: the systemd unit at M6 carries
`Restart=on-failure`, so a crash costs one clipboard's worth of content rather than the rest of
the session. Anything copied *by another application* is unaffected — that application owns its
own selection, and clippo's history of it is on disk either way.

The corollary is that clippo hears its own copy-back: taking the selection makes the compositor
announce it to every clipboard manager, clippo included. A guard keyed on the entry's hash
keeps that one capture out of the history, so `clippo copy 2` moves entry 2 to the front once
rather than twice — and copying that same text again by hand afterwards still registers
normally.

## The CLI

`clippo` is the client, and every subcommand is one call to the running daemon — it never
opens the history database itself. With `clippod` running:

```sh
cargo run -p clippo-cli -- list      # or `just run-cli list`, or the built ./target/debug/clippo
```

| Command | What it does |
|---|---|
| `clippo list [-n N] [--offset N] [--json]` | The history, most recently used first. Defaults to 20 entries; `-n 0` is all of them. |
| `clippo search QUERY [-n N] [--json]` | Fuzzy-match the previews, best match first. |
| `clippo copy ID` | Put that entry back on the clipboard, and move it to the front of the history. Needs `clippod` to stay running — see above. |
| `clippo pin ID` / `clippo pin ID --off` | Pin or unpin. A pinned entry is exempt from retention and from `clear`. |
| `clippo rm ID...` | Delete entries, pinned or not. |
| `clippo clear [--yes] [--include-pinned]` | Delete the whole history. Asks first. |
| `clippo pause on` / `off` / *(nothing)* | Stop recording, resume recording, or print `paused` / `recording`. |
| `clippo reveal ID` | Print an entry's whole value to stdout. |

`clippo <command> --help` has the detail. Two conventions worth knowing:

- **stdout is data, stderr is talk.** The table, `--json` and a revealed value go to stdout;
  confirmations and errors go to stderr. `clippo reveal 2 > key.pem` writes the value and
  nothing else, and a failure is still visible in the terminal.
- **Errors exit non-zero.** Including a `clear` you answered `n` to.

### Naming an entry

The `ID` column is a number you can shorten. An id that exists is always itself — `12` is entry
12 even when 120 and 127 exist — and otherwise any unambiguous *start* of an id works, so
`clippo copy 14` finds entry 142. A prefix that could mean several entries is refused with the
list of candidates rather than guessing:

```
$ clippo copy 1
clippo: `1` could mean any of these 3 entries — type more of the id:
   12  hunter2 second line
  120  a long preview that goes on and on and …
  127  image/png, 2.0 KB
```

`clippo rm` takes several ids at once. They are all resolved before anything is deleted, so one
bad reference fails the command rather than half of it, and two references to the same entry —
`clippo rm 1 12` where `1` can only mean 12 — delete it once.

### Reading a list

```
$ clippo list
 ID  AGE  KIND   FL  PREVIEW
  3   6s  text   ..  hello from cosmic-term
 12   1m  text   .s  hunter2 second line
120   3h  html   p.  a long preview that goes on and on and on and on and on and on …
127   4d  image  ..  image/png, 2.0 KB
```

`AGE` is since the entry was last used. `FL` is two flags: `p` when the entry is pinned, `s`
when clippo suspects a password or a token. The preview is cut to fit the column — `--json`
carries the daemon's whole preview, its timestamps as Unix milliseconds, and every other field
under the same name, which is what to script against.

### Terminal safety

A preview is whatever somebody copied, which includes escape sequences and right-to-left
overrides — printed raw, those repaint a terminal or reorder a row so that it shows one entry's
text next to another's id. So the table is flattened to one line and escaped first:
`\u{1b}[31m` is shown, not obeyed.

`--json` keeps the daemon's whole preview, newlines and all, because a script wants the value
rather than a column's rendering of it — but it is still safe to look at. The characters that
matter come out as `\uXXXX`, which is JSON's own spelling for them, so what a script decodes is
exactly what was stored and only the display changes.

`clippo reveal` is the deliberate exception. It exists to be redirected or piped, and a value
that came back altered would be the wrong value — so it prints exactly what was stored, with no
trailing newline added. Its `--help` says so. Redirect it when you do not know what the entry
holds.

### When the daemon is not running

Every subcommand fails with the same line rather than a raw D-Bus error:

```
$ clippo list
clippo: clippod is not running — nothing owns com.nilfactor.Clippo on the session bus. Start it
with `systemctl --user start clippod`, or run it in a host terminal with `cargo run -p clippod`
```

(The systemd unit itself arrives with packaging in M6; until then, `just run-daemon`.)

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
