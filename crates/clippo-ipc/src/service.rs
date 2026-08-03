//! The served side: the `#[zbus::interface]` block, plus the trait a daemon
//! implements to fill it in.
//!
//! The interface lives here rather than in `clippod` so that the served
//! signatures and the proxied ones are written down once, next to each other.
//! [`ClippoInterface`] is therefore stateless glue: it owns an
//! `Arc<dyn ClippoBackend>` and forwards. All of the behaviour — the store, the
//! preview cache, the paused flag — is on the other side of that trait, in
//! `clippod`, where it can be tested without a bus.
//!
//! # The one member that inspects its caller
//!
//! With one exception, every member forwards without looking at the message
//! header, because the header carries nothing a *read of the user's own data*
//! should depend on. The exception is `Paste`, whose effect leaves clippo
//! entirely: it presses the user's paste shortcut into whatever window has
//! keyboard focus, so a peer that can call it can type a chosen clipboard entry
//! into a terminal. That one is checked against [`crate::peer`] before it is
//! forwarded — see [`ClippoInterface::with_policy`], and read `peer`'s own docs
//! on how weak a check it is.

use std::sync::Arc;

use async_trait::async_trait;
use tracing::{debug, warn};
use zbus::message::Header;
use zbus::object_server::SignalEmitter;
use zbus::{fdo, Connection};

use crate::peer::{self, PeerPolicy};
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
    ///
    /// The two halves go together: an implementation that could not hand the
    /// flavors to the compositor must not do the store half either, because a
    /// reordered history is a record of a paste that did not happen.
    ///
    /// Note that an implementation also *watches* the clipboard, and so hears
    /// its own copy-back come back round as a fresh selection. Keeping that out
    /// of the history is its problem to solve; `clippod` guards on the entry
    /// hash.
    async fn copy(&self, id: i64) -> fdo::Result<()>;

    /// [`copy`][Self::copy], and then press the user's paste shortcut into
    /// whatever has keyboard focus. Answers whether the key was pressed.
    ///
    /// The copy half must happen first and must still happen when the second
    /// half cannot: a compositor that will not synthesise keys still leaves the
    /// entry on the clipboard for the user to paste by hand, which is strictly
    /// what `Copy` would have given them. So this fails only for the reasons
    /// `Copy` fails, and a keystroke that was not sent comes back as `false`
    /// rather than as an error.
    ///
    /// `false` has three causes and a caller cannot tell them apart, because
    /// none of them changes what it should do: the user turned `auto_paste`
    /// off, the compositor offers no way to synthesise keys, or the attempt
    /// failed. The reason is in the daemon's journal. What `false` means to a
    /// frontend is the same in every case — the entry is on the clipboard and
    /// the user has not had it pasted for them.
    ///
    /// **Which window receives it is not knowable here.** Whatever has focus
    /// when the keystroke lands gets it, so a caller with a surface of its own
    /// on screen — the applet's picker — has to close it first, and even then
    /// is racing the compositor's focus handling. The daemon waits before
    /// pressing for exactly that reason.
    async fn paste(&self, id: i64) -> fdo::Result<bool>;

    /// Remove one entry, pinned or not.
    async fn delete(&self, id: i64) -> fdo::Result<()>;

    /// Pin or unpin an entry.
    async fn pin(&self, id: i64, pinned: bool) -> fdo::Result<()>;

    /// Empty the history, sparing pinned entries unless `include_pinned`.
    async fn clear(&self, include_pinned: bool) -> fdo::Result<()>;

    /// The full stored value of one entry. The only member that returns one.
    ///
    /// A masked entry's value comes out of here unmasked, which is the point:
    /// masking is display-only, and this is the display asking for the value
    /// on the user's explicit instruction. Everything else — `List`, `Search`,
    /// and anything added later that renders a row — gets
    /// [`EntrySummary::preview`][crate::EntrySummary::preview], which is a mask
    /// for a sensitive entry before it reaches the database.
    async fn reveal(&self, id: i64) -> fdo::Result<String>;

    /// The stored PNG thumbnail of an image entry.
    ///
    /// Unlike [`reveal`][Self::reveal] this is not a full value: the thumbnail
    /// is a downscale capture derived and stored precisely so that a frontend
    /// drawing a list never has to ask for the image itself.
    ///
    /// `NotSupported` for a non-image entry, and for an image stored without a
    /// thumbnail — capture skips one it could not generate rather than refusing
    /// the entry, so this failing is a normal thing for a frontend to handle.
    async fn thumbnail(&self, id: i64) -> fdo::Result<Vec<u8>>;

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
    /// Which executables may call `Paste`. Built once, at construction: the
    /// answer does not change while the daemon runs, and stat-ing four
    /// directories on every keystroke-driven call would be a syscall storm for
    /// a constant.
    callers: PeerPolicy,
}

impl ClippoInterface {
    /// Serve `com.nilfactor.Clippo` out of `backend`, admitting `Paste` only
    /// from the clippo binaries installed on this machine.
    pub fn new(backend: Arc<dyn ClippoBackend>) -> Self {
        Self::with_policy(backend, PeerPolicy::installed())
    }

    /// The same, with the caller allowlist chosen explicitly.
    ///
    /// Exists for tests, which are not run from an installed tree and would
    /// otherwise have to be one of clippo's own binaries to call `Paste` at
    /// all. [`new`][Self::new] is what `clippod` uses.
    pub fn with_policy(backend: Arc<dyn ClippoBackend>, callers: PeerPolicy) -> Self {
        Self { backend, callers }
    }

    /// Refuse a caller that is not one of clippo's own binaries.
    ///
    /// The refusal is `AccessDenied` rather than `Failed` so that a frontend
    /// can tell "clippo will not do this for you" apart from "clippo tried and
    /// could not", and it is logged with the pid and the exe — the journal line
    /// is the only record anybody gets that something on the bus went looking
    /// for a keystroke.
    async fn vet(
        &self,
        member: &str,
        header: &Header<'_>,
        connection: &Connection,
    ) -> fdo::Result<()> {
        let Some(sender) = header.sender() else {
            warn!(member, "clippo refused a call that carried no sender");
            return Err(fdo::Error::AccessDenied(format!(
                "clippo refuses {member}: {}",
                peer::PeerError::NoSender
            )));
        };

        match peer::check(connection, sender, &self.callers).await {
            Ok(caller) => {
                debug!(member, pid = caller.pid, exe = %caller.exe.display(), "clippo checked a caller");
                Ok(())
            }
            Err(error) => {
                // Pid and exe as their own fields as well as in the message, so
                // `journalctl -o json` can filter on them: this is the line an
                // operator greps for after the fact, and the one that says
                // which process to go and look at.
                warn!(
                    member,
                    sender = sender.as_str(),
                    pid = error.pid(),
                    exe = error.exe().map(|exe| exe.display().to_string()),
                    %error,
                    "clippo refused a caller it does not recognise"
                );
                Err(fdo::Error::AccessDenied(format!(
                    "clippo refuses {member} from a peer it does not recognise: {error}. \
                     {member} types into whatever window has keyboard focus, so clippod does it \
                     only for its own frontends"
                )))
            }
        }
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

    /// `Paste(id) -> bool`.
    ///
    /// The one member whose caller is inspected. `#[zbus(header)]` and
    /// `#[zbus(connection)]` are zbus's special arguments — they do not appear
    /// in the introspected signature and no caller passes them, so the wire
    /// contract is still `(x) -> b`.
    async fn paste(
        &self,
        id: i64,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<bool> {
        self.vet("Paste", &header, connection).await?;
        self.backend.paste(id).await
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

    /// `Thumbnail(id) -> Vec<u8>`.
    async fn thumbnail(&self, id: i64) -> fdo::Result<Vec<u8>> {
        self.backend.thumbnail(id).await
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
