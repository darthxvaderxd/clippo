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
use std::sync::Arc;

use async_trait::async_trait;
use clippo_core::{EntryId, EntryKind, NewEntry, Timestamp};
use clippo_ipc::{ClippoBackend, ClippoInterface, EntrySummary};
use clippo_store::{dedup, Store, StoreError};
use clippo_wayland::Selection;
use tokio::sync::{Mutex, MutexGuard};
use tracing::{debug, error, info, warn};
use zbus::fdo;
use zbus::object_server::SignalEmitter;

use crate::cache::PreviewCache;
use crate::preview;

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
    signals: Signals,
}

impl Daemon {
    /// Build a daemon around an open store, filling the cache from it.
    ///
    /// The cache is built here rather than lazily so that a database that will
    /// not read fails at startup, in the journal, rather than on a user's first
    /// `List`.
    pub fn new(store: Store, signals: Signals) -> Result<Arc<Self>, StoreError> {
        let mut state = State {
            store,
            cache: PreviewCache::new(),
        };
        reload(&mut state)?;
        info!(
            entries = state.cache.entry_count(),
            "clippo loaded the clipboard history"
        );
        Ok(Arc::new(Self {
            state: Mutex::new(state),
            paused: AtomicBool::new(false),
            signals,
        }))
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

        if self.paused.load(Ordering::Relaxed) {
            debug!(
                flavors = selection.flavors.len(),
                "clippo is paused, so this copy was not recorded"
            );
            return;
        }
        if selection.is_empty() {
            debug!(
                advertised = selection.advertised.len(),
                "clippo captured nothing storable from a copy"
            );
            return;
        }

        let Some(new) = new_entry(selection, now) else {
            debug!("a copy carried no flavor clippo can store, so it was skipped");
            return;
        };
        let kind = new.kind;
        let sensitive = new.sensitive;

        let insertion = {
            let mut state = self.state.lock().await;
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
fn new_entry(selection: Selection, now: Timestamp) -> Option<NewEntry> {
    // Read before the flavors are moved out.
    let sensitive = selection.has_password_manager_hint();
    let flavors = selection.flavors;

    let kind = EntryKind::for_flavors(&flavors)?;
    let hash = dedup::hash(kind, &flavors)?;
    Some(NewEntry {
        created_at: now,
        kind,
        preview: preview::build(kind, &flavors),
        hash,
        // The MIME hint, which is the one detection signal that needs no
        // heuristic and that only the capture path can see — the marker flavor
        // may not even be stored. The shape regexes and the entropy rule join
        // it at M4, in `clippo-core`; the store ORs this flag on a repeat copy
        // so a later, better-informed capture can only ever raise it.
        sensitive,
        flavors,
    })
}

#[async_trait]
impl ClippoBackend for Daemon {
    async fn list(&self, limit: u32, offset: u32) -> fdo::Result<Vec<EntrySummary>> {
        let state = self.state.lock().await;
        Ok(state.cache.page(limit as usize, offset as usize))
    }

    async fn search(&self, query: &str, limit: u32) -> fdo::Result<Vec<EntrySummary>> {
        let mut state = self.state.lock().await;
        Ok(state.cache.search(query, limit as usize))
    }

    /// Put an entry back on the clipboard — **not implemented yet**, and it
    /// says so.
    ///
    /// Offering a stored entry back to the compositor needs `clippo-wayland`'s
    /// source half and the self-echo guard that goes with it, which is the next
    /// milestone; M1a implemented watching only. Until that lands, the honest
    /// answer is an error: a caller that got `Ok(())` would paste whatever was
    /// on the clipboard before and have nothing to tell them why — and
    /// `clippo copy 2` followed by Ctrl-V is precisely the ROADMAP check a user
    /// would run.
    ///
    /// The store half is not done either. Moving the entry to the front records
    /// a use that did not happen, so a `busctl` poke would silently reorder the
    /// real history; the `touch` belongs with the paste, in the same call that
    /// earns it.
    async fn copy(&self, id: i64) -> fdo::Result<()> {
        warn!(
            id,
            "Copy did nothing: putting an entry back on the clipboard is not implemented yet"
        );
        Err(fdo::Error::NotSupported(format!(
            "clippo cannot put entry {id} back on the clipboard yet: the copy-back offer path is \
             not implemented. The history is unchanged"
        )))
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
    use std::sync::atomic::AtomicI64;

    use clippo_core::Flavor;
    use clippo_store::{Key, Retention};
    use clippo_wayland::{SelectionKind, PASSWORD_MANAGER_HINT_MIME};
    use tempfile::TempDir;

    use super::*;

    /// A daemon over a temp database, with signals counted rather than sent.
    struct Fixture {
        daemon: Arc<Daemon>,
        /// Hands out a fresh millisecond per capture. `last_used_at` has
        /// millisecond resolution and decides the order of the history, so
        /// captures made against the real clock inside one millisecond would
        /// order by a tie-break rather than by the rule under test.
        ///
        /// The values start just after the epoch, which also makes `Copy`'s
        /// real-clock `touch` unambiguously newer than anything captured here.
        clock: AtomicI64,
        /// Removed on drop, so it has to outlive the daemon.
        _dir: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("a temp dir for the test database");
            let key = Key::random().expect("a key for the test database");
            let store =
                Store::open(dir.path().join("history.db"), &key).expect("the store should open");
            Self {
                daemon: Daemon::new(store, Signals::discard()).expect("the daemon should start"),
                clock: AtomicI64::new(1_000),
                _dir: dir,
            }
        }

        /// Capture each text one millisecond after the last.
        async fn capture(&self, texts: &[&str]) {
            for text in texts {
                let now = Timestamp::from_unix_millis(self.clock.fetch_add(1, Ordering::Relaxed));
                self.daemon.capture_at(text_selection(text), now).await;
            }
        }

        async fn previews(&self) -> Vec<String> {
            previews(&self.daemon).await
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

    /// The cache is rebuilt from the store at startup, so a second daemon over
    /// the same database searches the history the first one recorded. ROADMAP
    /// Verification §6, minus the process.
    #[tokio::test]
    async fn a_restarted_daemon_rebuilds_its_cache_from_the_store() {
        let dir = tempfile::tempdir().expect("a temp dir for the test database");
        let key = Key::random().expect("a key for the test database");
        let path = dir.path().join("history.db");

        let first = Daemon::new(Store::open(&path, &key).expect("open"), Signals::discard())
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

    /// `Copy` is absent from this list on purpose: it refuses every id, known
    /// or not, until it can actually paste one.
    #[tokio::test]
    async fn an_unknown_id_is_an_argument_error_on_every_member_that_takes_one() {
        let fixture = Fixture::new();
        let missing = 4_242;
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
    /// path can see — the marker flavor need not even be stored — so it is read
    /// here rather than left to M4.
    #[tokio::test]
    async fn a_password_manager_copy_is_recorded_as_sensitive() {
        let fixture = Fixture::new();
        fixture
            .daemon
            .capture_at(
                Selection {
                    kind: SelectionKind::Clipboard,
                    advertised: vec![
                        "text/plain".to_owned(),
                        PASSWORD_MANAGER_HINT_MIME.to_owned(),
                    ],
                    flavors: vec![
                        Flavor::new("text/plain", "hunter2"),
                        Flavor::new(PASSWORD_MANAGER_HINT_MIME, "secret"),
                    ],
                    dropped: Vec::new(),
                },
                Timestamp::from_unix_millis(1_000),
            )
            .await;

        let entry = fixture.daemon.list(1, 0).await.unwrap().remove(0);
        assert!(entry.sensitive);
        assert_eq!(
            entry.kind, "text",
            "the marker carries no content of its own"
        );
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

    /// `Copy` cannot paste yet, so it must not report a success it did not
    /// have — and must not reorder the history to record a use that never
    /// happened. Both halves land together, next milestone.
    #[tokio::test]
    async fn copy_refuses_rather_than_reporting_a_paste_that_did_not_happen() {
        let fixture = Fixture::new();
        fixture.capture(&["first", "second"]).await;
        let oldest = fixture.daemon.search("first", 1).await.unwrap()[0].id;

        assert!(matches!(
            fixture.daemon.copy(oldest).await,
            Err(fdo::Error::NotSupported(_))
        ));
        assert_eq!(
            fixture.previews().await,
            ["second", "first"],
            "the history is untouched"
        );
        assert_eq!(fixture.emitted(), 2, "two captures and nothing else");
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
