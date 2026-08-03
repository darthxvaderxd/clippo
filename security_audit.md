# clippo — security audit

Reviewer: Sam (security engineer)
Date: 2026-08-03
Revision audited: `574472c` on `harness/sam-worker/2026-08-03-04-59-48-clippo-security-audit`
(M7 complete — all seven crates present)

This is an audit of the repository as it stands, not a review of a diff. Nothing here has been
fixed; each finding says precisely enough for someone else to fix it.

Second revision, after review. What changed: **F5** is new — the mirror of F1, and the reason F2
is more than a denial of service; *Dependency posture* now works from `cargo tree -d` rather than
from the manifests, which is what surfaced the `zbus 4.4.0` beneath `oo7` on the daemon's link
path; the image-decoding and key-management certifications now state which decoder and which
crate they actually cover; and `clippo reveal`'s unescaped output has moved from a clause inside
a clean certification to a note of its own. F1–F4 are otherwise unchanged.

Third revision: F5's fix previously suggested that `clippod` request its name with
`AllowReplacement | ReplaceExisting` so a restarting daemon could take the name back. **That was
wrong and the correction is in F5's third fix step** — `ReplaceExisting` only succeeds against an
owner that itself set `AllowReplacement`, which an impostor will not, so the change would have
bought nothing against the attacker while letting any peer take the name from a live daemon in
one call. The step now says what is true instead: name-request flags are not a lever here at all.
Findings and severities are unchanged.

---

## Scope and method

Every source file in `crates/` was read, plus `res/clippod.service`, `.github/workflows/ci.yml`,
the `justfile`'s install/uninstall recipes, `Cargo.toml` and `Cargo.lock`; `cargo tree -d` and
`cargo tree -i` were run to establish which versions of a duplicated crate are on which
binary's link path. The approach was to trace values from untrusted inputs to sinks rather than
to grep for patterns. The untrusted inputs clippo actually has are:

| Input | Who controls it | Where it enters |
|---|---|---|
| Selection MIME types and blob bytes | any Wayland client on the seat | `clippo-wayland::watch` |
| D-Bus method arguments | any peer on the session bus | `clippo-ipc::service` |
| D-Bus method *replies* | whoever owns `com.nilfactor.Clippo` | `clippo-applet::bus`, `clippo-cli::client` |
| `~/.config/clippo/config.toml` | the user | `clippo-core::config` |
| `history.db`, `key` | whoever can write the data directory | `clippo-store` |
| CLI arguments | the user | `clippo-cli` |

Two sinks are genuinely dangerous and both were traced end to end: the **synthesised keystroke**
(`clippo-wayland::keys`, reached only from `Paste`), and **`Reveal`**, the one member that
returns a stored value in full. There is no shell execution, no process spawning, no `unsafe`,
and no path or URL built from clipboard content anywhere in the workspace — `grep` for
`Command::new`, `unsafe`, `process::` returns only `ExitCode` imports and test scaffolding.

The codebase is unusually careful. Masking, display escaping, key handling, echo suppression and
blob lifetime are all reasoned about in comments *and* pinned by tests, and I found no defect in
any of them. The findings below are concentrated in one place the design has not reasoned about
— **peer identity on the session bus**, in both directions: who is allowed to call the daemon
(F1) and who the frontends are willing to accept *as* the daemon (F5) — plus one input-handling
bug on the capture path.

---

## Findings

### F1 — The D-Bus surface has no caller authentication; `Paste` makes `clippod` a keystroke-injection deputy and `Reveal` a clipboard-history oracle

**Severity: Medium.** Reachable by any peer on the session bus with no authentication and no user
interaction. Not remote; it does not cross a user boundary, but it does cross a *sandbox*
boundary, which is where the real escalation is.
**Confidence: the code path is confirmed, traced end to end. The sandbox escalation is
high-confidence but not tested on a live host — see below.**

**The path.** `clippod` connects to the session bus and exports its object
(`crates/clippod/src/main.rs:153-177`). Every member is forwarded straight to the backend with no
inspection of the message header (`crates/clippo-ipc/src/service.rs:125-181`); nothing in the
workspace ever reads the sender's unique name, calls `GetConnectionCredentials`, or consults a
policy file. There is no D-Bus policy `.conf` shipped in `res/`, and the session bus imposes none
of its own between peers of the same user.

Two members are more than "read the user's own data":

- **`Paste(id)`** (`crates/clippod/src/daemon.rs:506-576`) copies the entry, waits 120 ms, and
  then presses the configured chord through `zwp_virtual_keyboard_v1` into whatever holds
  keyboard focus. The daemon holds that keyboard for the life of the process
  (`crates/clippod/src/main.rs:213-219`).
- **`Reveal(id)`** (`crates/clippod/src/daemon.rs:624-639`) returns the whole stored value of any
  entry, mask or no mask. `List` gives a caller every id and the `sensitive` flag, so choosing
  which entry to reveal takes one prior call.

**Exploitation scenario.** A Flatpak-packaged application with `--socket=session-bus` — an
extremely common manifest line — is, on this platform, deliberately denied the privileged Wayland
protocols: DESIGN.md itself records that the Flatpak-proxied socket "filters out privileged
protocols including data-control", which is the same filter that covers
`zwp_virtual_keyboard_v1`. Such an app cannot read the clipboard and cannot synthesise input.
With `clippod` running it can do both:

1. `com.nilfactor.Clippo.List(0, 0)` → every entry id, with `sensitive` marking the passwords.
2. `Reveal(id)` on each → the full history in the clear, including every value the masking
   feature exists to keep off the screen. The sandbox's clipboard restriction is bypassed
   completely.
3. To inject input: take the clipboard itself if it can, or simply pick any existing entry, then
   `Paste(id)`. `clippod` — outside the sandbox — presses `Ctrl+V` into whichever window has
   focus. Aim it at a terminal and the pasted entry is a command line.

The same two calls are available to any non-sandboxed process running as the user, which by
itself is not an escalation (that process could read `~/.local/share/clippo` and the keyring
anyway). The escalation is specifically that `clippod` re-exports two *privileged Wayland
capabilities* — reading the clipboard and typing into other applications — over an unauthenticated
IPC channel that sandboxes routinely leave open precisely because it is assumed not to carry
those capabilities.

**The project has already reasoned about this threat once and stopped one field short.**
`crates/clippo-ipc/src/lib.rs:140-146` excludes `entries.hash` from `EntrySummary` because it is
"a confirmation oracle for a guessed value — it belongs inside the encrypted database, not on a
session bus that any process running as the user can call". That is exactly the right analysis;
it just was not carried across to the member that returns the value the hash would only have
confirmed.

**Fix.** In rough order of cost:

1. **State the threat model.** DESIGN.md has a "Known risks" table and this is not in it. At
   minimum, say that every session-bus peer is fully trusted with the entire history and with
   the ability to type into focused windows, so an operator can weigh that against sandboxed
   applications.
2. **Restrict `Paste` specifically.** It is the only member whose effect leaves clippo's own
   data. `zbus` gives the served method the message header via a `#[zbus(header)] hdr: Header<'_>`
   argument; `hdr.sender()` plus `fdo::DBusProxy::get_connection_unix_process_id` yields a pid
   whose `/proc/<pid>/exe` (and `/.flatpak-info` presence) can be checked against an allowlist —
   the applet and the CLI. This is advisory rather than airtight (pids are reusable and an
   allowlisted binary can be driven by its own caller), so it should be described as a speed bump,
   not a boundary.
3. **Consider a config knob** — `allow_reveal_over_dbus` / the existing `auto_paste` extended to
   "applet only" — so a user who runs sandboxed applications can turn the two capabilities off
   without losing the history.

I would not recommend inventing a bespoke authentication scheme here. The genuinely correct
answer for a clipboard manager on Wayland is compositor-mediated access (a portal), which does
not exist for this yet; documenting the gap is the honest interim step.

See **F5**, which is the same gap read in the other direction and is placed next because the two
share a fix.

---

### F5 — Nothing verifies who owns `com.nilfactor.Clippo`, so a peer that takes the name becomes the history: every search keystroke, and every row the user is shown

*(Numbered out of order deliberately: F5 was found after F2–F4 but belongs beside F1, and
renumbering would break references from the previous revision of this document.)*

**Severity: Medium.** Reachable by any peer on the session bus with no authentication and no
user interaction, but it must first take a name the daemon normally holds — the "Taking the
name" section below is what makes that cheap rather than a coin-flip at login.
**Confidence: the code paths are confirmed, traced end to end (name request, both frontends'
lack of an owner check, the search-per-keystroke route). The queue-and-wait acquisition below is
standard D-Bus name-ownership behaviour applied to this unit file and this flag set, reasoned
rather than executed against a live bus — see the caveat where it is stated.**

**The path.** `acquire_name` (`crates/clippod/src/main.rs:289-311`) requests the name with
`RequestNameFlags::DoNotQueue` and nothing else. That flag set is right for the problem it was
written for — the module comment says so, and it is the reason a second `clippod` exits loudly
instead of writing to the same database — and the absence of `AllowReplacement` means a *running*
`clippod` cannot have the name taken from it. What is missing is the other half: neither frontend
ever asks who answered.

- The applet resolves `BUS_NAME` and calls (`crates/clippo-applet/src/bus.rs:381-430`). The only
  owner-related code in the workspace is presence-checking — `watch_daemon_name` (`:330-350`) and
  `daemon_is_running` (`:369-374`) — both of which ask *whether* the name has an owner, never
  *which* peer owns it.
- The CLI is the same: `Client::connect` (`crates/clippo-cli/src/client.rs:33-37`) builds a proxy
  on the well-known name and, as its own doc comment notes, sends nothing until the first call.
- `grep` for `GetConnectionCredentials`, `GetNameOwner` or `get_connection_unix_process_id` across
  the workspace returns nothing.

**Taking the name.** The impostor does not have to win a race at login, which is what makes this
worth writing down:

1. It calls `RequestName` *without* `DoNotQueue` and is put in the queue, owning nothing. This
   costs it nothing and is invisible — `clippod` never enumerates queued owners.
2. It waits for the current owner to drop the name; the queued peer becomes primary owner the
   instant that happens. Any mid-session `clippod` exit will do — a crash, an OOM kill, a
   `systemctl --user restart clippod`, or **F2 above**. (Logout is no use to the attacker:
   `PartOf=cosmic-session.target` ends the whole session, so there is no user left to watch.)
3. `clippod` restarts five seconds later (`RestartSec=5`), finds the name taken, and exits
   non-zero with the "another process already owns…" message. `Restart=on-failure` retries;
   `StartLimitBurst=20` over `StartLimitIntervalSec=300` means it gives up after about a hundred
   seconds and stays `failed` for the rest of the session. The impostor then owns the name
   uncontested and the real daemon is out of the way permanently. The user's evidence that
   anything happened is a `failed` unit and a journal line — the applet reconnects and looks
   normal.

So F2 is not only a denial of service on its own terms — it is the trigger for this. That chain
is the reason I would not treat F2's severity as purely availability.

**What the impostor gets.** The applet reconnects as soon as `NameOwnerChanged` says the name has
an owner again (`bus.rs:330-368`) and starts talking to it:

- **Every character typed into the picker's search field.** `Message::QueryChanged` calls
  `refresh()` on every keystroke and the applet sends the whole query to the daemon's `Search`
  rather than filtering locally (`crates/clippo-applet/src/app.rs:447-454`, `bus.rs:387-393`).
  The comment explains why — the applet and `clippo search` must rank identically, so only one of
  them ranks — and the reasoning is sound; the consequence is that the search box is a keylogger
  the moment the peer on the other end is hostile. Users search a clipboard history for the thing
  they are about to paste, so the queries are not innocuous.
- **Full control of what the user is shown.** `Search` replies are rendered as rows, `Reveal`
  replies are displayed as the entry's value, and `Thumbnail` replies are displayed as its
  picture. A user who opens the picker to retrieve a copied token sees rows the impostor wrote.
- **`Toggle` as well, on the same terms.** `serve_toggle` (`bus.rs:307-327`) *warns and carries
  on* when `com.nilfactor.ClippoApplet` is already taken — "another instance has it, and
  `clippo show` will reach that one". The consequence is small (a toggle carries no data) but it
  is the same missing check, and the comment shows the collision was considered as an
  operational problem rather than as a security one.

**What it does not get, stated so nobody over-reads this.** The impostor cannot type: keystroke
synthesis needs `zwp_virtual_keyboard_v1`, and the applet does nothing locally when `Paste`
returns — it neither writes the clipboard itself nor checks the reply (`bus.rs:400-403`). It
cannot read the real history either; that is on disk under the user's key and a hostile *peer*
has no more access to it than before. Whether it can put its own bytes on the clipboard depends
on its own Wayland access, not on anything clippo gives it: a normal client on the seat can set a
selection through `wl_data_device`, but the sandboxed app of F1's scenario — denied data-control
— generally cannot. **The primitive here is display and input *capture*, not clipboard write.**
It is the mirror image of F1: F1 is "any peer can call `clippod`", this is "any peer can *be*
`clippod`", and the second needs no privileged Wayland protocol at all.

**Fix.**

1. **Have the frontends check the owner, and make the check the same shape as F1's.**
   `fdo::DBusProxy::get_name_owner` gives the unique name behind `com.nilfactor.Clippo` and
   `get_connection_credentials` on that returns the owner's pid; `/proc/<pid>/exe` compared
   against the installed `clippod` path (plus the absence of `/.flatpak-info`) is the check with
   any content in it. Run it at connect time and again on every `NameOwnerChanged` that brings the
   name back, and refuse to send anything to an owner that fails it.

   Be clear about what this is not: **a uid check is nearly vacuous here.** Every peer on a
   session bus is the same uid by construction, so "is the owner my uid" is true for the impostor
   too. And the pid check has the same weaknesses on this side as it does in F1's — pids are
   reusable, and an allowlisted binary can be driven by whoever started it. This narrows the set
   of processes that can impersonate the daemon from "anything on the bus" to "anything that can
   be, or can drive, the real `clippod` binary". That is a speed bump, not a boundary, and it is
   worth having only because it is a handful of lines. It is the same helper F1 needs, pointed the
   other way — one function, two call sites, which is the argument for doing both at once.
2. **Make the frontends say so, loudly.** The reason the chain above ends with "the applet
   reconnects and looks normal" is that reconnection is silent. `watch_daemon_name`
   (`bus.rs:330-368`) treats a new owner as good news. Pair it with step 1: when the owner check
   fails, the picker should show that it is refusing to talk to whoever holds the name, and
   `clippo` should exit with that message rather than a generic call failure. A `failed` unit and
   a journal line are not evidence the user will look at; the picker is.
3. **Do not reach for the name-request flags. They cannot evict a squatter, and changing them
   makes this finding worse.** This is worth stating explicitly because it is the obvious-looking
   fix and it is a trap. `ReplaceExisting` succeeds only when the *current* owner requested
   `AllowReplacement` — zbus 5.18.0's own documentation is unambiguous
   (`src/fdo/dbus.rs:31-33`: "If `AllowReplacement` is not specified by application A, or
   `ReplaceExisting` is not specified by application B, then application B will not replace
   application A as the owner"; `ReplaceExisting`'s own doc at `:35-38` says the same from the
   other side, and `RequestNameReply::PrimaryOwner` at `:54-57` is documented as requiring both
   halves). The impostor chooses its own flags and has no reason to set `AllowReplacement`; it
   does not need it, because queue-and-wait requires only the *absence* of `DoNotQueue`. So a
   restarting `clippod` asking with `AllowReplacement | ReplaceExisting` would still get
   `Exists`, still hit the same `taken()` arm (`main.rs:307`), and still exit — step 3 of *Taking
   the name* plays out identically. What the change would buy the attacker, on the other hand, is
   substantial: `AllowReplacement` on `clippod` lets any session-bus peer take the name from a
   **live, healthy** daemon with a single `RequestName(ReplaceExisting)` call. That deletes this
   finding's own precondition — no crash needed, no F2 needed, no waiting — and turns a Medium
   finding into an unconditional one-call takeover.

   Dropping `DoNotQueue` so `clippod` queues instead of exiting is the only flag-level change
   that recovers ownership at all, and it reintroduces exactly the hazard `acquire_name`'s doc
   comment (`main.rs:284-288`) says the flag exists to prevent: a queued `clippod` watches the
   clipboard and writes to the database while owning nothing. Bus policy is no help either —
   `<deny own="…"/>` can only discriminate by uid, and every peer on a session bus is the same
   uid, the same reason the uid check in step 1 is vacuous. **There is no configuration-level
   defence here.** Steps 1 and 2 are the whole of the available mitigation, which is itself the
   argument for doing them.
4. **Say it in DESIGN.md's risk table**, alongside F1's entry. An operator reading the current
   table learns that peers are trusted *with* the history. They should also learn that a peer can
   *be* the history — and, per the previous step, that the name flags are not the lever they
   look like.

---

### F2 — One hostile selection can force an unbounded number of capped flavor reads, because dedup compares MIME strings exactly while the interest test does not

**Severity: Low–Medium.** Denial of service in itself — but see F5, for which a daemon crash is
the trigger, so an attacker who can reach both gets more than availability out of this one.
Reachable by any Wayland client on the seat, with no authentication and no user interaction
beyond the client setting a selection.
**Confidence: the code defect is confirmed by reading; the peak-resource figures below are
reasoned estimates, not measured.**

**The path.** `is_interesting` normalises before comparing — it strips *all* ASCII whitespace and
compares case-insensitively (`crates/clippo-wayland/src/mime.rs:40-45`, `:66-71`). The dedup in
`interesting_flavors` compares the raw strings instead:

```rust
// crates/clippo-wayland/src/watch.rs:972-980
if mime::is_interesting(mime) && !wanted.iter().any(|seen| seen == mime) {
    wanted.push(mime.clone());
}
```

So `"text/plain"`, `"text/plain "`, `" text/plain"`, `"te xt/plain"`, `"TEXT/plain"` and
`"text/pl a in"` are six distinct entries in `wanted`, all of which pass the interest test. Since
whitespace may be inserted at any position any number of times, the number of distinct strings a
source can produce that clippo will accept as interesting is unbounded.

Each surviving entry then gets its own pipe, its own loop registration and its own
`FlavorBuffer` with the full `max_flavor_bytes` cap — the per-flavor cap is applied per flavor,
and nothing caps the count (`crates/clippo-wayland/src/watch.rs:478-489`,
`crates/clippo-wayland/src/flavor.rs:125-129`).

**Exploitation scenario.** A malicious or merely buggy application creates a data source
advertising, say, 5 000 whitespace-permuted spellings of `text/plain`, then takes the selection.
`clippod` opens 5 000 pipes — exhausting a default `RLIMIT_NOFILE` of 1024 partway through, at
which point each failure is a `warn!` line carrying the attacker's MIME string — and buffers up
to 8 MB per surviving flavor for the 5-second `flavor_read_timeout`, with the attacker writing
into all of them concurrently. Peak resident memory is bounded by what the attacker can push in
five seconds (realistically single-digit GB on a fast machine), which is enough to OOM-kill the
daemon on a typical laptop. The attacker can repeat this on every selection, and
`Restart=on-failure` brings the daemon back for the next round. Losing `clippod` also empties the
clipboard, by design.

There is a second, cheaper effect on the same input: `interesting_flavors` is O(n²) in `wanted`,
and `wanted` is the attacker-controlled quantity, so ~100 k advertised variants is ~10¹⁰ string
comparisons on the watcher thread — the same thread that answers pastes.

**The right helper already exists and is not being called.** `mime::same`
(`crates/clippo-wayland/src/mime.rs:61-63`) is exactly this comparison — normalise both sides,
compare case-insensitively — and is used by `offer::blob_for` on the paste path. The dedup here
should use it. With `same`, `wanted` can never exceed the seven entries of `INTERESTING_MIMES`
and the resource use is bounded by construction.

**Fix.**

1. In `interesting_flavors`, replace `seen == mime` with `mime::same(seen, mime)`.
2. Belt and braces, since the bound then rests on a list rather than on arithmetic: cap `wanted`
   at `INTERESTING_MIMES.len()` and log once if a selection advertised more than that.
3. Consider bounding `OfferState::mimes` too (`crates/clippo-wayland/src/watch.rs:364-369`),
   which accumulates every advertised type including uninteresting ones with no ceiling; it is
   less severe because those strings are never read from a pipe, but it is the same shape of
   problem.
4. Second-order, worth one line while the file is open: the `warn!` calls at
   `crates/clippo-wayland/src/watch.rs:429`, `:485` and `:627` interpolate the attacker's MIME
   string into the journal unescaped. `clippo_core::display::is_invisible_or_reordering` exists
   for exactly this hazard and is applied to previews but not to MIME types. Low impact — the
   strings are bounded by the Wayland message size — but the volume is not bounded, and the
   journal is a shared resource.

---

### F3 — Secret detection, the preview, and the pasted bytes are read from three different flavors, so a secret in a non-preview flavor is never flagged

**Severity: Low.** A detection gap rather than a leak; no confidentiality boundary is crossed.
**Confidence: confirmed by reading `preview.rs` end to end; the practical exposure is my
judgement, not a measurement.**

**The path.** `describe` runs detection over `whole_value(kind, flavors)`
(`crates/clippod/src/preview.rs:127-148`, `:210-215`), and `whole_value` reads *one* flavor:
`preview_source`, which prefers the first flavor whose MIME essence is `text/plain` and falls
back to the canonical flavor (`crates/clippod/src/preview.rs:161-171`). Meanwhile the entry's
identity is BLAKE3 over the *canonical* (richest) flavor, and `Copy`/`Paste` offer back **every**
stored flavor (`crates/clippod/src/daemon.rs:316-320`).

**Exploitation scenario.** A web page calls `navigator.clipboard.write()` with a `ClipboardItem`
carrying `text/plain: "Click to continue"` and `text/html: "<code>sk-…realkey…</code>"`. The
entry is stored as `kind = html`; detection reads only the `text/plain` flavor, so no shape rule
and no entropy rule ever sees the key. The row is not flagged `sensitive`, gets no lock badge,
and the preview is the innocuous line. The key is still in the encrypted `flavors` table and is
still what a rich-text target receives on paste.

The consequence is bounded — the secret is not *displayed*, so nothing is leaked to a shoulder
surfer, and the store is encrypted either way. What is lost is the flag, which is the one signal
the applet and `clippo list` give the user that a row is dangerous. It is a false negative, and
DESIGN.md names false negatives as "the actual risk".

Note that the preview/paste divergence itself is inherent to a multi-flavor clipboard: pasting
from the original selection behaves identically. I am *not* claiming clippo introduces a spoofing
primitive. The finding is specifically about detection reading one flavor while the entry carries
several.

**Fix.** Run detection over every text-bearing flavor and OR the results — `describe` already
returns a single `sensitive` bool and a `Signal`, so the change is to iterate `flavors` in
`describe` rather than to call `whole_value` once, keeping `whole_value` as the source of the
*rendered* preview. Cost is one extra regex pass per additional text flavor per copy, which is
the same order as what the capture path already spends. `Reveal` should keep reading
`preview_source` so that revealing shows the whole of the thing the row showed part of, as its
doc comment argues.

---

### F4 — A unit test writes to a predictable path in the shared temp directory

**Severity: Low.** Test-only; requires a local attacker on a shared build host.
**Confidence: confirmed by reading.**

`crates/clippo-core/src/config.rs:782-790`:

```rust
let dir = std::env::temp_dir().join(format!("clippo-config-{}", std::process::id()));
let file = dir.join(paths::CONFIG_FILE_NAME);
std::fs::create_dir_all(&dir).unwrap();
std::fs::write(&file, "max_entries = 7\n…").unwrap();
```

`create_dir_all` succeeds against a pre-existing directory, and `fs::write` follows symlinks. On
a multi-user machine another local account can pre-create `/tmp/clippo-config-<pid>/config.toml`
as a symlink — pids are a small, guessable space and the attacker can seed many candidates — and
have `cargo test` truncate and overwrite an arbitrary file writable by the user running the
tests.

**The right helper is already in the workspace and is used everywhere else in the repo.**
`tempfile = "3"` is in `[workspace.dependencies]` (`Cargo.toml:93`) and `key.rs`, `store.rs` and
`daemon.rs` all use `tempfile::tempdir()`. This one test does not.

**Fix.** Replace with `tempfile::tempdir()`, as the tests in the other crates do; that also
removes the `remove_dir_all` at the end, which currently leaks the directory whenever an
assertion above it fails. One wrinkle worth knowing before someone picks this up: it is not a
pure call-site swap, because `crates/clippo-core/Cargo.toml` has **no `[dev-dependencies]`
section at all** — the fix means adding one with `tempfile = { workspace = true }`, as
`clippo-store` already has. Still a small change, but not a one-liner.

---

## Notes — things worth knowing that are not vulnerabilities

- **The data directory's mode is enforced only when clippo creates it.** `create_data_dir`
  (`crates/clippo-store/src/key.rs:488-504`) creates `~/.local/share/clippo` at `0700` but
  returns `Ok(())` for an `AlreadyExists` directory without checking or tightening its mode, and
  `history.db` is created by SQLite at the process umask (typically `0644`). The key file's own
  mode *is* checked and a wider-than-`0600` file is refused, which is the check that matters, so
  this is not exploitable as it stands — but if the directory pre-exists at `0755`, the encrypted
  database is world-readable ciphertext, which leaks size and change frequency. A mode check on
  the directory alongside the existing one on the key file would close the gap cheaply.
- **The revealed value crosses D-Bus as an ordinary string.** Both sides handle their own copy
  well — `Zeroizing` from the moment it arrives in `crates/clippo-applet/src/bus.rs:412-414`, and
  a hand-written `Debug` at `:149-165` so a stray `debug!(?event)` cannot print it — but the
  zbus message buffers underneath are not zeroized on either end. There is no practical fix short
  of not sending the value; noting it so nobody later concludes the `Zeroizing` wrapper is a
  complete guarantee.
- **`Search` is unbounded work on an unbounded argument.** A session-bus peer can send a
  multi-megabyte query and `nucleo-matcher` will parse and score it under the state lock
  (`crates/clippod/src/cache.rs:92-114`). Any peer that can do this can also call `Reveal`, so it
  buys an attacker nothing they do not already have; a length cap on `query` would still be
  cheap insurance.
- **`clippo reveal` writes clipboard content to the terminal unescaped.** `run.rs:134-139` emits
  the stored value byte for byte — "no trailing newline, no sanitising", as the comment says — so
  a copied ANSI or OSC sequence reaches the terminal raw and can repaint it, retitle the window,
  or on some terminals prime the input buffer. I am not filing this as a finding: it is the
  documented purpose of the command, `clippo reveal --help` warns about escape sequences
  explicitly (`cli.rs:85`), and `reveals_help_says_it_does_not_sanitize` (`cli.rs:448`) pins the
  warning so it cannot be edited away unnoticed. That is the right treatment. It is recorded here
  because it is the one place the escaping invariant is deliberately off, and a reader of the
  "Terminal and UI injection" certification below should know where the exception is rather than
  infer that there is none.
- **The unsalted dedup hash is handled correctly.** It is deliberately kept out of
  `EntrySummary` and out of logs. Worth preserving as an invariant if the summary type ever
  grows a field.

## Areas examined and found clean

Stated explicitly so the absence of findings here is a result rather than an omission.

- **Key management** (`clippo-store/src/key.rs`) — 32 bytes from `getrandom`, never derived;
  `Zeroize` on drop and a redacting `Debug`; the `PRAGMA key` statement built into an
  exactly-sized `Zeroizing<String>` to avoid leaving an unzeroed reallocation behind; the
  wider-than-`0600` key file refused rather than used; `create_new` so a key file is never
  overwritten; and the rule-4 refusal that stops a fallback key file being minted underneath a
  database it cannot open. The reasoning in the module header is correct and the tests match it.
  Scope: this covers clippo's *own* handling of the key. It does not cover `oo7 0.3.3`, the
  Secret Service client that carries the key between `key.rs` and the keyring, or the
  `zbus 4.4.0` it brings with it — see *Dependency posture*.
- **Encryption at rest** — `PRAGMA key` is the first statement on the connection, the read that
  follows is what turns a wrong key into a named error, and
  `what_was_copied_is_not_in_the_file` (`store.rs:1474`) checks every file in the directory, not
  just `history.db`, for the plaintext. `secure_delete` is forced on by SQLCipher and incremental
  vacuum returns freed pages.
- **SQL** — every statement is parameterised. The only `format!` into SQL interpolates the
  `ENTRY_COLUMNS` constant. `STRICT` tables and `PRAGMA foreign_keys = ON` are both set, the
  latter being what makes `ON DELETE CASCADE` actually remove blobs.
- **Terminal and UI injection** — `clippo_core::display::one_line` escapes control characters
  *and* the Cf reordering/invisible ranges that `char::is_control` misses, counts output rather
  than input characters so escaping cannot widen a row, and is applied by the CLI table, the
  applet rows and `--json` (via `escape_invisible`, which is sound because JSON's syntax is
  ASCII). Every path that renders a value applies it; the single exception is `clippo reveal`,
  which is deliberate and is described in the Notes above rather than certified here.
- **Masking** — fixed-width bullet run so the mask does not leak length; grapheme-cluster
  counting so a combining accent or a ZWJ emoji is not cut in half; short values masked
  completely; `mask_prefix + mask_suffix` capped at 16 by the config loader; and masking applied
  *before storage*, so `List` and `Search` have no unmasked preview to return even in principle.
- **Image decoding *in the daemon*** — clipboard PNG/JPEG reaching `clippo-store` is decoded with
  `Limits::max_alloc` set to 256 MB (`clippo-store/src/images.rs:54`), the format is guessed from
  the bytes rather than trusted from the advertised MIME, and a decode failure stores the entry
  without a thumbnail rather than refusing it. Scope of that certification, stated because the
  tree contains two decoder pairs: this covers `image 0.25.10` → `png 0.18.1` / `zune-jpeg 0.5.15`,
  which is the pair that sees clipboard bytes. It does **not** cover `png 0.17.16` /
  `zune-jpeg 0.4.21` (see *Dependency posture*), and it does not cover the applet's own decode —
  `Thumbnails::store` hands the bytes from a `Thumbnail` reply straight to
  `cosmic::widget::image::Handle::from_bytes` (`clippo-applet/src/thumbs.rs:88`), where iced's
  image pipeline decodes them under whatever limits *it* sets. clippo's 256 MB ceiling is applied
  in `clippo-store` and does not travel with the bytes. Against a genuine `clippod` those bytes
  are its own re-encoded 256-pixel PNG and this is uninteresting; under F5 they are attacker-chosen,
  which is the case that makes it worth naming rather than assuming.
- **Paste-path resource handling** — per-flavor size cap with the buffer released rather than
  truncated, per-selection read timeout, per-paste write timeout, a cap on outstanding pastes
  with oldest-first eviction, and non-blocking fds throughout so a receiver that stops reading
  costs one registration rather than the loop.
- **Config parsing** — `deny_unknown_fields`, explicit range checks with named errors, and zero
  kept distinguishable from absent. No path or command is ever taken from the config.
- **Self-echo guard** — keyed on the store's own dedup hash, armed under the same lock as the
  offer, one-shot rather than a denylist, and cleared on `SelectionLost`. I looked specifically
  for a way to make it swallow a real copy and did not find one.
- **CI** — `on: push, pull_request` (not `pull_request_target`), no untrusted input reaching a
  shell, no secrets in the workflow.
- **systemd unit** — `NoNewPrivileges=true`, user unit, no `ExecStartPre` shell. The comment
  explaining why the heavier sandboxing options are off is a reasonable trade-off and is written
  down.

## Dependency posture

The versions clippo asks for directly are current: `openssl-src 300.6.1+3.6.3`, `image 0.25.10`,
`png 0.18.1`, `zune-jpeg 0.5.15`, `regex 1.13.1`, `tokio 1.53.1`, `zbus 5.18.0`,
`wayland-client 0.31.15`. No crate in the tree is at a version I recognise as carrying an open
advisory.

**That list is not the whole tree, and saying so is the point of this section.** `cargo tree -d`
reports 59 crate names present at more than one version, most of them the ordinary consequence of
depending on libcosmic from a git revision (two `bitflags`, four `hashbrown`, three `getrandom`,
three `linux-raw-sys`, and `iced_core`/`cosmic-config` twice each from two spellings of the same
git source — churn, not exposure). Three duplicates are on paths
this audit makes claims about, and an earlier revision of this document listed only the newer
copy of each, which read as "the tree is current" when what was true was "the versions clippo
names in its own manifests are current":

| Crate | Older copy in the lock | Pulled in by | On whose link path |
|---|---|---|---|
| `zbus` (+ `zvariant 4.2.0`, `zbus_names 3.0.0`) | **4.4.0** | `oo7 0.3.3` | **`clippod`**, via `clippo-store` |
| `png` | **0.17.16** | `tiny-skia 0.11.4` ← `iced_tiny_skia`, and `resvg 0.45.1` | `clippo-applet` only |
| `zune-jpeg` (+ `zune-core 0.4.12`) | **0.4.21** | `resvg 0.45.1` | `clippo-applet` only |

Each was confirmed with `cargo tree -i -p <crate>@<version>`.

The `zbus 4.4.0` one deserves naming because of *what* pulls it in. `oo7` is an unconditional
dependency of `clippo-store` (`crates/clippo-store/Cargo.toml:16`) and `clippo-store` is a
dependency of `clippod` alone, so **the daemon links two major versions of zbus**: 5.18.0 for the
interface it exports, 4.4.0 underneath the Secret Service client that carries the database key to
and from the keyring. `oo7` is not otherwise mentioned in this document, including in the
"Key management — found clean" paragraph, which audits `key.rs`'s handling of the key and not the
library that transports it. `default-features = false` with `["tokio", "native_crypto", "tracing"]`
(`Cargo.toml:63`) is a deliberate and sensible narrowing — the workspace comment explains it — but
narrowing features is not the same as auditing the crate, and I did not audit `oo7` or
`zbus 4.4.0`. Treat the key-transport path as *not covered* by this review rather than as cleared
by it.

The two older image decoders are the reverse case: worth listing, but not on a path clipboard
content reaches. They arrive through libcosmic's SVG rasterisation stack (`resvg`/`tiny-skia`),
which in this application renders theme and icon assets shipped with the desktop, not bytes from
the clipboard — `INTERESTING_MIMES` (`clippo-wayland/src/mime.rs:24-32`) has no SVG entry, so
clippo never stores or hands on an SVG. Clipboard PNG and JPEG go through `image 0.25.10` in the
daemon and through iced's image pipeline in the applet, both of which use the 0.18/0.5 copies.
They are recorded here because they are the oldest decoder versions present and because the
previous revision's enumeration silently dropped them.

`cargo tree -d` is the one command that surfaces all of this, and a future revision of this
section should start from its output rather than from the manifests.

One thing to keep an eye on rather than to act on now: `rusqlite 0.32.1` /
`libsqlite3-sys 0.30.1` pins the vendored SQLCipher, which trails upstream SQLite by some
distance. Nothing untrusted reaches the SQL parser — clippo never executes caller-supplied SQL
and never opens a database it did not write — so exposure is close to nil; this is maintenance,
not a finding. Adding `cargo-deny` or `cargo-audit` to the existing CI job would make that
judgement continuous instead of a point-in-time one.

---

## Summary

Ordered by severity; the numbering is discovery order.

| # | Finding | Severity | Reachable by | Confidence |
|---|---|---|---|---|
| F1 | Unauthenticated D-Bus surface; `Paste` injects keystrokes, `Reveal` returns the whole history | Medium | any session-bus peer, incl. sandboxed apps | confirmed (code); sandbox escalation untested on a host |
| F5 | No frontend checks who owns the daemon's name; an impostor sees every search keystroke and controls every row shown | Medium | any session-bus peer, after taking the name | confirmed (code); queue-and-wait acquisition reasoned, not executed |
| F2 | Unbounded flavor count per selection — exact-string dedup vs. normalised interest test | Low–Medium | any Wayland client on the seat | confirmed; impact figures estimated |
| F3 | Detection reads one flavor while the entry carries several | Low | anything that can set a multi-flavor selection | confirmed |
| F4 | Test writes to a predictable `$TMPDIR` path | Low | local user on a shared build host | confirmed |

F2 and F4 are small, self-contained changes and each has the correct helper already present in
the tree (`mime::same`, `tempfile`). F3 is a contained change to one function.

F1 and F5 are the two that need a decision rather than a patch, and they should be decided
together: they are the same missing question — *which peer is on the other end of this
connection* — asked once about callers and once about the daemon they call. F5's first fix step
is the cheaper of the two and I would take it regardless; beyond that the choice is a product one
about whether `Paste` and `Reveal` should be reachable by anything that can open a session bus
connection. That decision is a human's, not mine. What is *not* a decision, and is the thing I
would most want a reader to take from F5, is that there is no flag or policy setting that fixes
it: `clippod` cannot fight for its name, because `ReplaceExisting` only works against an owner
that consented to be replaced, and consenting is strictly worse. See F5's third fix step.

One area is explicitly **not covered**: `oo7` and the `zbus 4.4.0` beneath it, which is the code
that moves the database key between the daemon and the keyring. See *Dependency posture*.
