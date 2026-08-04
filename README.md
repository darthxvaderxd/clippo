# clippo

A clipboard-history manager for the COSMIC desktop, written in Rust — with first-class
handling of the thing most clipboard managers get wrong: secrets. History is encrypted at
rest, and a suspected password or token is never rendered in full in the UI, while still
pasting the real value.

- [docs/DESIGN.md](docs/DESIGN.md) — architecture, components, and the decisions on record.
- [docs/ROADMAP.md](docs/ROADMAP.md) — build order (M0–M7) and how each stage is verified.

> **Status:** M7 complete — all milestones are in. `clippod` records every copy and serves
> `com.nilfactor.Clippo` on the session bus, `clippo copy <id>` puts an entry back on the
> clipboard for any application to paste, a suspected password or token shows as
> `ab••••••••yz` rather than in full — see [Secrets](#secrets) — [the applet](#the-applet) is
> a keyboard-driven picker in the COSMIC panel that [pastes straight into whatever you were
> working in](#pasting-for-you), and [`just install`](#installing) puts the lot in place with a
> systemd user unit. What is left is the manual verification that needs a
> real COSMIC session; the roadmap says which boxes those are.

## ⚠️ Build and run from a host terminal, not RustRover's Flatpak

**This is the single most likely thing to cost you an afternoon.** RustRover runs in a Flatpak
sandbox where `WAYLAND_DISPLAY=wayland-1` resolves to Flatpak's proxied socket, which
**filters out privileged protocols including data-control**. Launched from RustRover's
terminal or run configurations, `clippod` finds no clipboard-manager protocol at all — and
the failure mode is not an error you go looking for, it is capture silently doing nothing.

Use a host terminal (`cosmic-term`), and check the socket before anything else:

## Building

The host needs a Rust toolchain (stable); RustRover's bundled one is for its own use only:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

It also needs two system libraries — the **development** packages specifically. Having the
runtime `.so` that any Wayland desktop already ships is not enough: the build scripts ask
`pkg-config` for `xkbcommon.pc` and `wayland-client.pc`, and only the `-dev` package installs
those. On top of that, SQLCipher and the OpenSSL it uses are compiled from source here
(`rusqlite`'s `bundled-sqlcipher-vendored-openssl` feature — the encrypted store is not linked
against whatever OpenSSL the distribution happens to have), which wants a C compiler, `make`
and `perl`:

```sh
# Debian, Ubuntu, Pop!_OS
sudo apt install build-essential pkg-config perl libxkbcommon-dev libwayland-dev

# Fedora
sudo dnf install gcc make pkgconf-pkg-config perl-core libxkbcommon-devel wayland-devel

# Arch
sudo pacman -S --needed base-devel libxkbcommon wayland
```

Missing `libxkbcommon-dev` is the one that actually catches people, and it is worth knowing
what it looks like, because the failure is reported by a dependency's build script rather than
by anything with clippo's name on it:

```
error: failed to run custom build command for `smithay-client-toolkit v0.20.0`
  The system library `xkbcommon` required by crate `smithay-client-toolkit` was not found.
  The file `xkbcommon.pc` needs to be installed [...]
```

Then, from the repo root:

```sh
just build     # cargo build --workspace
just test      # cargo test --workspace
just check     # fmt + clippy + test, the build half of what CI runs
just deny      # cargo deny check advisories, the other half
```

`just deny` is separate because it needs `cargo-deny` installed and a network round trip to
the RustSec advisory database. CI runs it as its own job on every push, so the answer to "is
anything in this tree carrying a published advisory" is a current one rather than whenever
somebody last looked. What is deliberately accepted, and why, is in
[deny.toml](deny.toml) — as `ignore` entries with the reasoning next to them, never as a
loosened check.

`just` itself is optional for development — every recipe there is a one-line `cargo`
invocation you can run directly. `install`, `uninstall` and the two `*-panel` recipes are the
ones that do real work; see below.

## Installing

Four steps, none of them needing root, in a host terminal rather than a Flatpak — see the
warning at the top of this file. Installing from the wrong one installs perfectly well and
then captures nothing.

```sh
just install                              # 1. build and place the files
systemctl --user enable --now clippod     # 2. start recording, now and on every login
just add-to-panel                         # 3. put the icon in the panel's right wing
```

4. **Bind a shortcut**, if you want one — **Settings → Keyboard → Shortcuts → Custom
   shortcuts**, the command `clippo show` and the key `Super+V`.

Only the first three are `just`'s to do; the last is a COSMIC setting, and the sections below
say why none of the last three is something `install` does for you. Steps 3 and 4 are
optional in the sense that `clippo
list` and `clippo copy` work without them — but `Super+V` needs *both*, because `clippo show`
asks the panel applet to open, and a shortcut with no applet to talk to fails every time you
press it.

When all four are done, `clippo list` shows what you have copied, `Super+V` opens the picker,
and `systemctl --user status clippod` says `active (running)`.

`just install` does a release build and then puts everything in the XDG user locations:

| What | Where |
|---|---|
| `clippod`, `clippo`, `clippo-applet` | `~/.local/bin` |
| systemd **user** unit | `~/.config/systemd/user/clippod.service` |
| applet `.desktop` | `~/.local/share/applications` |
| AppStream metainfo | `~/.local/share/metainfo` |
| icons | `~/.local/share/icons/hicolor/{scalable,symbolic}/apps` |

`~/.local/bin` has to be on your `PATH` for `clippo` to be typeable — most shells on Pop!\_OS
add it already, and `command -v clippo` after installing tells you. The applet does not depend
on that: `install` writes the absolute path into the `.desktop`'s `Exec`, because the `PATH`
that would matter there is `cosmic-panel`'s rather than your shell's.

If either `XDG_DATA_HOME` or `XDG_CONFIG_HOME` is set, it is honoured — but only when it holds
an *absolute* path, which is what the XDG spec says and what `clippod` itself does. A relative
or empty value falls back to `~/.local/share` and `~/.config`, so the daemon and the installer
can never disagree about where anything is.

`systemctl --user enable --now clippod` starts the daemon and starts it again on every login:
the unit is `WantedBy=cosmic-session.target`, so it comes up with the COSMIC session and goes
down with it. `install` deliberately does *not* enable it for you — starting a daemon that
then owns your clipboard is not something to do to somebody as a side effect of copying
files.

`just add-to-panel` puts the icon at the front of the panel's **right** wing — the inner edge
of the right-hand group, next to the centre and ahead of the status applets, which is where
something you open on purpose belongs rather than lost among the indicators.
`just remove-from-panel` takes it off again and leaves the rest of the panel exactly as it
was. Both are idempotent, and `add-to-panel` *moves* an icon you have already placed by hand
rather than giving you two of them. It is a separate recipe rather than part of `install` for
the same reason the daemon is not enabled for you: rearranging somebody's panel is not
something to do to them as a side effect of copying files.

That position cannot be shipped in `res/`, which is why a recipe has to write it at all.
`cosmic-panel` reads exactly three keys out of an applet's `.desktop` — `X-CosmicShrinkable`,
`X-CosmicHoverPopup` and `X-NotificationsAppletClients` — and `cosmic-settings` reads one,
`X-CosmicApplet=true`, which is what makes clippo *offerable* in **Settings → Desktop → Panel
→ Configure panel applets** and says nothing about where it lands. There is no key for a wing
or an index. Placement lives only in
`~/.config/cosmic/com.system76.CosmicPanel.Panel/v1/plugins_wings`, a RON `Some((left, right))`
naming the applets in each wing, so that is the file `add-to-panel` edits. Three things it is
careful about:

- **An absent or `None` file means the distro's default layout, not an empty panel.** It seeds
  from the system copy under `XDG_DATA_DIRS` in that case. Writing just clippo into an empty
  layout would silently take every other applet off your panel.
- **It matches `cosmic-settings`' own formatting byte for byte**, so a second run can tell
  "already in place" from "changed" by comparing rather than diffing whitespace.
- **It refuses rather than guesses** if the file is not the shape it expects, leaving it
  untouched.

One thing worth knowing either way: `cosmic-settings` holds `plugins_wings` in memory and
writes the whole file back when you change anything on its Panel page, so an edit made
underneath a running one lasts until your next click there. `add-to-panel` warns if it sees
`cosmic-settings` running. Doing the whole thing by hand in that settings page works just as
well and is how you move it afterwards — a panel is somebody's own arrangement.
[The applet](#the-applet) is what you get, and what it does with the keyboard once it is open.

The `Super+V` shortcut is manual for a different reason: COSMIC owns global shortcuts and
clippo does not register one for itself. [Global shortcut](#global-shortcut) has the GUI route
above written out, the RON file to edit instead, and the two things that most often make a
binding do nothing.

### Installing somewhere else

```sh
just prefix=/usr/local install        # or: PREFIX=/usr/local just install
just prefix=/usr destdir=pkg install  # staged, for building a distro package
```

A prefix moves everything to the FHS layout under it — `$prefix/bin`, `$prefix/share/…`,
`$prefix/lib/systemd/user` — and `install` rewrites the unit's `ExecStart` and the `.desktop`'s
`Exec` to match. The unit stays a *user* unit whichever prefix you use: `clippod` needs the
session's Wayland socket, its session bus and its keyring, and a system service has none of
those. `uninstall` stops and disables it at every prefix for the same reason.

`destdir` prepends a staging root and touches nothing outside it — no cache refresh, no
`systemctl`, since on a packaged install those belong to the installing machine.

The assignment goes *before* the recipe name. That is `just`'s own grammar: `just install
prefix=/usr/local` is read as a request for a recipe named `prefix=/usr/local` and fails, so
use one of the two forms above.

### Uninstalling

```sh
just uninstall
```

It stops and disables the unit first, then removes the binaries, the unit, the `.desktop`,
the metainfo and the icons, and refreshes the caches. Both recipes are idempotent: installing
twice overwrites, and `uninstall` on a machine with nothing installed succeeds quietly.

**It does not touch your clipboard history.** Uninstalling a program is not a reason to throw
away what it was keeping for you, so the history and its key are left exactly where they are:

| What | Where |
|---|---|
| the encrypted history | `~/.local/share/clippo/history.db` |
| the key, normally | your keyring, as *clippo clipboard history database key* |
| the key, if there was no keyring on first run | `~/.local/share/clippo/key`, mode `0600` |

To delete those too, ask for it by name — this is the only recipe in the project that
destroys anything, and it confirms before it does:

```sh
just purge-data
```

That removes `~/.local/share/clippo` and prints how to clear the keyring entry, which is
separate and stays until you remove it in **Passwords and Keys** or with:

```sh
secret-tool clear xdg:schema com.nilfactor.Clippo.DatabaseKey \
                  application clippo purpose history-database-key
```

Deleting `~/.local/share/clippo` by hand does exactly the same thing; there is nothing else
to clean up.

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
not a clippo bug and there is no fix, only a mitigation: the systemd unit carries
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
| `clippo paste ID` | The same, and then press your paste shortcut into whatever window has keyboard focus. From a terminal that is usually the terminal itself. Says so when it pressed nothing. See [Pasting for you](#pasting-for-you). |
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

`systemctl --user start clippod` is the answer once you have [installed](#installing); from a
working copy, `just run-daemon`.

## The applet

A panel icon that opens a picker over the same history the CLI reads. It owns no clipboard
state of its own — every row came from the daemon's `Search`, and every action it takes is a
call on the same D-Bus members `clippo` uses, so there is no second code path to disagree with
the first.

Normally `cosmic-panel` starts it, and putting it on the panel is `just add-to-panel` or COSMIC
**Settings → Desktop → Panel → Configure panel applets**. What puts clippo in that settings
list is the `.desktop` file `just install` writes, and specifically its `X-CosmicApplet=true` —
without it clippo is not offered there whatever else is installed. See
[Installing](#installing) for where the icon lands and why placement is a recipe of its own.

To run it by hand from a working copy instead, without installing:

```sh
cargo run -p clippo-applet          # or `just run-applet`
```

Uninstalled, the panel icon falls back to the theme's `edit-paste-symbolic`, because clippo's
own icon is only in the icon theme once `just install` has put it there.

### Keyboard

The picker is keyboard-first: the search field takes focus the moment it opens, so you can
start typing immediately, and nothing below needs the mouse.

| Key | What it does |
|---|---|
| *(type)* | Filter. This is the daemon's `Search`, so the applet and `clippo search` rank a query identically. |
| `↑` / `↓` | Move the highlight. It stops at the ends rather than wrapping. |
| `Enter` | Paste the highlighted entry into whatever you were last working in, and close. It is copied first and always, so it is on the clipboard either way — see [Pasting for you](#pasting-for-you). |
| `Delete` | Remove the highlighted entry. No confirmation — this is one row of a rolling history, and `clippo clear` is the destructive one. |
| `Ctrl+P` | Pin or unpin. A pinned entry is exempt from retention and from `clear`. |
| `Ctrl+R` | Show the highlighted entry's stored value in place, up to a bound. This is how a masked row is read — see below. |
| `Escape` | Close without doing anything. Clicking outside the picker does *not* — see [the applet](#the-applet) for why. |

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
the selection stops it being drawn, and it is dropped when the picker closes — however it
closed, including when the compositor took the focus away. The value is held zeroized from the moment it arrives, so dropping
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
picked up without a restart. `clippo` has to be on `PATH` for `Spawn` to find it, which
[installing](#installing) arranges as long as `~/.local/bin` is on yours; from an
uninstalled working copy, use the absolute path to the built binary instead.

Two things worth knowing about this path:

- `clippo show` needs the **applet**, not just the daemon. A running `clippod` is not enough,
  and the error says so and names the panel setting.
- A picker opened by a global shortcut has no input serial to hand the compositor, so it could
  get no `xdg_popup` grab — and with no grab, no keyboard. That is why the picker is a layer
  surface rather than a popup: layer shell can ask for keyboard focus outright. The visible
  cost is that **clicking outside the picker does not close it**; `Escape` does, as does the
  panel icon or a second `clippo show`.
- The picker opens **in the middle of the screen** rather than under the panel icon. A layer
  surface is positioned against the whole output rather than against the applet that opened it,
  so it goes where COSMIC's other keyboard-opened surfaces go.

### Pasting for you

Pressing `Enter` on a row does not only copy it. `clippod` puts the entry on the clipboard and
then presses your paste shortcut into whatever window had focus before the picker opened, so
the value lands where your cursor was. `clippo paste ID` does the same thing from a terminal.

Wayland gives no client a way to write into another application's window — deliberately — so
this works by synthesising the keystroke you would have pressed, through
`zwp_virtual_keyboard_v1`. `cosmic-comp` supports it. A compositor that does not is not a
problem: `Paste` still copies, and you paste by hand exactly as before — and if you would
rather it never did this, `auto_paste = false` below turns it off. `clippod` says which you
have at startup:

```
$ journalctl --user -u clippod -b | grep 'paste shortcut'
clippo can press the paste shortcut for you
```

**The shortcut it presses is one setting for every application**, and applications disagree
about it. The default is `Ctrl+V`, which is right nearly everywhere and wrong in most
terminals — `cosmic-term`, like most, pastes on `Ctrl+Shift+V`. Set whichever you paste into
most often:

```toml
# ~/.config/clippo/config.toml
paste_shortcut = "Ctrl+Shift+V"
```

**To turn the pressing off entirely**, leaving `Enter` to copy and nothing else:

```toml
# ~/.config/clippo/config.toml
auto_paste = false
```

That is a switch on whether clippo may synthesise input at all, not a preference about the
picker, so `clippo paste` stops pressing too — a setting that said "never type into my
windows" and then had an exception that types into your windows would not be one. With it off,
`clippod` does not even create the virtual keyboard, so there is nothing left to press with.
Everything else is unchanged: `Enter` still copies, and your own paste key still works.

Modifiers are `Ctrl`, `Shift`, `Alt` and `Super`, in any order and any casing, and the key is
a letter, a digit or `Insert`. A shortcut clippo cannot press stops the daemon at startup and
names the part it could not read, rather than leaving you with a `Paste` that quietly does
nothing. Like every other key, it is read once — `systemctl --user restart clippod` after
editing.

Two limits worth knowing, both of which follow from how Wayland works rather than from clippo:

- **It goes to whatever has focus, which nothing can address or predict.** The picker closes
  first and the daemon waits a moment before pressing, but there is no event anywhere that
  says "focus has finished returning" — so if you click into a different window in that
  moment, that is where it pastes.
- **The copy always happens.** If the shortcut is wrong for the application you are pasting
  into, nothing is lost: the entry is on the clipboard, and the application's own paste key
  still works. That is also why a keystroke clippo could not send is not reported as a failed
  `Paste` — the half that matters succeeded. The reason goes to `clippod`'s journal.

## Who can talk to clippod

Worth knowing before you decide how much to keep in your history: **a session bus authenticates
nobody.** Every process running as you can call every member of `com.nilfactor.Clippo`, and
every process running as you can equally *take* that name the moment nothing owns it. Those are
the same gap read in two directions, and clippo does what can be done about each — which is less
than it sounds like, so it is written down rather than implied.

The part that is not just "a local process can read local files" is this: `clippod` re-exports
two capabilities Wayland is careful about. Reading the clipboard, and typing into other
applications. A Flatpak application with `--socket=session-bus` — an extremely common manifest
line — is deliberately denied both by the proxied Wayland socket. With `clippod` running it can
have them back: `List` for the ids, `Reveal` for each value, `Paste` to have clippod type a
chosen entry into whatever window has focus. Aim that at a terminal and the entry is a command
line.

**What clippo checks.** `Paste` looks at who called it: the bus is asked for the caller's pid,
`/proc/<pid>/exe` is read, and it must be one of the clippo binaries installed beside `clippod`
— a peer inside a Flatpak sandbox is refused whatever its executable claims to be. The
frontends run the identical check in the other direction, on whoever owns the daemon's name,
before they send anything and again every time the name changes hands. `clippo` exits non-zero
saying so, and the picker draws it instead of quietly reconnecting.

**Be clear about what that is.** Pids get reused, and an allowlisted binary does whatever
whoever started it tells it to. It narrows impersonation from *anything on the bus* to
*anything that can be, or can drive, the real binary*. That is a speed bump, not a boundary,
and it is worth having mostly because it is cheap. A uid check would be worth nothing at all —
every peer on a session bus is the same uid, which is also why a D-Bus policy file cannot help.
The genuinely correct answer is compositor-mediated access, which does not exist for clipboard
managers on Wayland yet.

One consequence to expect: `Paste` from a binary outside the daemon's own tree is refused, and
says which executable it saw. An installed `clippod` talking to a `cargo run` frontend is the
case that hits — run both from the same place.

**Turning the two capabilities off.** If you run sandboxed applications and would rather they
could not ask at all:

```toml
# ~/.config/clippo/config.toml
allow_privileged_members = false
```

`Reveal` and `Paste` are then refused outright, and nothing else changes: the history is intact,
`clippo list` and `clippo search` work, and `Copy` still puts an entry on the clipboard for you
to paste yourself. The cost is that `clippo reveal` stops working, so `Ctrl+R` in the picker has
nothing to show, and `Enter` in the picker becomes a copy: the applet falls back to `Copy` when
its `Paste` comes back refused, so choosing an entry still puts it on the clipboard and only the
keystroke is lost. `clippo paste 2` on the command line reports the refusal instead — use
`clippo copy 2`. **The daemon enforces it**, not the frontends —
a switch a caller could skip by not being the applet would not be one. Read once at startup, so
`systemctl --user restart clippod` after editing, and `clippod` says which way it is set:

```
$ journalctl --user -u clippod -b | grep allow_privileged_members
allow_privileged_members is off; Reveal and Paste will be refused over D-Bus
```

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

Everything `just install` places lives in `res/`: the systemd user unit, the applet
`.desktop`, the AppStream metainfo and the two icons.

## License

GPL-3.0-only. See [LICENSE](LICENSE).
