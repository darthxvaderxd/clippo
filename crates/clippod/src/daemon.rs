//! Everything the daemon does, with no bus in sight.
//!
//! [`Daemon`] owns the store, the preview cache and the paused flag, and
//! implements [`ClippoBackend`] — the trait `clippo-ipc` serves. The zbus
//! machinery lives on the other side of that trait, which is what lets the
//! tests at the bottom of this file exercise `List`, `Search`, `Delete`,
//! `Clear` and the capture path against a real temporary database without a
//! session bus to talk to.
//!
//! # The invariant
//!
//! Every method that changes the history does the same three things in the same
//! order, through [`Daemon::commit`]:
//!
//! 1. write to the store,
//! 2. reload the cache from the store,
//! 3. emit `HistoryChanged`.
//!
//! Step 2 is what keeps search honest — see [`crate::cache`] — and step 3 is
//! what means no frontend ever polls. Doing them in one place is what stops a
//! new mutation from being added later with only one of the three.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use clippo_core::{Chord, EntryId, EntryKind, Flavor, NewEntry, SecretsConfig, Timestamp};
use clippo_ipc::{ClippoBackend, ClippoInterface, EntrySummary};
use clippo_store::{dedup, is_offerable, Store, StoreError};
use clippo_wayland::{Clipboard, Keystrokes, Selection};
use tokio::sync::{Mutex, MutexGuard};
use tracing::{debug, error, info, warn};
use zbus::fdo;
use zbus::object_server::SignalEmitter;

use crate::cache::{self, PreviewCache};
use crate::echo::EchoGuard;
use crate::preview;

/// How long `Paste` waits between copying and pressing the shortcut.
///
/// Long enough for the compositor to have taken keyboard focus off the applet's
/// picker and put it back on the window underneath, because a keystroke sent
/// before that happens goes to the picker. There is no event this side of the
/// bus that says focus has settled — see [`Daemon::paste`] — so it is a number,
/// and a generous one: 120 ms is imperceptible next to the act of choosing an
/// entry, and being early means pasting into the wrong surface.
const FOCUS_SETTLE: std::time::Duration = std::time::Duration::from_millis(120);

/// The mutable half, behind one lock.
///
/// The store and the cache are locked together on purpose: they are two
/// representations of one history, and a reader that could see the cache
/// between a write and its reload would see a search index that disagrees with
/// the database.
///
/// `rusqlite::Connection` is `Send` but not `Sync`, so a lock of some kind is
/// required regardless. Queries against a local SQLite file are microseconds,
/// so it is never held long enough to be worth `spawn_blocking` — and it is
/// never held across an `await`, which is why [`Daemon::commit`] takes the
/// guard by value and drops it before it emits anything.
struct State {
    store: Store,
    cache: PreviewCache,
    /// In here, not beside the store, because arming it and the write that
    /// earns it are one operation: `Copy` must not be able to put an entry on
    /// the clipboard between another task arming the guard and that task
    /// offering. Holding one lock across both makes that impossible rather
    /// than unlikely. See [`crate::echo`].
    echo: EchoGuard,
}

/// The clipboard daemon.
///
/// Shared as an `Arc` between the served object and the capture task; every
/// method takes `&self`.
pub struct Daemon {
    state: Mutex<State>,
    /// Outside the lock, so `Paused()` answers instantly and `SetPaused(true)`
    /// takes effect even while a large image capture holds the store.
    paused: AtomicBool,
    /// The detection and masking knobs, read once at startup like the rest of
    /// the config. Not behind the lock: capture reads it and nothing writes it.
    secrets: SecretsConfig,
    signals: Signals,
    /// Where `Copy` puts an entry. Filled in once, after the Wayland watcher
    /// has started — see [`Daemon::connect_clipboard`].
    clipboard: OnceLock<Arc<dyn Clipboard>>,
    /// What `Paste` presses once the entry is on the clipboard, and the
    /// combination to press. Filled in once, like the clipboard, and absent on
    /// a compositor that offers no way to synthesise keys — see
    /// [`Daemon::connect_keystrokes`].
    keystrokes: OnceLock<Arc<dyn Keystrokes>>,
    /// The user's `paste_shortcut`, read once at startup with the rest of the
    /// config.
    paste_shortcut: Chord,
    /// The user's `auto_paste`. `false` makes `Paste` stop after the copy.
    auto_paste: bool,
}

impl Daemon {
    /// Build a daemon around an open store, filling the cache from it.
    ///
    /// The cache is built here rather than lazily so that a database that will
    /// not read fails at startup, in the journal, rather than on a user's first
    /// `List`.
    ///
    /// `secrets` is the `[secrets]` table of the user's config, which decides
    /// how much of a masked entry is shown and whether the entropy rule runs at
    /// all. It is read once, here, for the reason the config module gives:
    /// re-deriving it mid-run would leave a history captured under two
    /// different sets of rules.
    pub fn new(
        store: Store,
        signals: Signals,
        secrets: SecretsConfig,
        paste_shortcut: Chord,
        auto_paste: bool,
    ) -> Result<Arc<Self>, StoreError> {
        let mut state = State {
            store,
            cache: PreviewCache::new(),
            echo: EchoGuard::default(),
        };
        reload(&mut state)?;
        info!(
            entries = state.cache.entry_count(),
            "clippo loaded the clipboard history"
        );
        Ok(Arc::new(Self {
            state: Mutex::new(state),
            paused: AtomicBool::new(false),
            secrets,
            signals,
            clipboard: OnceLock::new(),
            keystrokes: OnceLock::new(),
            paste_shortcut,
            auto_paste,
        }))
    }

    /// Give the daemon the clipboard it will serve `Copy` from.
    ///
    /// Separate from [`Daemon::new`] because of the order `main` runs in: the
    /// D-Bus object is exported before the well-known name is taken, and the
    /// Wayland watcher only starts after it, so that a second `clippod` dies at
    /// the name request without ever having touched the compositor. The daemon
    /// therefore exists for a moment before the clipboard does, and `Copy`
    /// during that moment says so rather than pretending.
    ///
    /// Calling this twice is a bug, and it is ignored rather than obeyed: the
    /// second clipboard would leave the first watcher owning a selection
    /// nothing could replace.
    pub fn connect_clipboard(&self, clipboard: Arc<dyn Clipboard>) {
        if self.clipboard.set(clipboard).is_err() {
            error!("clippo was given a second clipboard to serve Copy from; ignoring it");
        }
    }

    /// Give the daemon the keyboard `Paste` presses the shortcut on.
    ///
    /// Optional in a way the clipboard is not. Without it `Paste` is `Copy`,
    /// which is a working clipboard manager and what every user had before this
    /// existed, so a compositor with no `zwp_virtual_keyboard_v1` is a reason
    /// to log once at startup rather than to refuse to run. `main` does not
    /// call this at all in that case.
    pub fn connect_keystrokes(&self, keystrokes: Arc<dyn Keystrokes>) {
        if self.keystrokes.set(keystrokes).is_err() {
            error!("clippo was given a second keyboard to serve Paste from; ignoring it");
        }
    }

    /// Another application took the clipboard from us.
    ///
    /// The self-echo guard goes with it: whatever copy-back it was armed for is
    /// not coming back now that somebody else owns the selection, and a guard
    /// left armed would swallow the next real copy of that content.
    pub async fn selection_lost(&self) {
        let mut state = self.state.lock().await;
        if state.echo.clear() {
            debug!(
                "another application took the clipboard before clippo's own copy-back came \
                 round; the self-echo guard was cleared"
            );
        }
    }

    /// Record a selection the Wayland watcher captured.
    ///
    /// Takes the selection by value: its flavors are the copied bytes, up to
    /// `max_flavor_bytes` of them, and cloning a screenshot to store it would
    /// double the peak memory of every image copy.
    ///
    /// Nothing here returns a failure to anybody, because there is nobody to
    /// return it to — this runs in the capture task, not in a method call. Each
    /// outcome is logged instead, at the level that says whether the user lost
    /// anything.
    pub async fn capture(&self, selection: Selection) {
        self.capture_at(selection, Timestamp::now()).await;
    }

    /// [`Daemon::capture`] against a clock the caller supplies.
    ///
    /// The capture time is a parameter for the same reason
    /// `Store::enforce_retention` takes one: `last_used_at` has millisecond
    /// resolution and decides the order of the whole history, so a test that
    /// captured three things inside one millisecond would be asserting on a tie
    /// broken by insertion order rather than on the ordering rule.
    async fn capture_at(&self, selection: Selection, now: Timestamp) {
        for dropped in &selection.dropped {
            warn!(
                mime = %dropped.mime,
                reason = %dropped.reason,
                "clippo could not read one flavor of a copy"
            );
        }

        if selection.is_empty() {
            debug!(
                advertised = selection.advertised.len(),
                "clippo captured nothing storable from a copy"
            );
            return;
        }

        let Some(new) = new_entry(selection, now, &self.secrets) else {
            debug!("a copy carried no flavor clippo can store, so it was skipped");
            return;
        };
        let kind = new.kind;
        let sensitive = new.sensitive;

        let insertion = {
            let mut state = self.state.lock().await;

            // Before the paused check, so that a copy-back made while paused
            // still spends the guard it armed. Left armed, it would swallow the
            // next real copy of that same content instead. See [`crate::echo`].
            if state.echo.is_echo(&new.hash) {
                debug!(
                    kind = %kind,
                    "ignoring clippo's own copy-back rather than recording it as a copy"
                );
                return;
            }
            if self.paused.load(Ordering::Relaxed) {
                debug!(
                    flavors = new.flavors.len(),
                    "clippo is paused, so this copy was not recorded"
                );
                return;
            }

            match state.store.insert(&new) {
                Ok(insertion) => {
                    // Retention runs inside `insert`, so the reload here is
                    // also what takes an evicted entry out of search results.
                    if let Err(error) = reload(&mut state) {
                        error!(
                            error = %error,
                            "clippo stored a copy but could not reload its preview cache; \
                             search and the list may be a copy behind until the next change"
                        );
                    }
                    insertion
                }
                Err(error) => {
                    // `ImageTooLarge` is the expected one and names its own
                    // knob; the rest are a database that would not write.
                    error!(error = %error, "clippo did not store a copy");
                    return;
                }
            }
        };

        info!(
            id = insertion.id().get(),
            kind = %kind,
            sensitive,
            repeat = insertion.was_deduplicated(),
            "clippo recorded a copy"
        );
        self.signals.history_changed().await;
    }

    /// [`ClippoBackend::copy`] against a clock the caller supplies, for the
    /// same reason [`Daemon::capture_at`] takes one.
    async fn copy_at(&self, id: EntryId, now: Timestamp) -> fdo::Result<()> {
        let clipboard = self.clipboard.get().ok_or_else(|| {
            // Only reachable in the moment between the object being exported
            // and the watcher starting; see `Daemon::connect_clipboard`.
            warn!(
                id = id.get(),
                "Copy arrived before clippo had a clipboard to put it on"
            );
            fdo::Error::Failed(
                "clippo is still starting up and has no clipboard to put an entry on yet; \
                 try again"
                    .to_owned(),
            )
        })?;

        let mut state = self.state.lock().await;
        let stored = state
            .store
            .get(id)
            .map_err(|error| failed("copy", error))?
            .ok_or_else(|| no_such_entry(id))?;

        // Every stored flavor except the ones clippo derived for itself. The
        // thumbnail is the one that matters: advertising `image/png;clippo-thumb`
        // would let an application negotiate it and paste a 256-pixel version of
        // the user's screenshot — a paste that succeeds with the wrong picture,
        // which is worse than one that fails. `clippo-store` owns the list, next
        // to the MIME it mirrors.
        let offerable: Vec<Flavor> = stored
            .flavors
            .into_iter()
            .filter(|flavor| is_offerable(&flavor.mime))
            .collect();
        if offerable.is_empty() {
            return Err(fdo::Error::Failed(format!(
                "entry {id} has nothing clippo can put on the clipboard"
            )));
        }
        let flavors = offerable.len();

        // Armed before the flavors go anywhere near the compositor: the echo
        // can be on the capture channel before `offer` returns.
        state.echo.arm(&stored.entry.hash);
        if let Err(error) = clipboard.offer(offerable) {
            state.echo.clear();
            error!(id = id.get(), error = %error, "clippo could not take the clipboard");
            return Err(fdo::Error::Failed(format!(
                "clippo could not put entry {id} on the clipboard: {error}. \
                 The history is unchanged"
            )));
        }

        // The paste has happened as far as anyone outside can tell, so a
        // failure here is a bookkeeping failure, not a failed `Copy`. Reporting
        // it as one would have a user retry something that worked.
        let touched = match state.store.touch(id, now) {
            Ok(touched) => touched,
            Err(error) => {
                error!(
                    id = id.get(),
                    error = %error,
                    "clippo put an entry on the clipboard but could not move it to the front"
                );
                false
            }
        };
        self.commit(state, touched).await;

        info!(
            id = id.get(),
            flavors, "clippo put an entry on the clipboard"
        );
        Ok(())
    }

    /// Whether new copies are being recorded.
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Apply the retention limits to a history that has been sitting idle.
    ///
    /// Retention normally runs inside `Store::insert`, so a daemon nobody has
    /// copied anything into since Friday has not swept: entries can pass the
    /// age limit while it idles. This is the pass that covers the gap, at
    /// startup.
    ///
    /// It is a *write*, which is why it belongs here rather than in `main`'s
    /// setup: a second `clippod` must not delete rows out of a database the
    /// running one is serving from, so this is called only once the well-known
    /// name is ours. See [`crate::main`]'s ordering.
    pub async fn sweep_retention(&self, now: Timestamp) -> Result<(), StoreError> {
        let mut state = self.state.lock().await;
        let swept = state.store.enforce_retention(now)?;
        if !swept.is_empty() {
            info!(
                expired = swept.expired,
                over_capacity = swept.over_capacity,
                "clippo dropped entries that had fallen outside its retention limits"
            );
        }
        // Through `commit` like every other mutation, so the cache cannot be
        // left holding entries the sweep has just deleted.
        self.commit(state, !swept.is_empty()).await;
        Ok(())
    }

    /// Run a mutation, then reload the cache and announce the change.
    ///
    /// `changed` says whether anything actually happened: a `Delete` of an id
    /// that is not there, or a `Clear` of an empty history, is not a change and
    /// must not wake every frontend up to re-read an identical list.
    ///
    /// The reload failing is logged rather than returned. The caller's write
    /// has already committed — reporting failure would tell them their delete
    /// did not happen when it did.
    ///
    /// Takes the guard by value so it can drop it before announcing. Emitting
    /// under the lock would make every read wait on a write to the bus socket,
    /// and the announcement is about a change that has already happened — there
    /// is nothing left for the lock to protect.
    async fn commit(&self, mut state: MutexGuard<'_, State>, changed: bool) {
        if !changed {
            return;
        }
        if let Err(error) = reload(&mut state) {
            error!(
                error = %error,
                "clippo changed the history but could not reload its preview cache; \
                 search and the list may be stale until the next change"
            );
        }
        drop(state);
        self.signals.history_changed().await;
    }
}

/// Refill the cache from the store. See [`crate::cache`] for why wholesale.
fn reload(state: &mut State) -> Result<(), StoreError> {
    // `usize::MAX` is "no limit": the cache mirrors the whole history, whose
    // size retention already bounds.
    let entries = state.store.list(usize::MAX, 0)?;
    state.cache.replace(entries);
    Ok(())
}

/// The `NewEntry` a captured selection becomes, or `None` if it is not storable.
///
/// `None` means the selection carried no flavor with content of its own — a
/// bare `x-kde-passwordManagerHint`, say. Such a copy has no
/// [`EntryKind`] and therefore no canonical flavor, so it has no
/// `entries.hash`, so there is no identity to store it under.
///
/// Detection and masking happen here, once, because this is where the whole
/// value is: the marker flavor may never be stored, and the entropy rule needs
/// the value rather than the 120 characters of it a preview keeps. What goes
/// into the database is already masked — see [`crate::preview`].
fn new_entry(selection: Selection, now: Timestamp, secrets: &SecretsConfig) -> Option<NewEntry> {
    // Read before the flavors are moved out.
    let hinted = selection.has_password_manager_hint();
    let flavors = selection.flavors;

    let kind = EntryKind::for_flavors(&flavors)?;
    let hash = dedup::hash(kind, &flavors)?;
    let described = preview::describe(kind, &flavors, hinted, secrets);

    if let Some(signal) = described.signal {
        // The rule, never the value: this is the line that answers "why is my
        // UUID masked?" from the journal rather than from a rebuild.
        debug!(
            rule = %signal,
            kind = %kind,
            "clippo is treating a copy as a secret"
        );
    }

    Some(NewEntry {
        created_at: now,
        kind,
        preview: described.preview,
        hash,
        // The store ORs this flag on a repeat copy, so a later, better-informed
        // capture of the same bytes can only ever raise it.
        sensitive: described.sensitive,
        flavors,
    })
}

#[async_trait]
impl ClippoBackend for Daemon {
    async fn list(&self, limit: u32, offset: u32) -> fdo::Result<Vec<EntrySummary>> {
        let state = self.state.lock().await;
        Ok(state.cache.page(limit as usize, offset as usize))
    }

    /// The history, fuzzy-matched against `query`.
    ///
    /// The length check happens **before** the lock, deliberately: an
    /// over-long query is refused without the caller ever holding up the
    /// frontends that are using the daemon properly. See
    /// [`cache::MAX_QUERY_CHARS`].
    async fn search(&self, query: &str, limit: u32) -> fdo::Result<Vec<EntrySummary>> {
        cache::check_query(query).map_err(|error| fdo::Error::InvalidArgs(error.to_string()))?;

        let mut state = self.state.lock().await;
        state
            .cache
            .search(query, limit as usize)
            .map_err(|error| fdo::Error::InvalidArgs(error.to_string()))
    }

    /// Put an entry back on the clipboard, and move it to the front.
    ///
    /// Everything below happens under one lock, which is what makes the
    /// self-echo guard sound: the compositor announces the new selection to
    /// every data-control client, clippo included, so the capture task can be
    /// holding this entry's own flavors before this method returns. It cannot
    /// act on them until the guard is armed, because it needs this same lock to
    /// look at them.
    ///
    /// `last_used_at` moves for the same reason a repeat copy moves it: pasting
    /// something out of the history is a use of it, and a user who pasted an
    /// entry expects to find it at the top next time. The guard is what stops
    /// that from happening twice — once here, once when the copy-back comes
    /// round as a capture.
    async fn copy(&self, id: i64) -> fdo::Result<()> {
        self.copy_at(EntryId::new(id), Timestamp::now()).await
    }

    async fn paste(&self, id: i64) -> fdo::Result<bool> {
        // The copy half first, and its failure is the whole call's failure:
        // pressing the paste shortcut with the old clipboard still loaded would
        // paste the wrong thing, which is worse than pasting nothing.
        self.copy_at(EntryId::new(id), Timestamp::now()).await?;

        // Checked after the copy rather than before it, so that turning
        // `auto_paste` off changes exactly one thing: whether a key is pressed.
        // The entry still reaches the clipboard, the history still reorders,
        // and `Paste` still succeeds — it is `Copy` with the second half
        // switched off, not a different member.
        if !self.auto_paste {
            debug!(id, "auto_paste is off; the entry is on the clipboard");
            return Ok(false);
        }

        let Some(keystrokes) = self.keystrokes.get().cloned() else {
            // Logged at debug rather than warn: on a compositor without the
            // protocol this is every `Paste`, and `main` has already said so
            // once, at startup, where it is useful.
            debug!(
                id,
                "clippo has no way to press the paste shortcut; the entry is on the clipboard"
            );
            return Ok(false);
        };

        // The picker is still on screen when this call arrives — the applet
        // sends `Paste` and closes in the same breath, because it cannot send
        // it *after* closing without keeping a surface alive to send from. So
        // the keystroke has to wait for the compositor to take focus off the
        // picker and give it back to the window underneath, and there is no
        // event here that says it has: this daemon is not the applet, has no
        // surface, and sees none of that.
        //
        // Hence a delay, which is exactly the kind of thing that is wrong on
        // somebody else's machine. It is deliberately more than it usually
        // needs to be; the cost of being early is pasting into the picker.
        tokio::time::sleep(FOCUS_SETTLE).await;

        let shortcut = self.paste_shortcut.clone();
        // `send` sleeps while the key is held and then blocks on a round trip,
        // so it does not belong on a runtime worker.
        let sent = tokio::task::spawn_blocking(move || keystrokes.send(&shortcut)).await;

        match sent {
            Ok(Ok(())) => {
                info!(id, shortcut = %self.paste_shortcut, "clippo pressed the paste shortcut");
                return Ok(true);
            }
            // Reported to the journal and not to the caller, deliberately. The
            // entry is on the clipboard, so the user can finish this by hand,
            // and an error here would have a frontend report a failed `Paste`
            // for something that half worked — the half that matters most.
            Ok(Err(error)) => {
                warn!(
                    id,
                    error = %error,
                    "clippo put the entry on the clipboard but could not press the paste shortcut"
                );
            }
            Err(error) => {
                warn!(
                    id,
                    error = %error,
                    "clippo's keystroke task did not finish; the entry is still on the clipboard"
                );
            }
        }
        Ok(false)
    }

    async fn delete(&self, id: i64) -> fdo::Result<()> {
        let id = EntryId::new(id);
        let mut state = self.state.lock().await;
        let deleted = state
            .store
            .delete(id)
            .map_err(|error| failed("delete", error))?;
        self.commit(state, deleted).await;
        if !deleted {
            return Err(no_such_entry(id));
        }
        info!(id = id.get(), "clippo deleted an entry");
        Ok(())
    }

    async fn pin(&self, id: i64, pinned: bool) -> fdo::Result<()> {
        let id = EntryId::new(id);
        let mut state = self.state.lock().await;
        let updated = state
            .store
            .set_pinned(id, pinned)
            .map_err(|error| failed("pin", error))?;
        self.commit(state, updated).await;
        if !updated {
            return Err(no_such_entry(id));
        }
        info!(id = id.get(), pinned, "clippo changed an entry's pin");
        Ok(())
    }

    async fn clear(&self, include_pinned: bool) -> fdo::Result<()> {
        let mut state = self.state.lock().await;
        let cleared = state
            .store
            .clear(include_pinned)
            .map_err(|error| failed("clear", error))?;
        self.commit(state, cleared > 0).await;
        Ok(())
    }

    /// The whole stored value of one entry — the only member that returns one.
    ///
    /// Loading the flavors from the store rather than caching them is part of
    /// that: the cache holds previews, so there is nothing in memory for a
    /// stray `{cache:?}` to leak, and a revealed value lives only as long as
    /// this call.
    async fn reveal(&self, id: i64) -> fdo::Result<String> {
        let id = EntryId::new(id);
        let state = self.state.lock().await;
        let stored = state
            .store
            .get(id)
            .map_err(|error| failed("reveal", error))?
            .ok_or_else(|| no_such_entry(id))?;

        preview::reveal(stored.entry.kind, &stored.flavors).ok_or_else(|| {
            fdo::Error::NotSupported(format!(
                "entry {id} is {}, which has no text to reveal",
                stored.entry.kind
            ))
        })
    }

    /// The stored thumbnail, or a reason there is not one.
    ///
    /// Read straight out of the flavors — nothing is decoded or scaled here.
    /// The whole point of the derived `image/png;clippo-thumb` row is that
    /// drawing an image row costs one blob read, so generating a missing
    /// thumbnail on demand would quietly reintroduce the full-size decode this
    /// member exists to avoid. An image stored without one is reported as such
    /// and the frontend draws a placeholder.
    ///
    /// "One blob read" is why this goes through
    /// [`Store::entry`][clippo_store::Store::entry] and
    /// [`Store::thumbnail`][clippo_store::Store::thumbnail] rather than
    /// `Store::get`. `get` returns every flavor, so it reads and decrypts the
    /// full-size PNG in order to hand back the small one beside it — the decode
    /// would still be avoided, the read and the SQLCipher decrypt would not,
    /// and the claim above would be false for exactly the entries it is about.
    async fn thumbnail(&self, id: i64) -> fdo::Result<Vec<u8>> {
        let id = EntryId::new(id);
        let state = self.state.lock().await;
        let entry = state
            .store
            .entry(id)
            .map_err(|error| failed("thumbnail", error))?
            .ok_or_else(|| no_such_entry(id))?;

        if entry.kind != EntryKind::Image {
            return Err(fdo::Error::NotSupported(format!(
                "entry {id} is {}, which has no thumbnail",
                entry.kind
            )));
        }

        state
            .store
            .thumbnail(id)
            .map_err(|error| failed("thumbnail", error))?
            .ok_or_else(|| {
                fdo::Error::NotSupported(format!(
                    "entry {id} is an image stored without a thumbnail; \
                     it was too large or could not be decoded at capture"
                ))
            })
    }

    async fn set_paused(&self, paused: bool) -> fdo::Result<()> {
        // `swap` rather than a load and a store: two frontends toggling at once
        // must not both decide they were the one that changed it.
        if self.paused.swap(paused, Ordering::Relaxed) != paused {
            info!(
                paused,
                "clippo {} recording new copies",
                if paused { "stopped" } else { "resumed" }
            );
        }
        Ok(())
    }

    async fn paused(&self) -> fdo::Result<bool> {
        Ok(self.is_paused())
    }
}

/// Where `HistoryChanged` goes.
///
/// A daemon under test has no bus, and a signal that silently went nowhere
/// would make "is it emitted on every mutation?" untestable. So emission is a
/// value: [`Signals::bus`] sends, `Signals::discard` (tests only) does not, and
/// both keep the count.
pub struct Signals {
    emitter: Option<SignalEmitter<'static>>,
    emitted: AtomicU64,
}

impl Signals {
    /// Emit on the session bus.
    pub fn bus(emitter: SignalEmitter<'static>) -> Self {
        Self {
            emitter: Some(emitter),
            emitted: AtomicU64::new(0),
        }
    }

    /// Count emissions and send nothing, for tests, which have no bus.
    #[cfg(test)]
    pub fn discard() -> Self {
        Self {
            emitter: None,
            emitted: AtomicU64::new(0),
        }
    }

    /// How many times `HistoryChanged` has been emitted.
    #[cfg(test)]
    pub fn emitted(&self) -> u64 {
        self.emitted.load(Ordering::Relaxed)
    }

    /// Announce that the history changed.
    ///
    /// A failed emission is logged, not propagated: the mutation that prompted
    /// it has already happened, and a frontend that missed the signal recovers
    /// on its next call, whereas turning it into a failed `Delete` would leave
    /// a user retrying something that already worked.
    async fn history_changed(&self) {
        // The count is what an applet that is not updating is measured against:
        // signals emitted here versus signals the applet acted on says which
        // side of the bus the problem is.
        let emitted = self.emitted.fetch_add(1, Ordering::Relaxed) + 1;
        debug!(emitted, "clippo announced HistoryChanged");
        let Some(emitter) = &self.emitter else {
            return;
        };
        if let Err(error) = ClippoInterface::history_changed(emitter).await {
            warn!(
                error = %error,
                "clippo could not emit HistoryChanged; frontends will update on their next call"
            );
        }
    }
}

/// A store failure, as it crosses the bus.
///
/// The message is the error's own, which names paths, MIME types and limits but
/// never clipboard content — see `clippo-store`'s `StoreError`.
fn failed(member: &'static str, error: StoreError) -> fdo::Error {
    error!(member, error = %error, "clippo could not answer a D-Bus call");
    fdo::Error::Failed(format!("clippo could not {member}: {error}"))
}

/// The error for an id that is not in the history.
///
/// `InvalidArgs` rather than `Failed`: the call was well formed and the daemon
/// is healthy, the argument just names an entry that is not there — usually
/// because something else deleted it between a `List` and a click.
fn no_such_entry(id: EntryId) -> fdo::Error {
    fdo::Error::InvalidArgs(format!("there is no clipboard history entry with id {id}"))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::atomic::AtomicI64;
    use std::sync::Mutex as StdMutex;

    use clippo_core::Flavor;
    use clippo_store::{Key, Retention, THUMBNAIL_MIME};
    use clippo_wayland::{OfferError, SelectionKind, PASSWORD_MANAGER_HINT_MIME};
    use tempfile::TempDir;

    use super::*;

    /// The default paste shortcut, for the daemons these tests build.
    ///
    /// Which combination it is does not matter to anything here — no test
    /// presses a key on a real compositor — but it has to be *a* chord, and
    /// taking the documented default keeps these from asserting against a value
    /// no user would ever have.
    fn paste_shortcut() -> Chord {
        clippo_core::config::DEFAULT_PASTE_SHORTCUT
            .parse()
            .expect("the default paste shortcut parses")
    }

    /// The compositor, in process.
    ///
    /// This is the seam DESIGN.md's risk table asks the self-echo loop to be
    /// tested against: it keeps what it was handed, and [`Fixture::echo`] feeds
    /// it back exactly as cosmic-comp would — a `selection` event carrying the
    /// flavors clippo just set, delivered to clippo's own watcher.
    #[derive(Debug, Default)]
    struct FakeClipboard {
        offers: StdMutex<Vec<Vec<Flavor>>>,
    }

    /// The keyboard, in process: it records what it was asked to press.
    ///
    /// The seam that makes `Paste` testable without a compositor, and the
    /// reason [`Keystrokes`] is a trait. What these tests can check is the part
    /// that is clippo's own logic — that the copy happens first, that it
    /// happens even when the keystroke cannot, and that the chord pressed is
    /// the configured one — and not whether cosmic-comp honours the keystroke,
    /// which needs a compositor and is ROADMAP Verification §6's job.
    #[derive(Debug, Default)]
    struct FakeKeyboard {
        pressed: StdMutex<Vec<String>>,
        /// Set to make every press fail, standing in for a compositor that
        /// took the protocol away mid-run.
        broken: bool,
    }

    impl FakeKeyboard {
        fn presses(&self) -> Vec<String> {
            self.pressed.lock().expect("not poisoned").clone()
        }
    }

    impl Keystrokes for FakeKeyboard {
        fn send(&self, chord: &Chord) -> Result<(), clippo_wayland::KeyError> {
            if self.broken {
                return Err(clippo_wayland::KeyError::Unsupported);
            }
            self.pressed
                .lock()
                .expect("not poisoned")
                .push(chord.to_string());
            Ok(())
        }
    }

    impl FakeClipboard {
        /// The flavors of the most recent copy-back.
        fn last_offer(&self) -> Vec<Flavor> {
            self.offers
                .lock()
                .expect("the fake clipboard's lock")
                .last()
                .cloned()
                .expect("something should have been put on the clipboard")
        }

        fn offer_count(&self) -> usize {
            self.offers.lock().expect("the fake clipboard's lock").len()
        }
    }

    impl Clipboard for FakeClipboard {
        fn offer(&self, flavors: Vec<Flavor>) -> Result<(), OfferError> {
            if flavors.is_empty() {
                return Err(OfferError::NothingToOffer);
            }
            self.offers
                .lock()
                .expect("the fake clipboard's lock")
                .push(flavors);
            Ok(())
        }
    }

    /// A clipboard that will not take anything, for the failure path.
    #[derive(Debug)]
    struct BrokenClipboard;

    impl Clipboard for BrokenClipboard {
        fn offer(&self, _flavors: Vec<Flavor>) -> Result<(), OfferError> {
            Err(OfferError::WatcherStopped)
        }
    }

    /// A daemon over a temp database, with signals counted rather than sent.
    struct Fixture {
        daemon: Arc<Daemon>,
        clipboard: Arc<FakeClipboard>,
        /// Hands out a fresh millisecond per capture. `last_used_at` has
        /// millisecond resolution and decides the order of the history, so
        /// captures made against the real clock inside one millisecond would
        /// order by a tie-break rather than by the rule under test.
        clock: AtomicI64,
        /// Removed on drop, so it has to outlive the daemon.
        _dir: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with_secrets(SecretsConfig::default())
        }

        /// The same, with the `[secrets]` table the caller wants — the entropy
        /// knob is the only thing any test needs to vary.
        fn with_secrets(secrets: SecretsConfig) -> Self {
            Self::build(secrets, true)
        }

        /// A daemon with `auto_paste = false`, for the one thing that knob
        /// changes.
        fn without_auto_paste() -> Self {
            Self::build(SecretsConfig::default(), false)
        }

        fn build(secrets: SecretsConfig, auto_paste: bool) -> Self {
            let dir = tempfile::tempdir().expect("a temp dir for the test database");
            let key = Key::random().expect("a key for the test database");
            let store =
                Store::open(dir.path().join("history.db"), &key).expect("the store should open");
            let daemon = Daemon::new(
                store,
                Signals::discard(),
                secrets,
                paste_shortcut(),
                auto_paste,
            )
            .expect("the daemon should start");
            let clipboard = Arc::new(FakeClipboard::default());
            daemon.connect_clipboard(Arc::clone(&clipboard) as Arc<dyn Clipboard>);
            Self {
                daemon,
                clipboard,
                clock: AtomicI64::new(1_000),
                _dir: dir,
            }
        }

        /// The next millisecond, so every write has a distinct `last_used_at`.
        fn tick(&self) -> Timestamp {
            Timestamp::from_unix_millis(self.clock.fetch_add(1, Ordering::Relaxed))
        }

        /// Capture each text one millisecond after the last.
        async fn capture(&self, texts: &[&str]) {
            for text in texts {
                self.daemon
                    .capture_at(text_selection(text), self.tick())
                    .await;
            }
        }

        /// A copy out of a password manager: the text, plus the marker flavor
        /// KeePassXC attaches to it.
        async fn capture_password(&self, password: &str) {
            self.daemon
                .capture_at(
                    Selection {
                        kind: SelectionKind::Clipboard,
                        advertised: vec![
                            "text/plain".to_owned(),
                            PASSWORD_MANAGER_HINT_MIME.to_owned(),
                        ],
                        flavors: vec![
                            Flavor::new("text/plain", password),
                            Flavor::new(PASSWORD_MANAGER_HINT_MIME, "secret"),
                        ],
                        dropped: Vec::new(),
                    },
                    self.tick(),
                )
                .await;
        }

        /// `Copy(id)`, on the same fresh-millisecond clock.
        async fn copy(&self, id: EntryId) -> fdo::Result<()> {
            self.daemon.copy_at(id, self.tick()).await
        }

        /// Deliver the last copy-back back to the daemon, as the compositor
        /// does: taking the selection makes it announce the new one to every
        /// data-control client, clippo's own watcher included.
        async fn echo(&self) {
            let selection = selection_of(&self.clipboard.last_offer());
            self.daemon.capture_at(selection, self.tick()).await;
        }

        async fn previews(&self) -> Vec<String> {
            previews(&self.daemon).await
        }

        /// The id of the one entry whose preview matches, for readable tests.
        async fn id_of(&self, preview: &str) -> EntryId {
            let found = self.daemon.search(preview, 1).await.expect("search");
            EntryId::new(found.first().expect("a matching entry").id)
        }

        async fn last_used_at(&self, id: EntryId) -> Timestamp {
            self.daemon
                .state
                .lock()
                .await
                .store
                .get(id)
                .expect("the store should read")
                .expect("the entry should still be there")
                .entry
                .last_used_at
        }

        fn emitted(&self) -> u64 {
            self.daemon.signals.emitted()
        }
    }

    fn text_selection(text: &str) -> Selection {
        Selection {
            kind: SelectionKind::Clipboard,
            advertised: vec!["text/plain;charset=utf-8".to_owned()],
            flavors: vec![Flavor::new("text/plain;charset=utf-8", text)],
            dropped: Vec::new(),
        }
    }

    /// The `Selection` a compositor delivers for a source advertising these
    /// flavors — including the watcher's own filter, which fetches only the
    /// flavors clippo finds interesting.
    fn selection_of(offered: &[Flavor]) -> Selection {
        Selection {
            kind: SelectionKind::Clipboard,
            advertised: offered.iter().map(|flavor| flavor.mime.clone()).collect(),
            flavors: offered
                .iter()
                .filter(|flavor| clippo_wayland::is_interesting(&flavor.mime))
                .cloned()
                .collect(),
            dropped: Vec::new(),
        }
    }

    /// A real PNG, so the store derives a real thumbnail to leave out of the
    /// copy-back. Bytes it cannot decode would give it nothing to exclude, and
    /// the test would pass without proving anything.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut picture = image::RgbImage::new(width, height);
        for (x, y, pixel) in picture.enumerate_pixels_mut() {
            *pixel = image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]);
        }
        let mut bytes = Vec::new();
        picture
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("the test image should encode");
        bytes
    }

    fn image_selection(png: Vec<u8>) -> Selection {
        Selection {
            kind: SelectionKind::Clipboard,
            advertised: vec!["image/png".to_owned()],
            flavors: vec![Flavor::new("image/png", png)],
            dropped: Vec::new(),
        }
    }

    async fn previews(daemon: &Daemon) -> Vec<String> {
        daemon
            .list(0, 0)
            .await
            .expect("list should answer")
            .into_iter()
            .map(|entry| entry.preview)
            .collect()
    }

    #[tokio::test]
    async fn a_captured_copy_is_listed_searchable_and_announced() {
        let fixture = Fixture::new();
        fixture.capture(&["hello world"]).await;

        assert_eq!(fixture.previews().await, ["hello world"]);
        let found = fixture.daemon.search("hlo", 10).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, "text");
        assert_eq!(fixture.emitted(), 1);
    }

    /// A query nobody types is refused over the wire, and — the part that
    /// matters — refused before the state lock is taken, so a peer sending one
    /// cannot hold up the frontends using the daemon properly.
    #[tokio::test]
    async fn an_over_long_search_query_is_refused_without_taking_the_lock() {
        let fixture = Fixture::new();
        fixture.capture(&["hello world"]).await;

        // Held for the duration of the call: if `search` waited for it, the
        // `await` below would never return.
        let held = fixture.daemon.state.lock().await;

        // Bounded so that a check moved back under the lock fails the test
        // rather than hanging it.
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fixture.daemon.search(&"h".repeat(4 * 1024 * 1024), 10),
        )
        .await
        .expect("an over-long query must be refused without waiting for the lock")
        .unwrap_err();
        assert!(matches!(error, fdo::Error::InvalidArgs(_)), "{error:?}");
        assert!(error.to_string().contains("search query"), "{error}");

        drop(held);

        // And an ordinary query still works once nothing is holding the lock.
        let found = fixture.daemon.search("hlo", 10).await.unwrap();
        assert_eq!(found.len(), 1);
    }

    /// The cache is rebuilt from the store at startup, so a second daemon over
    /// the same database searches the history the first one recorded. ROADMAP
    /// Verification §6, minus the process.
    #[tokio::test]
    async fn a_restarted_daemon_rebuilds_its_cache_from_the_store() {
        let dir = tempfile::tempdir().expect("a temp dir for the test database");
        let key = Key::random().expect("a key for the test database");
        let path = dir.path().join("history.db");

        let first = Daemon::new(
            Store::open(&path, &key).expect("open"),
            Signals::discard(),
            SecretsConfig::default(),
            paste_shortcut(),
            true,
        )
        .expect("start");
        first
            .capture_at(
                text_selection("survives a restart"),
                Timestamp::from_unix_millis(1_000),
            )
            .await;
        drop(first);

        let second = Daemon::new(
            Store::open(&path, &key).expect("reopen"),
            Signals::discard(),
            SecretsConfig::default(),
            paste_shortcut(),
            true,
        )
        .expect("restart");
        assert_eq!(second.search("restart", 10).await.unwrap().len(), 1);
        assert_eq!(second.list(0, 0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_deleted_entry_leaves_search_immediately() {
        let fixture = Fixture::new();
        fixture.capture(&["keep me", "delete me"]).await;

        let doomed = fixture.daemon.search("delete me", 1).await.unwrap()[0].id;
        fixture.daemon.delete(doomed).await.expect("delete");

        assert!(fixture
            .daemon
            .search("delete me", 10)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(fixture.previews().await, ["keep me"]);
        assert_eq!(fixture.emitted(), 3, "two captures and one delete");
    }

    #[tokio::test]
    async fn clearing_empties_the_cache_and_spares_pins() {
        let fixture = Fixture::new();
        fixture.capture(&["ordinary", "pinned"]).await;

        let keeper = fixture.daemon.search("pinned", 1).await.unwrap()[0].id;
        fixture.daemon.pin(keeper, true).await.expect("pin");
        fixture.daemon.clear(false).await.expect("clear");
        assert_eq!(fixture.previews().await, ["pinned"]);

        fixture
            .daemon
            .clear(true)
            .await
            .expect("clear including pinned");
        assert!(fixture.previews().await.is_empty());
        assert!(fixture.daemon.search("", 0).await.unwrap().is_empty());
    }

    /// Retention evicts inside `Store::insert`, which reports what it wrote and
    /// not what it dropped. Reloading the whole cache is what keeps the evicted
    /// entry out of search regardless.
    #[tokio::test]
    async fn an_entry_retention_evicted_leaves_search_too() {
        let fixture = Fixture::new();
        fixture
            .daemon
            .state
            .lock()
            .await
            .store
            .set_retention(Retention {
                max_entries: 2,
                max_age: None,
            });
        fixture.capture(&["first", "second", "third"]).await;

        assert_eq!(fixture.previews().await, ["third", "second"]);
        assert!(fixture.daemon.search("first", 10).await.unwrap().is_empty());
    }

    /// A `Delete` of an id that is not there changes nothing, so it must not
    /// wake every frontend up to re-read an identical list.
    #[tokio::test]
    async fn a_mutation_that_changed_nothing_is_not_announced() {
        let fixture = Fixture::new();
        fixture.capture(&["one"]).await;
        let before = fixture.emitted();

        assert!(fixture.daemon.delete(9_999).await.is_err());
        assert_eq!(fixture.emitted(), before);

        fixture.daemon.clear(true).await.expect("clear");
        assert_eq!(fixture.emitted(), before + 1);

        fixture
            .daemon
            .clear(true)
            .await
            .expect("clear an empty history");
        assert_eq!(fixture.emitted(), before + 1);
    }

    #[tokio::test]
    async fn pausing_stops_captures_and_leaves_reads_working() {
        let fixture = Fixture::new();
        fixture.capture(&["before the pause"]).await;

        fixture.daemon.set_paused(true).await.expect("pause");
        assert!(fixture.daemon.paused().await.unwrap());
        fixture.capture(&["during the pause"]).await;

        assert_eq!(fixture.previews().await, ["before the pause"]);
        assert_eq!(fixture.daemon.search("pause", 10).await.unwrap().len(), 1);
        assert_eq!(fixture.emitted(), 1, "nothing recorded, nothing announced");

        fixture.daemon.set_paused(false).await.expect("resume");
        assert!(!fixture.daemon.paused().await.unwrap());
        fixture.capture(&["after the pause"]).await;
        assert_eq!(
            fixture.previews().await,
            ["after the pause", "before the pause"],
            "the paused copy is gone for good; resuming does not replay it"
        );
    }

    /// A repeat copy is a bump, not a second row — `clippo-store`'s dedup seen
    /// from the daemon's side, including that frontends still hear about it.
    #[tokio::test]
    async fn a_repeat_copy_moves_the_entry_rather_than_adding_one() {
        let fixture = Fixture::new();
        fixture.capture(&["first", "second", "first"]).await;

        assert_eq!(fixture.previews().await, ["first", "second"]);
        assert_eq!(fixture.emitted(), 3);
    }

    #[tokio::test]
    async fn reveal_returns_the_whole_value_where_list_returned_a_preview() {
        let fixture = Fixture::new();
        let whole = format!("{}tail", "long ".repeat(100));
        fixture.capture(&[&whole]).await;

        let summary = fixture.daemon.list(1, 0).await.unwrap().remove(0);
        assert!(summary.preview.chars().count() < whole.chars().count());
        assert_eq!(fixture.daemon.reveal(summary.id).await.unwrap(), whole);
    }

    #[tokio::test]
    async fn an_unknown_id_is_an_argument_error_on_every_member_that_takes_one() {
        let fixture = Fixture::new();
        let missing = 4_242;
        assert!(matches!(
            fixture.daemon.copy(missing).await,
            Err(fdo::Error::InvalidArgs(_))
        ));
        assert_eq!(
            fixture.clipboard.offer_count(),
            0,
            "an id that is not there must not reach the compositor"
        );
        assert!(matches!(
            fixture.daemon.delete(missing).await,
            Err(fdo::Error::InvalidArgs(_))
        ));
        assert!(matches!(
            fixture.daemon.pin(missing, true).await,
            Err(fdo::Error::InvalidArgs(_))
        ));
        assert!(matches!(
            fixture.daemon.reveal(missing).await,
            Err(fdo::Error::InvalidArgs(_))
        ));
    }

    /// The password-manager marker is the one detection signal only the capture
    /// path can see — the marker flavor need not even be stored.
    ///
    /// **Masked, not skipped**, which DESIGN.md is explicit about: the entry is
    /// there, it is a text entry, and the value is still in the store. What
    /// changes is the one line a frontend shows.
    #[tokio::test]
    async fn a_password_manager_copy_is_recorded_as_sensitive() {
        let fixture = Fixture::new();
        fixture.capture_password("hunter2").await;

        let entry = fixture.daemon.list(1, 0).await.unwrap().remove(0);
        assert!(entry.sensitive);
        assert_eq!(
            entry.kind, "text",
            "the marker carries no content of its own"
        );
        assert!(!entry.preview.contains("hunter2"), "{}", entry.preview);
        assert_eq!(fixture.daemon.reveal(entry.id).await.unwrap(), "hunter2");
    }

    /// The same bytes twice, and only the second copy knows they are a secret.
    ///
    /// `hunter2` out of a text editor and then out of a password manager hash
    /// alike — the marker is never the canonical flavor — so the second arrival
    /// is a bump rather than a new row. The bump has to bring its *preview*
    /// with it and not just its flag: a row marked sensitive whose preview is
    /// still the password is worse than one that was never flagged, because the
    /// applet draws its lock badge next to the plaintext.
    ///
    /// The same shape covers the two routes with no password manager in them —
    /// a row captured while `entropy_rule` was off, and a row captured before
    /// masking existed at all. Both are upgraded by the next copy.
    #[tokio::test]
    async fn a_repeat_copy_that_knows_better_masks_the_preview_it_already_stored() {
        let fixture = Fixture::new();

        fixture.capture(&["hunter2"]).await;
        let entry = fixture.daemon.list(1, 0).await.unwrap().remove(0);
        assert!(!entry.sensitive, "nothing about it looks like a secret yet");
        assert_eq!(entry.preview, "hunter2");

        fixture.capture_password("hunter2").await;

        let listed = fixture.daemon.list(0, 0).await.unwrap();
        assert_eq!(listed.len(), 1, "the same bytes are still one entry");
        let entry = &listed[0];
        assert!(entry.sensitive);
        assert!(
            !entry.preview.contains("hunter2"),
            "the preview stored in the clear survived the bump: {}",
            entry.preview
        );
        assert!(
            !format!("{listed:?}").contains("hunter2"),
            "the password crossed the bus in a List response"
        );
        assert_eq!(
            fixture.daemon.reveal(entry.id).await.unwrap(),
            "hunter2",
            "and the value is still there to be revealed and pasted"
        );
    }

    /// The acceptance criterion, as a test: no member but `Reveal` returns a
    /// sensitive value, and this is the check that is supposed to fail the next
    /// time a preview-building helper is refactored.
    ///
    /// It asserts on the *whole serialised payload* rather than on the preview
    /// field, so a value that turned up in some field added later — a subtitle,
    /// a tooltip, a search snippet — would fail it too.
    #[tokio::test]
    async fn no_sensitive_value_ever_appears_in_a_list_or_search_payload() {
        let fixture = Fixture::new();
        let secrets = [
            "sk-ClippoFixtureNotARealKey00000000000000000000",
            "Xr4$Tp9!Lm2#Wq7&Zc5%",
            "postgres://clippo:not-a-real-password@db.example.internal:5432/clippo",
        ];
        fixture.capture(&secrets).await;
        fixture.capture_password("Tr0ub4dor&3").await;

        let listed = fixture.daemon.list(0, 0).await.unwrap();
        assert_eq!(listed.len(), 4, "every one of them is stored");
        assert!(
            listed.iter().all(|entry| entry.sensitive),
            "all four should have been detected: {:?}",
            listed.iter().map(|e| &e.preview).collect::<Vec<_>>()
        );

        let mut payload = format!("{listed:?}");
        // Search reaches the same summaries by another route, so it gets the
        // same treatment. A masked preview is unmatchable, so search by what a
        // user would actually type.
        for query in ["sk", "clippo", "Tr0ub4dor", "postgres"] {
            payload.push_str(&format!(
                "{:?}",
                fixture.daemon.search(query, 10).await.unwrap()
            ));
        }

        for secret in secrets.iter().chain(&["Tr0ub4dor&3"]) {
            assert!(
                !payload.contains(secret),
                "{secret} crossed the bus in a List or Search response"
            );
        }

        // …and `Reveal` does return them, or the masking above would be
        // hiding the values from their owner rather than from the room.
        for entry in listed {
            let revealed = fixture.daemon.reveal(entry.id).await.unwrap();
            assert!(
                secrets.contains(&revealed.as_str()) || revealed == "Tr0ub4dor&3",
                "reveal returned {revealed:?}"
            );
        }
    }

    /// The highest-severity bug this milestone could introduce: a mask reaching
    /// the clipboard. `Copy` must offer the stored bytes, whatever the preview
    /// says. M3c's path is untouched by masking, and this is what says so.
    #[tokio::test]
    async fn copying_a_masked_entry_puts_the_real_value_on_the_clipboard() {
        let fixture = Fixture::new();
        let password = "Xr4$Tp9!Lm2#Wq7&Zc5%";
        fixture.capture(&[password]).await;

        let entry = fixture.daemon.list(1, 0).await.unwrap().remove(0);
        assert!(entry.sensitive && !entry.preview.contains("Tp9"));

        fixture.copy(EntryId::new(entry.id)).await.expect("copy");
        let offered = fixture.clipboard.last_offer();
        assert_eq!(
            offered
                .iter()
                .find(|flavor| flavor.mime.starts_with("text/plain"))
                .and_then(|flavor| flavor.as_str()),
            Some(password),
            "a paste must get the value, not the mask"
        );
        assert!(
            offered
                .iter()
                .all(|flavor| !String::from_utf8_lossy(&flavor.data).contains('\u{2022}')),
            "a bullet reached the clipboard"
        );
    }

    /// `Paste` is `Copy` plus a keystroke, and this is the "plus a keystroke"
    /// half: the configured chord, pressed once.
    #[tokio::test]
    async fn pasting_copies_and_then_presses_the_configured_shortcut() {
        let fixture = Fixture::new();
        let keyboard = Arc::new(FakeKeyboard::default());
        fixture
            .daemon
            .connect_keystrokes(Arc::clone(&keyboard) as Arc<dyn Keystrokes>);
        fixture.capture(&["the thing to paste"]).await;

        let id = fixture.daemon.list(1, 0).await.unwrap().remove(0).id;
        assert!(
            fixture.daemon.paste(id).await.expect("paste"),
            "a pressed shortcut is reported as pressed"
        );

        assert_eq!(
            fixture.clipboard.offer_count(),
            1,
            "Paste has to copy before it presses anything"
        );
        assert_eq!(keyboard.presses(), vec!["Ctrl+V".to_owned()]);
    }

    /// The knob, and the whole of what it changes: the entry still reaches the
    /// clipboard and the call still succeeds, and no key is pressed.
    #[tokio::test]
    async fn auto_paste_off_copies_and_presses_nothing() {
        let fixture = Fixture::without_auto_paste();
        let keyboard = Arc::new(FakeKeyboard::default());
        fixture
            .daemon
            .connect_keystrokes(Arc::clone(&keyboard) as Arc<dyn Keystrokes>);
        fixture.capture(&["copied, not pasted"]).await;

        let id = fixture.daemon.list(1, 0).await.unwrap().remove(0).id;
        assert!(
            !fixture.daemon.paste(id).await.expect("paste must not fail"),
            "nothing was pressed, and the answer has to say so"
        );

        assert_eq!(
            fixture.clipboard.offer_count(),
            1,
            "auto_paste is about the keystroke, not about the copy"
        );
        assert!(
            keyboard.presses().is_empty(),
            "auto_paste = false must not press a key even with a keyboard to press it with"
        );
    }

    /// The ordering that matters most: pressing the shortcut before the entry
    /// is on the clipboard would paste whatever was there before, which is a
    /// wrong paste rather than a failed one.
    #[tokio::test]
    async fn a_paste_that_cannot_copy_never_presses_anything() {
        let fixture = Fixture::new();
        let keyboard = Arc::new(FakeKeyboard::default());
        fixture
            .daemon
            .connect_keystrokes(Arc::clone(&keyboard) as Arc<dyn Keystrokes>);

        assert!(matches!(
            fixture.daemon.paste(4_242).await,
            Err(fdo::Error::InvalidArgs(_))
        ));
        assert!(
            keyboard.presses().is_empty(),
            "an entry that does not exist must not paste the last one"
        );
    }

    /// A compositor that will not synthesise keys degrades `Paste` to `Copy`
    /// rather than failing it: the entry is on the clipboard, so the user can
    /// finish by hand, and reporting an error would have a frontend say the
    /// whole thing failed.
    #[tokio::test]
    async fn a_paste_with_no_keyboard_at_all_still_copies_and_still_succeeds() {
        let fixture = Fixture::new();
        fixture.capture(&["copied but not pressed"]).await;

        let id = fixture.daemon.list(1, 0).await.unwrap().remove(0).id;
        assert!(
            !fixture.daemon.paste(id).await.expect("paste must not fail"),
            "no keyboard means nothing was pressed"
        );

        assert_eq!(fixture.clipboard.offer_count(), 1);
    }

    /// The same, for a keyboard that is there and refuses.
    #[tokio::test]
    async fn a_keystroke_that_fails_does_not_fail_the_paste() {
        let fixture = Fixture::new();
        let keyboard = Arc::new(FakeKeyboard {
            broken: true,
            ..FakeKeyboard::default()
        });
        fixture
            .daemon
            .connect_keystrokes(Arc::clone(&keyboard) as Arc<dyn Keystrokes>);
        fixture.capture(&["copied but not pressed"]).await;

        let id = fixture.daemon.list(1, 0).await.unwrap().remove(0).id;
        assert!(
            !fixture.daemon.paste(id).await.expect("paste must not fail"),
            "a refused keystroke is not a pressed one"
        );

        assert_eq!(fixture.clipboard.offer_count(), 1);
        assert!(keyboard.presses().is_empty());
    }

    /// A masked entry pastes its real value, for the same reason `Copy` does —
    /// and this is the path where getting it wrong would be worst, because the
    /// value lands in another application without the user seeing it first.
    #[tokio::test]
    async fn pasting_a_masked_entry_presses_for_the_real_value() {
        let fixture = Fixture::new();
        let keyboard = Arc::new(FakeKeyboard::default());
        fixture
            .daemon
            .connect_keystrokes(Arc::clone(&keyboard) as Arc<dyn Keystrokes>);
        let password = "Xr4$Tp9!Lm2#Wq7&Zc5%";
        fixture.capture(&[password]).await;

        let entry = fixture.daemon.list(1, 0).await.unwrap().remove(0);
        assert!(entry.sensitive);
        assert!(fixture.daemon.paste(entry.id).await.expect("paste"));

        assert_eq!(
            fixture
                .clipboard
                .last_offer()
                .iter()
                .find(|flavor| flavor.mime.starts_with("text/plain"))
                .and_then(|flavor| flavor.as_str()),
            Some(password),
            "a synthesised paste must deliver the value, not the mask"
        );
        assert_eq!(keyboard.presses().len(), 1);
    }

    /// DESIGN.md's escape hatch, at the daemon level: the knob switches off the
    /// entropy rule for captures and leaves the other two working.
    #[tokio::test]
    async fn the_entropy_knob_turns_off_one_rule_and_not_the_others() {
        let fixture = Fixture::with_secrets(SecretsConfig {
            entropy_rule: false,
            ..SecretsConfig::default()
        });
        fixture
            .capture(&["Xr4$Tp9!Lm2#Wq7&Zc5%", "AKIAIOSFODNN7EXAMPLE"])
            .await;
        fixture.capture_password("Tr0ub4dor&3").await;

        let previews = fixture.previews().await;
        assert!(
            previews.contains(&"Xr4$Tp9!Lm2#Wq7&Zc5%".to_owned()),
            "the entropy rule should be off: {previews:?}"
        );

        let flagged: Vec<bool> = fixture
            .daemon
            .list(0, 0)
            .await
            .unwrap()
            .iter()
            .map(|entry| entry.sensitive)
            .collect();
        // Newest first: the password-manager copy, the AWS id, the password.
        assert_eq!(flagged, [true, true, false]);
    }

    /// A copy of nothing but the marker has no canonical flavor, so it has no
    /// hash and no identity to store it under.
    #[tokio::test]
    async fn a_copy_with_no_storable_flavor_is_skipped_quietly() {
        let fixture = Fixture::new();
        fixture
            .daemon
            .capture_at(
                Selection {
                    kind: SelectionKind::Clipboard,
                    advertised: vec![PASSWORD_MANAGER_HINT_MIME.to_owned()],
                    flavors: vec![Flavor::new(PASSWORD_MANAGER_HINT_MIME, "secret")],
                    dropped: Vec::new(),
                },
                Timestamp::from_unix_millis(1_000),
            )
            .await;

        assert!(fixture.previews().await.is_empty());
        assert_eq!(fixture.emitted(), 0);
    }

    /// `Copy` puts every stored flavor on the clipboard, moves the entry to the
    /// front and says so — the same reordering a fresh duplicate copy causes.
    #[tokio::test]
    async fn copy_offers_the_entry_bumps_it_and_announces_the_change() {
        let fixture = Fixture::new();
        fixture.capture(&["first", "second"]).await;
        let oldest = fixture.id_of("first").await;
        let before = fixture.last_used_at(oldest).await;
        let announced = fixture.emitted();

        fixture.copy(oldest).await.expect("copy should work");

        assert_eq!(
            fixture.clipboard.last_offer(),
            [Flavor::new("text/plain;charset=utf-8", "first")],
            "what the compositor was handed"
        );
        assert_eq!(
            fixture.previews().await,
            ["first", "second"],
            "a paste is a use, so the entry moves to the front"
        );
        assert!(fixture.last_used_at(oldest).await > before);
        assert_eq!(fixture.emitted(), announced + 1);
    }

    /// The specific bug the exclusion prevents: an application negotiating
    /// `image/png;clippo-thumb` and pasting a 256-pixel version of the user's
    /// screenshot. A paste that succeeds with the wrong picture is worse than
    /// one that fails, so the thumbnail never reaches the compositor at all.
    #[tokio::test]
    async fn the_derived_thumbnail_is_never_offered_back() {
        let fixture = Fixture::new();
        let full_size = png(400, 300);
        fixture
            .daemon
            .capture_at(image_selection(full_size.clone()), fixture.tick())
            .await;

        let id = EntryId::new(fixture.daemon.list(1, 0).await.unwrap()[0].id);
        let stored: Vec<String> = {
            let state = fixture.daemon.state.lock().await;
            let entry = state.store.get(id).unwrap().unwrap();
            entry.flavors.iter().map(|f| f.mime.clone()).collect()
        };
        assert!(
            stored.iter().any(|mime| mime == THUMBNAIL_MIME),
            "the store must have derived a thumbnail for this to be a real test: {stored:?}"
        );

        fixture.copy(id).await.expect("copy should work");

        let offered = fixture.clipboard.last_offer();
        assert_eq!(
            offered.iter().map(|f| f.mime.as_str()).collect::<Vec<_>>(),
            ["image/png"],
            "every stored flavor except the one clippo derived for itself"
        );
        assert_eq!(offered[0].data, full_size, "and it is the full-size image");
    }

    /// What the applet draws an image row from, and what it must cost. The
    /// member returns the derived thumbnail and not the full-size PNG beside
    /// it — the whole reason a thumbnail is stored at capture.
    #[tokio::test]
    async fn the_thumbnail_member_returns_the_derived_png_and_not_the_full_size_one() {
        let fixture = Fixture::new();
        let full_size = png(400, 300);
        fixture
            .daemon
            .capture_at(image_selection(full_size.clone()), fixture.tick())
            .await;
        let id = fixture.daemon.list(1, 0).await.unwrap()[0].id;

        let thumb = fixture.daemon.thumbnail(id).await.expect("an image row");

        assert_ne!(thumb, full_size, "not the image the user copied");
        assert!(thumb.len() < full_size.len());
        let stored = {
            let state = fixture.daemon.state.lock().await;
            state.store.get(EntryId::new(id)).unwrap().unwrap()
        };
        assert_eq!(
            thumb,
            stored
                .flavors
                .iter()
                .find(|flavor| flavor.mime == THUMBNAIL_MIME)
                .expect("the store derived one")
                .data,
            "exactly the stored thumbnail flavor"
        );
    }

    /// The two refusals, which the applet tells apart from a sick daemon: an
    /// entry that has no thumbnail to give, and an id that is not there.
    #[tokio::test]
    async fn asking_for_a_thumbnail_that_does_not_exist_is_refused_not_faked() {
        let fixture = Fixture::new();
        fixture.capture(&["just some text"]).await;
        let id = fixture.daemon.list(1, 0).await.unwrap()[0].id;

        let refused = fixture
            .daemon
            .thumbnail(id)
            .await
            .expect_err("text has no thumbnail");
        assert!(
            matches!(refused, fdo::Error::NotSupported(_)),
            "{refused:?}"
        );

        let missing = fixture
            .daemon
            .thumbnail(9_999)
            .await
            .expect_err("there is no such entry");
        assert!(
            matches!(missing, fdo::Error::InvalidArgs(_)),
            "{missing:?}, so a frontend can tell it from a daemon fault"
        );
    }

    /// DESIGN.md's risk table: *"**Self-echo loop** — a wrong hash guard
    /// re-enters every copy-back into history → Integration test at M3."*
    ///
    /// Both directions, because getting this wrong the other way is also a bug:
    /// a permanent "ignore this hash" would make a deliberate re-copy of the
    /// same text vanish from the history with nothing to say why.
    #[tokio::test]
    async fn a_copy_back_does_not_re_enter_the_history_but_a_real_re_copy_still_bumps_it() {
        let fixture = Fixture::new();
        fixture.capture(&["first", "second"]).await;
        let oldest = fixture.id_of("first").await;

        fixture.copy(oldest).await.expect("copy should work");
        let after_copy = fixture.emitted();
        let bumped_by_copy = fixture.last_used_at(oldest).await;

        // The compositor announces the selection clippo just took, to clippo.
        fixture.echo().await;

        assert_eq!(
            fixture.previews().await,
            ["first", "second"],
            "the copy-back must not add a second entry"
        );
        assert_eq!(
            fixture.last_used_at(oldest).await,
            bumped_by_copy,
            "nor bump the entry a second time for a use that never happened"
        );
        assert_eq!(fixture.emitted(), after_copy, "and nothing to announce");

        // Now the other direction. Something else is copied, so "first" is no
        // longer at the front, and then the user copies that same text by hand.
        fixture.capture(&["third"]).await;
        assert_eq!(fixture.previews().await, ["third", "first", "second"]);
        let before_recopy = fixture.emitted();

        fixture.capture(&["first"]).await;

        assert_eq!(
            fixture.previews().await,
            ["first", "third", "second"],
            "copying the same text by hand is a real copy and must bump the entry"
        );
        assert!(fixture.last_used_at(oldest).await > bumped_by_copy);
        assert_eq!(fixture.emitted(), before_recopy + 1);
    }

    /// The guard is armed for one capture, so an echo that never arrives must
    /// not leave it waiting: another application taking the clipboard is the
    /// signal that it is not coming.
    #[tokio::test]
    async fn losing_the_selection_clears_the_guard_so_the_next_real_copy_registers() {
        let fixture = Fixture::new();
        fixture.capture(&["first"]).await;
        let id = fixture.id_of("first").await;

        fixture.copy(id).await.expect("copy should work");
        assert!(fixture.daemon.state.lock().await.echo.is_armed());

        fixture.daemon.selection_lost().await;
        assert!(!fixture.daemon.state.lock().await.echo.is_armed());

        // The very content the guard was armed for, copied by hand.
        let before = fixture.last_used_at(id).await;
        let announced = fixture.emitted();
        fixture.capture(&["first"]).await;

        assert!(
            fixture.last_used_at(id).await > before,
            "a stale guard would have swallowed this copy"
        );
        assert_eq!(fixture.emitted(), announced + 1);
        assert_eq!(fixture.previews().await, ["first"], "still one entry");
    }

    /// A copy-back made while paused still spends its guard. Left armed, it
    /// would swallow the next real copy of that content instead.
    #[tokio::test]
    async fn a_copy_back_that_arrives_while_paused_still_spends_the_guard() {
        let fixture = Fixture::new();
        fixture.capture(&["first"]).await;
        let id = fixture.id_of("first").await;

        fixture.copy(id).await.expect("copy should work");
        fixture.daemon.set_paused(true).await.expect("pause");
        fixture.echo().await;
        fixture.daemon.set_paused(false).await.expect("resume");

        assert!(!fixture.daemon.state.lock().await.echo.is_armed());
        let before = fixture.last_used_at(id).await;
        fixture.capture(&["first"]).await;
        assert!(fixture.last_used_at(id).await > before);
    }

    /// A daemon over a temp database with a clipboard of the caller's choosing,
    /// for the two paths [`Fixture`]'s working one cannot reach.
    async fn daemon_with(clipboard: Option<Arc<dyn Clipboard>>) -> (Arc<Daemon>, TempDir, EntryId) {
        let dir = tempfile::tempdir().expect("a temp dir for the test database");
        let key = Key::random().expect("a key for the test database");
        let daemon = Daemon::new(
            Store::open(dir.path().join("history.db"), &key).expect("open"),
            Signals::discard(),
            SecretsConfig::default(),
            paste_shortcut(),
            true,
        )
        .expect("start");
        if let Some(clipboard) = clipboard {
            daemon.connect_clipboard(clipboard);
        }
        daemon
            .capture_at(text_selection("first"), Timestamp::from_unix_millis(1_000))
            .await;
        let id = EntryId::new(daemon.list(1, 0).await.unwrap()[0].id);
        (daemon, dir, id)
    }

    /// A `Copy` the compositor never got is a failed `Copy`: nothing
    /// reordered, and no guard left armed to swallow the next real copy of that
    /// entry.
    #[tokio::test]
    async fn a_copy_the_clipboard_refused_changes_nothing_and_leaves_no_guard() {
        let (daemon, _dir, id) = daemon_with(Some(Arc::new(BrokenClipboard))).await;
        let announced = daemon.signals.emitted();

        let refused = daemon
            .copy_at(id, Timestamp::from_unix_millis(2_000))
            .await
            .expect_err("the clipboard refused it");
        assert!(matches!(refused, fdo::Error::Failed(_)), "{refused:?}");

        let entry = daemon.state.lock().await.store.get(id).unwrap().unwrap();
        assert_eq!(
            entry.entry.last_used_at,
            Timestamp::from_unix_millis(1_000),
            "a Copy that could not paste must not reorder the history"
        );
        assert_eq!(daemon.signals.emitted(), announced);
        assert!(
            !daemon.state.lock().await.echo.is_armed(),
            "a guard armed for a copy-back that never left would swallow a real copy"
        );
    }

    /// Before the Wayland watcher has started there is no clipboard to put an
    /// entry on, and `Copy` says so rather than reporting a paste.
    #[tokio::test]
    async fn copy_before_the_watcher_is_up_says_so_and_changes_nothing() {
        let (daemon, _dir, id) = daemon_with(None).await;

        let refused = daemon
            .copy_at(id, Timestamp::from_unix_millis(2_000))
            .await
            .expect_err("there is no clipboard yet");
        assert!(matches!(refused, fdo::Error::Failed(_)), "{refused:?}");

        let entry = daemon.state.lock().await.store.get(id).unwrap().unwrap();
        assert_eq!(entry.entry.last_used_at, Timestamp::from_unix_millis(1_000));
        assert!(!daemon.state.lock().await.echo.is_armed());
    }

    /// The startup sweep goes through the same commit path as every other
    /// mutation, so what it evicts leaves search as well as the database.
    #[tokio::test]
    async fn the_startup_retention_sweep_reloads_the_cache_and_announces_itself() {
        let fixture = Fixture::new();
        fixture.capture(&["first", "second", "third"]).await;
        let before = fixture.emitted();

        fixture
            .daemon
            .state
            .lock()
            .await
            .store
            .set_retention(Retention {
                max_entries: 1,
                max_age: None,
            });
        fixture
            .daemon
            .sweep_retention(Timestamp::from_unix_millis(2_000))
            .await
            .expect("the sweep should run");

        assert_eq!(fixture.previews().await, ["third"]);
        assert!(fixture.daemon.search("first", 10).await.unwrap().is_empty());
        assert_eq!(fixture.emitted(), before + 1);

        // A sweep that dropped nothing is not a change, so it wakes nobody.
        fixture
            .daemon
            .sweep_retention(Timestamp::from_unix_millis(2_001))
            .await
            .expect("the second sweep should run");
        assert_eq!(fixture.emitted(), before + 1);
    }
}
