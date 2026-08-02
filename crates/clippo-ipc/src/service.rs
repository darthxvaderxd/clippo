//! The served side: the `#[zbus::interface]` block, plus the trait a daemon
//! implements to fill it in.
//!
//! The interface lives here rather than in `clippod` so that the served
//! signatures and the proxied ones are written down once, next to each other.
//! [`ClippoInterface`] is therefore stateless glue: it owns an
//! `Arc<dyn ClippoBackend>` and forwards. All of the behaviour — the store, the
//! preview cache, the paused flag — is on the other side of that trait, in
//! `clippod`, where it can be tested without a bus.

use std::sync::Arc;

use async_trait::async_trait;
use zbus::object_server::SignalEmitter;
use zbus::{fdo, Connection};

use crate::{EntrySummary, OBJECT_PATH};

/// What a daemon has to be able to do to serve `com.nilfactor.Clippo`.
///
/// One method per member of DESIGN.md's table, in that order. Implementations
/// return [`fdo::Error`] because that is what crosses the bus: a
/// [`fdo::Error::InvalidArgs`] for an id that is not in the history, a
/// [`fdo::Error::Failed`] for a database that would not answer.
#[async_trait]
pub trait ClippoBackend: Send + Sync + 'static {
    /// The history, most recently used first. `limit` of 0 means "no limit".
    async fn list(&self, limit: u32, offset: u32) -> fdo::Result<Vec<EntrySummary>>;

    /// Fuzzy-match `query` against the previews, best match first. An empty
    /// query matches everything.
    async fn search(&self, query: &str, limit: u32) -> fdo::Result<Vec<EntrySummary>>;

    /// Put this entry back on the clipboard and move it to the front.
    async fn copy(&self, id: i64) -> fdo::Result<()>;

    /// Remove one entry, pinned or not.
    async fn delete(&self, id: i64) -> fdo::Result<()>;

    /// Pin or unpin an entry.
    async fn pin(&self, id: i64, pinned: bool) -> fdo::Result<()>;

    /// Empty the history, sparing pinned entries unless `include_pinned`.
    async fn clear(&self, include_pinned: bool) -> fdo::Result<()>;

    /// The full stored value of one entry. The only member that returns one.
    async fn reveal(&self, id: i64) -> fdo::Result<String>;

    /// Stop or resume recording new copies.
    async fn set_paused(&self, paused: bool) -> fdo::Result<()>;

    /// Whether recording is paused.
    async fn paused(&self) -> fdo::Result<bool>;
}

/// The object served at [`OBJECT_PATH`].
///
/// Construct one with [`ClippoInterface::new`] and hand it to
/// `zbus::connection::Builder::serve_at`.
pub struct ClippoInterface {
    backend: Arc<dyn ClippoBackend>,
}

impl ClippoInterface {
    /// Serve `com.nilfactor.Clippo` out of `backend`.
    pub fn new(backend: Arc<dyn ClippoBackend>) -> Self {
        Self { backend }
    }
}

/// Every method takes `&self`, so zbus runs calls concurrently and a slow
/// `Search` does not block a `Paused`. Whatever serialisation the state needs
/// is the backend's business.
#[zbus::interface(name = "com.nilfactor.Clippo")]
impl ClippoInterface {
    /// `List(limit, offset) -> Vec<EntrySummary>`.
    async fn list(&self, limit: u32, offset: u32) -> fdo::Result<Vec<EntrySummary>> {
        self.backend.list(limit, offset).await
    }

    /// `Search(query, limit) -> Vec<EntrySummary>`.
    async fn search(&self, query: &str, limit: u32) -> fdo::Result<Vec<EntrySummary>> {
        self.backend.search(query, limit).await
    }

    /// `Copy(id)`.
    async fn copy(&self, id: i64) -> fdo::Result<()> {
        self.backend.copy(id).await
    }

    /// `Delete(id)`.
    async fn delete(&self, id: i64) -> fdo::Result<()> {
        self.backend.delete(id).await
    }

    /// `Pin(id, bool)`.
    async fn pin(&self, id: i64, pinned: bool) -> fdo::Result<()> {
        self.backend.pin(id, pinned).await
    }

    /// `Clear(include_pinned)`.
    async fn clear(&self, include_pinned: bool) -> fdo::Result<()> {
        self.backend.clear(include_pinned).await
    }

    /// `Reveal(id) -> String`.
    async fn reveal(&self, id: i64) -> fdo::Result<String> {
        self.backend.reveal(id).await
    }

    /// `SetPaused(bool)`.
    async fn set_paused(&self, paused: bool) -> fdo::Result<()> {
        self.backend.set_paused(paused).await
    }

    /// `Paused() -> bool`.
    async fn paused(&self) -> fdo::Result<bool> {
        self.backend.paused().await
    }

    /// `HistoryChanged`, emitted after every mutation.
    ///
    /// An associated function rather than a method: the daemon emits this from
    /// its capture task, which has no reference to the served object, only a
    /// [`SignalEmitter`] from [`emitter`].
    #[zbus(signal)]
    pub async fn history_changed(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

/// A [`SignalEmitter`] for clippo's object path, for code that has a connection
/// but no handle on the served object.
pub fn emitter(connection: &Connection) -> zbus::Result<SignalEmitter<'static>> {
    SignalEmitter::new(connection, OBJECT_PATH)
}
