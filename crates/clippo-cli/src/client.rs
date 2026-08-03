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
//!
//! # Asking who answered, before asking anything else
//!
//! [`Client::connect`] resolves the owner of `com.nilfactor.Clippo` and checks
//! it against [`clippo_ipc::peer`] before the first call. A well-known name is
//! not an identity: nothing stops another process on the session bus from
//! taking it while `clippod` is down, and everything below would then send that
//! process the user's search queries and print back whatever it answered with.
//! Read `peer`'s docs for how weak the check is — it is a speed bump — but a
//! frontend that never asks at all is the reason the takeover is invisible.

use clippo_ipc::peer::{Owner, PeerPolicy};
use clippo_ipc::{ClippoAppletProxyBlocking, ClippoProxyBlocking, EntrySummary, BUS_NAME};
use zbus::blocking::Connection;

use crate::error::CliError;
use crate::ids;

/// A connection to `com.nilfactor.Clippo` on the session bus.
pub struct Client {
    proxy: ClippoProxyBlocking<'static>,
    connection: Connection,
}

impl Client {
    /// Connect to the session bus and point a proxy at the daemon.
    ///
    /// This succeeds whether or not `clippod` is running: nothing is sent
    /// until the first call, and it is that call which reports an absent
    /// daemon. Failing here means there is no session bus at all — or that
    /// something clippo does not recognise is holding the daemon's name, which
    /// is a refusal rather than an error in the connection.
    pub fn connect() -> Result<Self, CliError> {
        Self::connect_to(BUS_NAME, &PeerPolicy::installed())
    }

    /// [`connect`][Self::connect], against a chosen name and allowlist.
    ///
    /// Split out for the tests. They need both halves parameterised: a test
    /// binary is not one of clippo's own, so it could never be a *trusted*
    /// owner under the real policy, and it must not take the real
    /// `com.nilfactor.Clippo` either — the whole suite runs on one bus, and a
    /// test that squatted on the daemon's name would fail whichever other test
    /// happened to want it.
    fn connect_to(bus_name: &str, policy: &PeerPolicy) -> Result<Self, CliError> {
        let connection = Connection::session().map_err(CliError::from_connect)?;

        // Nobody home is not a refusal: the daemon may simply not be running,
        // and the first call is what says so with the sentence that names
        // `systemctl`. Anything else that fails the check *is* a refusal, and
        // nothing is sent to it.
        match clippo_ipc::peer::owner_of_blocking(&connection, bus_name, policy)
            .map_err(CliError::from_connect)?
        {
            Owner::Absent | Owner::Trusted(_) => {}
            Owner::Untrusted(why) => return Err(CliError::DaemonNotTrusted(why.to_string())),
        }

        let proxy = ClippoProxyBlocking::builder(&connection)
            .destination(bus_name.to_owned())
            .map_err(CliError::from_connect)?
            .build()
            .map_err(CliError::from_connect)?;
        Ok(Self { proxy, connection })
    }

    /// `Toggle()` on the *applet's* interface, not the daemon's.
    ///
    /// The one call in this file that goes somewhere other than `clippod`, and
    /// the proxy is built here rather than in [`connect`][Self::connect]
    /// because every other subcommand would pay for it without using it. It
    /// shares the connection: one process needs one bus connection, whichever
    /// names it happens to talk to.
    ///
    /// The check runs here too, against the applet's name. A `Toggle` carries
    /// no data, so this is the weakest of the three call sites — but what it
    /// asks for is a picker on the user's screen, and a picker drawn by
    /// somebody else is a convincing place to type.
    pub fn toggle_applet(&self) -> Result<(), CliError> {
        match clippo_ipc::peer::owner_of_blocking(
            &self.connection,
            clippo_ipc::APPLET_BUS_NAME,
            &PeerPolicy::installed(),
        )
        .map_err(CliError::from_connect)?
        {
            Owner::Absent | Owner::Trusted(_) => {}
            Owner::Untrusted(why) => return Err(CliError::AppletNotTrusted(why.to_string())),
        }

        let applet = ClippoAppletProxyBlocking::new(&self.connection)
            .map_err(|error| CliError::from_applet_call("Toggle", error))?;
        applet
            .toggle()
            .map_err(|error| CliError::from_applet_call("Toggle", error))
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

    /// `Paste(id)`, answering whether the shortcut was actually pressed.
    pub fn paste(&self, id: i64) -> Result<bool, CliError> {
        self.proxy
            .paste(id)
            .map_err(|error| CliError::from_call("Paste", error))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A name of this test's own, so that a suite running on one shared bus
    /// never has two tests fighting over `com.nilfactor.Clippo`.
    const SCRATCH: &str = "com.nilfactor.ClippoCliOwnerTest";

    /// Whether there is a session bus to talk to. Mirrors the round-trip
    /// test's: `just test` and CI run the suite under `dbus-run-session`.
    fn has_session_bus() -> bool {
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
            return true;
        }
        eprintln!(
            "skipping: no DBUS_SESSION_BUS_ADDRESS. Run the suite under `dbus-run-session -- \
             cargo test`, as `just test` and CI do."
        );
        false
    }

    /// The refusal, against a real owner on a real bus.
    ///
    /// This test process genuinely holds the name and is genuinely not a clippo
    /// binary, which is the impostor's exact position: `connect_to` has to
    /// resolve it, read its `/proc/<pid>/exe`, and decline — before a proxy
    /// exists to send anything through.
    #[test]
    fn connecting_refuses_an_owner_that_is_not_a_clippo_binary() {
        if !has_session_bus() {
            return;
        }

        let squatter = Connection::session().expect("a second connection");
        squatter.request_name(SCRATCH).expect("the scratch name");

        let error = Client::connect_to(SCRATCH, &PeerPolicy::from_paths([]))
            .err()
            .expect("an unrecognised owner must be refused");
        assert!(matches!(error, CliError::DaemonNotTrusted(_)), "{error:?}");

        // What the user reads has to name the process, or there is nothing to
        // go and look at.
        let printed = error.to_string();
        assert!(
            printed.contains(&std::process::id().to_string()),
            "{printed}"
        );
        assert!(printed.contains("Nothing was sent"), "{printed}");
    }

    /// And the same owner against a list it is on. Same bus, same pid, opposite
    /// answer — so the refusal above is the check working rather than
    /// `connect_to` refusing everything.
    #[test]
    fn connecting_accepts_an_owner_that_is_on_the_list() {
        if !has_session_bus() {
            return;
        }

        let squatter = Connection::session().expect("a second connection");
        // A name per test: two `#[test]`s in one binary run on two threads.
        let name = format!("{SCRATCH}Allowed");
        squatter.request_name(name.as_str()).expect("the name");

        let me = std::env::current_exe().expect("this test process has an exe");
        Client::connect_to(&name, &PeerPolicy::from_paths([me]))
            .expect("an allowlisted owner must be accepted");
    }

    /// Nothing owning the name is not a refusal. `clippod` not running is the
    /// most common state this code is in, and it has to reach the first call so
    /// the user gets the sentence about `systemctl` rather than a warning about
    /// an impostor that is not there.
    #[test]
    fn connecting_to_a_name_nobody_owns_is_not_a_refusal() {
        if !has_session_bus() {
            return;
        }

        Client::connect_to(
            "com.nilfactor.ClippoNobodyOwnsThis",
            &PeerPolicy::from_paths([]),
        )
        .expect("an absent daemon is not an untrusted one");
    }
}
