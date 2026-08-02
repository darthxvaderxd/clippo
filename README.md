# clippo

A clipboard-history manager for the COSMIC desktop, written in Rust — with first-class
handling of the thing most clipboard managers get wrong: secrets. History is encrypted at
rest, and a suspected password or token is never rendered in full in the UI, while still
pasting the real value.

- [docs/DESIGN.md](docs/DESIGN.md) — architecture, components, and the decisions on record.
- [docs/ROADMAP.md](docs/ROADMAP.md) — build order (M0–M6) and how each stage is verified.

> **Status:** M5 complete. Capture, encrypted storage, the daemon, the `clippo` CLI, the
> copy-back path, secret masking and the panel applet are in: `clippod` records every copy,
> serves `com.nilfactor.Clippo` on the session bus, `clippo copy <id>` puts an entry back on
> the clipboard for any application to paste, a suspected password or token shows as
> `ab••••••••yz` rather than in full — see [Secrets](#secrets) — and
> [the applet](#the-applet) is a keyboard-driven picker in the COSMIC panel. Still to come —
> packaging: the systemd unit, the `.desktop` file and `just install`. See the roadmap for
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
| `clippo show` | Open the panel applet's picker, or close it if it is already open. The one subcommand that talks to the applet rather than to `clippod` — see [The applet](#the-applet). |

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
   12  hu••••••••ne
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
 12   1m  text   .s  hu••••••••ne
120   3h  html   p.  a long preview that goes on and on and on and on and on and on …
127   4d  image  ..  image/png, 2.0 KB
```

`AGE` is since the entry was last used. `FL` is two flags: `p` when the entry is pinned, `s`
when clippo suspects a password or a token. Entry 12 above is one — its preview is a mask, and
[Secrets](#secrets) is what that means. The preview is cut to fit the column — `--json`
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

## The applet

A panel icon that opens a picker over the same history the CLI reads. It owns no clipboard
state of its own — every row came from the daemon's `Search`, and every action it takes is a
call on the same D-Bus members `clippo` uses, so there is no second code path to disagree with
the first.

```sh
cargo run -p clippo-applet          # or `just run-applet`
```

Normally `cosmic-panel` starts it, and adding it to the panel is COSMIC Settings → Desktop →
Panel → Configure panel applets. The `.desktop` file that puts clippo in that list arrives
with packaging in M6; until then, run it by hand from a host terminal.

### Keyboard

The picker is keyboard-first: the search field takes focus the moment it opens, so you can
start typing immediately, and nothing below needs the mouse.

| Key | What it does |
|---|---|
| *(type)* | Filter. This is the daemon's `Search`, so the applet and `clippo search` rank a query identically. |
| `↑` / `↓` | Move the highlight. It stops at the ends rather than wrapping. |
| `Enter` | Copy the highlighted entry and close. The picker gets out of the way because you are about to paste. |
| `Delete` | Remove the highlighted entry. No confirmation — this is one row of a rolling history, and `clippo clear` is the destructive one. |
| `Ctrl+P` | Pin or unpin. A pinned entry is exempt from retention and from `clear`. |
| `Ctrl+R` | Show the highlighted entry's stored value in place, up to a bound. This is how a masked row is read — see below. |
| `Escape` | Close without doing anything. |

The highlight follows the *entry*, not the row number. That matters because the list changes
while you are looking at it: a copy made in another window arrives at the top and shifts every
row below it, and an index-based highlight would quietly move to a different entry between you
reading a row and pressing `Enter`.

### Masked rows, and revealing one

A row the daemon flagged as sensitive shows `ab••••••••yz` and a small lock badge. The badge is
the part that makes the mask legible — bullets on their own could be a value that genuinely
looks like that.

`Ctrl+R` calls `Reveal(id)` for that one row and shows the answer in place. It is **never
cached**: the applet answers with it only while that same row is still highlighted, so moving
the selection stops it being drawn, and it is dropped when the popup closes — whether you
closed it or clicked away. The value is held zeroized from the moment it arrives, so dropping
it wipes the memory rather than leaving the secret in a freed buffer for a core dump to pick
up — with one honest gap: the buffer zbus deserialises the reply into is not clippo's to wipe.

The revealed value is wrapped rather than cut at the width of the list, but it is bounded — the
first 2 000 characters over at most 12 lines, with an `…` when there was more. That is far more
than any value the mask exists for: nothing longer than 128 characters is flagged on entropy in
the first place. The cap is there because `Reveal` returns the whole stored value, the binding
works on any row rather than only a masked one, and a megabyte of pasted text would otherwise
draw a row hundreds of screens tall. Use `clippo reveal <id>` to see a long value whole.

Copying a masked row still copies the **real** value. Masking is display-only everywhere in
clippo; see [Secrets](#secrets).

### Live updates, and when the daemon is not running

The applet subscribes to the daemon's `HistoryChanged` and never polls. Copy something in
another window with the picker open and it appears at the top; `clippo rm <id>` in a terminal
removes that row from under you.

With the picker *closed* the applet listens and does nothing else. The signal fires on every
copy you make all day, and re-reading a list nobody is looking at would cost the daemon a
ranked search — and a thumbnail fetch for every screenshot — for no visible benefit. Opening
the picker refreshes it, so what you see is always current.

If `clippod` is not running the picker says so, with the command to start it. It deliberately
does *not* show an empty list, which would read as "your clipboard history is gone". It
reconnects on its own when the daemon comes back — a restarted `clippod` is answered again
with nothing reopened and no keypress.

### Global shortcut

clippo does not register a global shortcut for itself, because COSMIC owns those. Binding one
is a one-time manual step, and it runs `clippo show` — which asks the applet to open its
picker, or to close it if it is already open.

The supported route is the GUI: **COSMIC Settings → Keyboard → Shortcuts → Custom shortcuts**,
add a shortcut with the command `clippo show` and the key `Super+V`.

To do it by hand instead, the shortcuts config is a cosmic-config directory. Add the binding to

```
~/.config/cosmic/com.system76.CosmicSettings.Shortcuts/v1/custom
```

which is RON — a map from a binding to an action:

```ron
{
    (
        modifiers: [Super],
        key: "v",
        description: Some("clippo — clipboard history"),
    ): Spawn("clippo show"),
}
```

If the file already exists it already has that outer `{ … }`, so add the one entry inside it
rather than replacing the file — every custom shortcut you have lives in there. The change is
picked up without a restart. `clippo` has to be on `PATH` for `Spawn` to find it; before `just
install` exists (M6), use the absolute path to the built binary.

Two things worth knowing about this path:

- `clippo show` needs the **applet**, not just the daemon. A running `clippod` is not enough,
  and the error says so and names the panel setting.
- A picker opened by a global shortcut has no input serial to hand the compositor, so it gets
  no `xdg_popup` grab. Whether it takes keyboard focus anyway is cosmic-comp's call — it is
  the one part of this that could not be settled by reading libcosmic, and it is on the
  [verification list](docs/ROADMAP.md#6-restart-resilience).

## Secrets

The feature clippo exists for. A clipboard history is a list of the last five hundred things
you copied, and some of them were passwords — so clippo **never renders a suspected secret in
full**. It masks it instead, and masking changes nothing but the display.

### What gets flagged

Three rules, checked in this order, at the moment a copy is captured. The rule that fired is in
the journal (`journalctl --user -u clippod`) at debug level, by name, so "why is that masked?"
has an answer that is not a guess:

| Rule | Fires on | Notes |
|---|---|---|
| `mime-hint` | The `x-kde-passwordManagerHint` flavor | KeePassXC and Bitwarden set it. The application is telling us; nothing is inferred. |
| `shape:<name>` | `sk-…`, `ghp_`/`gho_`/`github_pat_`, `AKIA…`, `xox[baprs]-`, a JWT, a `-----BEGIN … PRIVATE KEY-----` block, `postgres://user:pass@…` | Prefix matches. Adding a provider is one regex. |
| `entropy` | A single token, 8–128 characters, three or more character classes, above 3.5 bits/char | The one heuristic, and the one you can turn off. |

A copy from a password manager is **masked, not skipped**: the entry is there, it pastes, and
`clippo reveal` prints it. A clipboard manager that silently drops a copy is worse than one that
hides it on screen.

The entropy rule declines git SHAs and UUIDs (hex is two character classes, and hyphens do not
count as a third), prose (whitespace), long base64 blobs and minified JavaScript (over 128
characters), and URLs and paths (structure, not randomness). Those are in
`crates/clippo-core/tests/corpus.toml` alongside the tokens and passwords that must be caught —
a fixture corpus in both directions, with the tuning written down next to the threshold it
justifies. A false positive there is annoying; a false negative is a password on a screen.

### What masking is

```
supersecretvalue  →  su••••••••ue
```

The first and last two characters, and a **fixed-width** run of bullets between them — eight,
whatever the value's length, because a mask that grew with the input would leak how long the
password was. A value with no more than four characters is hidden completely. Both counts are
configurable; the middle is not.

Masking is display-only, and that is a property of where it happens rather than a promise:

- **`clippo copy ID` pastes the real value.** The clipboard gets the stored bytes; the mask
  never leaves the list.
- **`clippo reveal ID` prints the real value**, and it is the only member of the daemon's D-Bus
  interface that returns one. `List` and `Search` hand out `entries.preview`, which for a
  sensitive entry is *already the mask in the database* — there is no unmasked preview for them
  to return.

Two consequences of masking before storage rather than on the way out, both worth knowing
before you go looking for them:

- **`clippo search` cannot find a masked entry by its contents.** Search matches previews, and
  a sensitive entry's preview is the mask, so the only thing that matches is the four visible
  characters. Find it in `clippo list` instead — masked rows are the ones with `s` in the `FL`
  column.
- **Entries copied before this version keep the preview they were stored with.** There is no
  migration pass over an existing history. Copying the same value again re-runs detection and
  replaces the preview with a mask, so anything you still use is corrected as you use it; to
  clear the rest at once, `clippo clear`.

### Configuring it

```toml
# ~/.config/clippo/config.toml
[secrets]
entropy_rule = true   # false keeps the MIME and shape rules and drops the heuristic
mask_prefix  = 2      # characters shown at the front; 0 shows none
mask_suffix  = 2      # …and at the back. The two together may not exceed 16.
```

`systemctl --user restart clippod` after editing — the config is read once, at startup.

### Checking it by hand

The automated half is `cargo test --workspace`; this is the half that needs a compositor, from
a host terminal with `clippod` running. It is [verification 4](docs/ROADMAP.md#4-secrets) in
the roadmap:

1. Copy a password out of KeePassXC, an `sk-`-prefixed token, and a JWT. Each shows as
   `ab••••••••yz` in `clippo list`, with `s` in the `FL` column.
2. `clippo copy <id>` on each, then paste into `cosmic-edit`. Each pastes **in full**. This is
   the one that matters: a mask reaching the clipboard would be the worst bug in this feature.
3. `clippo reveal <id>` prints each value whole.
4. Then the false-positive check: copy a git SHA (`git rev-parse HEAD`), a UUID
   (`uuidgen`), and a paragraph of prose. None of the three is flagged — no `s`, no bullets.

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
