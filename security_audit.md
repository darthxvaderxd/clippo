# clippo — security audit

Reviewer: Sam (security engineer)
Date: 2026-08-03
Revision audited: `574472c` on `harness/sam-worker/2026-08-03-04-59-48-clippo-security-audit`
(M7 complete — all seven crates present)

This is an audit of the repository as it stands, not a review of a diff. Nothing here has been
fixed; each finding says precisely enough for someone else to fix it.

---

## Scope and method

Every source file in `crates/` was read, plus `res/clippod.service`, `.github/workflows/ci.yml`,
the `justfile`'s install/uninstall recipes, `Cargo.toml` and `Cargo.lock`. The approach was to
trace values from untrusted inputs to sinks rather than to grep for patterns. The untrusted
inputs clippo actually has are:

| Input | Who controls it | Where it enters |
|---|---|---|
| Selection MIME types and blob bytes | any Wayland client on the seat | `clippo-wayland::watch` |
| D-Bus method arguments | any peer on the session bus | `clippo-ipc::service` |
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
— **who is allowed to call the daemon** — plus one input-handling bug on the capture path.

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

---

### F2 — One hostile selection can force an unbounded number of capped flavor reads, because dedup compares MIME strings exactly while the interest test does not

**Severity: Low–Medium.** Denial of service only. Reachable by any Wayland client on the seat,
with no authentication and no user interaction beyond the client setting a selection.
**Confidence: the code defect is confirmed by reading; the peak-resource figures below are
reasoned estimates, not measured.**

**The path.** `is_interesting` normalises before comparing — it strips *all* ASCII whitespace and
compares case-insensitively (`crates/clippo-wayland/src/mime.rs:40-45`, `:66-68`). The dedup in
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

**The right helper is already a dev-dependency and is used everywhere else in the repo.**
`tempfile` is in `[workspace.dependencies]` and `key.rs`, `store.rs` and `daemon.rs` all use
`tempfile::tempdir()`. This one test does not.

**Fix.** Replace with `tempfile::tempdir()`, as the neighbouring tests do; that also removes the
`remove_dir_all` at the end, which currently leaks the directory whenever an assertion above it
fails.

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
  ASCII). `clippo reveal` is the one deliberate exception and says so in its `--help`.
- **Masking** — fixed-width bullet run so the mask does not leak length; grapheme-cluster
  counting so a combining accent or a ZWJ emoji is not cut in half; short values masked
  completely; `mask_prefix + mask_suffix` capped at 16 by the config loader; and masking applied
  *before storage*, so `List` and `Search` have no unmasked preview to return even in principle.
- **Image decoding** — clipboard PNG/JPEG is decoded with `Limits::max_alloc` set to 256 MB
  (`clippo-store/src/images.rs:54`), the format is guessed from the bytes rather than trusted from
  the advertised MIME, and a decode failure stores the entry without a thumbnail rather than
  refusing it.
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

`Cargo.lock` is current across the board: `openssl-src 300.6.1+3.6.3`, `image 0.25.10`,
`png 0.18.1`, `zune-jpeg 0.5.15`, `regex 1.13.1`, `tokio 1.53.1`, `zbus 5.18.0`,
`wayland-client 0.31.15`. No crate in the tree is at a version I recognise as carrying an open
advisory.

One thing to keep an eye on rather than to act on now: `rusqlite 0.32.1` /
`libsqlite3-sys 0.30.1` pins the vendored SQLCipher, which trails upstream SQLite by some
distance. Nothing untrusted reaches the SQL parser — clippo never executes caller-supplied SQL
and never opens a database it did not write — so exposure is close to nil; this is maintenance,
not a finding. Adding `cargo-deny` or `cargo-audit` to the existing CI job would make that
judgement continuous instead of a point-in-time one.

---

## Summary

| # | Finding | Severity | Reachable by | Confidence |
|---|---|---|---|---|
| F1 | Unauthenticated D-Bus surface; `Paste` injects keystrokes, `Reveal` returns the whole history | Medium | any session-bus peer, incl. sandboxed apps | confirmed (code); sandbox escalation untested on a host |
| F2 | Unbounded flavor count per selection — exact-string dedup vs. normalised interest test | Low–Medium | any Wayland client on the seat | confirmed; impact figures estimated |
| F3 | Detection reads one flavor while the entry carries several | Low | anything that can set a multi-flavor selection | confirmed |
| F4 | Test writes to a predictable `$TMPDIR` path | Low | local user on a shared build host | confirmed |

F2 and F4 are small, self-contained changes and each has the correct helper already present in
the tree (`mime::same`, `tempfile`). F3 is a contained change to one function. F1 is the one that
needs a decision rather than a patch — the fix is partly documentation and partly a product
choice about whether `Paste` and `Reveal` should be callable by anything that can reach the
session bus. That decision is a human's, not mine.
