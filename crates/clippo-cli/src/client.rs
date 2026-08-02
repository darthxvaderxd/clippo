//! The daemon, as the CLI sees it: one blocking proxy and one method per
//! member.
//!
//! Blocking rather than async on purpose. `clippo copy 2` makes one or two
//! calls and exits; an async runtime would be a dependency, a nested-executor
//! question and a slower start, in exchange for concurrency this process has no
//! use for. `clippo-ipc` generates the blocking proxy from the same
//! declarations as the async one the applet will use, so nothing is restated.
//!
//! Every method is three lines — call, tag the failure with the member name,
//! hand back — because that tagging is the only thing this layer adds. The
//! commands themselves live in [`crate::run`].

use clippo_ipc::{ClippoProxyBlocking, EntrySummary};
use zbus::blocking::Connection;

use crate::error::CliError;
use crate::ids;

/// A connection to `com.nilfactor.Clippo` on the session bus.
pub struct Client {
    proxy: ClippoProxyBlocking<'static>,
}

impl Client {
    /// Connect to the session bus and point a proxy at the daemon.
    ///
    /// This succeeds whether or not `clippod` is running: nothing is sent
    /// until the first call, and it is that call which reports an absent
    /// daemon. Failing here means there is no session bus at all, which is a
    /// different problem with a different fix.
    pub fn connect() -> Result<Self, CliError> {
        let connection = Connection::session().map_err(CliError::from_connect)?;
        let proxy = ClippoProxyBlocking::new(&connection).map_err(CliError::from_connect)?;
        Ok(Self { proxy })
    }

    /// `List(limit, offset)`. A limit of 0 means the whole history.
    pub fn list(&self, limit: u32, offset: u32) -> Result<Vec<EntrySummary>, CliError> {
        self.proxy
            .list(limit, offset)
            .map_err(|error| CliError::from_call("List", error))
    }

    /// `Search(query, limit)`.
    pub fn search(&self, query: &str, limit: u32) -> Result<Vec<EntrySummary>, CliError> {
        self.proxy
            .search(query, limit)
            .map_err(|error| CliError::from_call("Search", error))
    }

    /// `Copy(id)`.
    pub fn copy(&self, id: i64) -> Result<(), CliError> {
        self.proxy
            .copy(id)
            .map_err(|error| CliError::from_call("Copy", error))
    }

    /// `Delete(id)`.
    pub fn delete(&self, id: i64) -> Result<(), CliError> {
        self.proxy
            .delete(id)
            .map_err(|error| CliError::from_call("Delete", error))
    }

    /// `Pin(id, pinned)`.
    pub fn pin(&self, id: i64, pinned: bool) -> Result<(), CliError> {
        self.proxy
            .pin(id, pinned)
            .map_err(|error| CliError::from_call("Pin", error))
    }

    /// `Clear(include_pinned)`.
    pub fn clear(&self, include_pinned: bool) -> Result<(), CliError> {
        self.proxy
            .clear(include_pinned)
            .map_err(|error| CliError::from_call("Clear", error))
    }

    /// `Reveal(id)` — the only call that returns a whole stored value.
    pub fn reveal(&self, id: i64) -> Result<String, CliError> {
        self.proxy
            .reveal(id)
            .map_err(|error| CliError::from_call("Reveal", error))
    }

    /// `SetPaused(paused)`.
    pub fn set_paused(&self, paused: bool) -> Result<(), CliError> {
        self.proxy
            .set_paused(paused)
            .map_err(|error| CliError::from_call("SetPaused", error))
    }

    /// `Paused()`.
    pub fn paused(&self) -> Result<bool, CliError> {
        self.proxy
            .paused()
            .map_err(|error| CliError::from_call("Paused", error))
    }

    /// The id one typed reference names.
    ///
    /// Resolution needs every id, not a page of them, so this asks for the
    /// whole history: an ambiguous prefix has to be able to see all of the
    /// entries it could have meant. Use [`Client::resolve_all`] when several
    /// references are being resolved at once.
    pub fn resolve(&self, typed: &str) -> Result<i64, CliError> {
        let entries = self.list(0, 0)?;
        Ok(ids::resolve(typed, &entries)?)
    }

    /// Several typed references, all against the same history.
    ///
    /// One `List` for the lot, and every reference resolved before anything is
    /// deleted: `clippo rm 1 2 zz` fails without having deleted entries 1 and
    /// 2, and no reference is ever resolved against a history that an earlier
    /// argument in the same command has already changed. Two references naming
    /// the same entry come back as one id — see [`ids::resolve_all`].
    pub fn resolve_all(&self, typed: &[String]) -> Result<Vec<i64>, CliError> {
        let entries = self.list(0, 0)?;
        Ok(ids::resolve_all(typed, &entries)?)
    }
}
