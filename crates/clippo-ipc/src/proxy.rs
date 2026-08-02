//! The calling side: `ClippoProxy`, generated from the same member list the
//! daemon serves.
//!
//! Frontends never build a method call by hand. `clippo list` and the applet's
//! popup both go through this proxy, so a member that is renamed or retyped in
//! [`crate::service`] fails to compile here rather than at the moment a user
//! clicks something.

use crate::EntrySummary;

/// A handle on a running `clippod`.
///
/// The defaults point at the daemon's well-known name and object path, so
/// `ClippoProxy::new(&connection)` is all a frontend needs.
///
/// Every method is an ordinary D-Bus call and can therefore fail with
/// `ServiceUnknown` when no daemon is running — worth reporting as "clippod is
/// not running" rather than as a raw bus error, because that is the one failure
/// a user can act on.
#[zbus::proxy(
    interface = "com.nilfactor.Clippo",
    default_service = "com.nilfactor.Clippo",
    default_path = "/com/nilfactor/Clippo"
)]
pub trait Clippo {
    /// The history, most recently used first.
    ///
    /// `limit` of 0 means "no limit". Previews only — see [`EntrySummary`].
    fn list(&self, limit: u32, offset: u32) -> zbus::Result<Vec<EntrySummary>>;

    /// Fuzzy-match `query` against every preview the daemon holds, best match
    /// first.
    ///
    /// An empty query matches everything, so `Search("")` is `List` with a
    /// limit and no offset.
    fn search(&self, query: &str, limit: u32) -> zbus::Result<Vec<EntrySummary>>;

    /// Put this entry back on the clipboard and move it to the front of the
    /// history.
    fn copy(&self, id: i64) -> zbus::Result<()>;

    /// Remove one entry, pinned or not.
    fn delete(&self, id: i64) -> zbus::Result<()>;

    /// Pin or unpin an entry, exempting it from retention and from an ordinary
    /// [`clear`](ClippoProxy::clear).
    fn pin(&self, id: i64, pinned: bool) -> zbus::Result<()>;

    /// Empty the history. Pinned entries survive unless `include_pinned`.
    fn clear(&self, include_pinned: bool) -> zbus::Result<()>;

    /// The full stored value of one entry, on explicit user action.
    ///
    /// The only member that returns unabridged content; from M4 previews are
    /// masked and this is how a user sees the real thing. Frontends must not
    /// cache the result.
    fn reveal(&self, id: i64) -> zbus::Result<String>;

    /// Stop or resume recording new copies. Reads keep working either way.
    fn set_paused(&self, paused: bool) -> zbus::Result<()>;

    /// Whether recording is currently paused.
    fn paused(&self) -> zbus::Result<bool>;

    /// Emitted after every change to the history, so a frontend never polls.
    ///
    /// Carries no payload deliberately: a signal that described the change
    /// would be a second, lossier copy of the history for a listener to get out
    /// of step with. Re-read with [`list`](ClippoProxy::list) or
    /// [`search`](ClippoProxy::search).
    #[zbus(signal)]
    fn history_changed(&self) -> zbus::Result<()>;
}
