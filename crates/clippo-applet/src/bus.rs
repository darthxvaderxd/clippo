//! The applet's whole relationship with D-Bus, in one background task.
//!
//! Two directions cross this module, and keeping them in one place is what
//! keeps [`crate::app`] synchronous and testable:
//!
//! - **Out** — [`Request`]s the UI makes: search, paste, delete, pin,
//!   reveal, thumbnail. Each is one call on the same `com.nilfactor.Clippo`
//!   proxy the CLI uses. M5 requires there be no second code path for these,
//!   and there is not one: `clippo pin 3` and `Ctrl+P` reach the identical
//!   member.
//! - **In** — [`Event`]s the UI reacts to: answers to those calls, the
//!   daemon's `HistoryChanged`, the daemon appearing or disappearing, and
//!   `Toggle` arriving on the applet's *own* interface from `clippo show`.
//!
//! # Nothing here polls
//!
//! Both of the things that change on their own are signals:
//!
//! - `HistoryChanged` is why a copy made in another window shows up while the
//!   popup is open.
//! - `NameOwnerChanged`, filtered to the daemon's name, is how the applet
//!   notices `clippod` stopping and starting. It is also why reconnection needs
//!   no code: a zbus signal stream is a match rule held by the *bus*, and a
//!   proxy is a name and a path rather than a socket to the service. Both
//!   outlive the daemon they refer to, so a restarted `clippod` starts being
//!   answered again with nothing rebuilt — the applet only has to notice, so it
//!   can refresh a list that went stale while the daemon was away.
//!
//! # Reconnection is not automatically good news
//!
//! That last property has a sharp edge, and it is the reason
//! [`clippo_ipc::peer`] is called from here. `com.nilfactor.Clippo` is a
//! *name*, not an identity: any peer on the session bus can take it while
//! `clippod` is down, and a proxy that "starts being answered again" would then
//! be answered by that peer. Since the applet sends the whole search query to
//! `Search` on every keystroke, and draws whatever comes back, an unchecked
//! reconnection turns the search box into a keylogger and the list into
//! somebody else's.
//!
//! So the owner is checked at connect time and again on **every**
//! `NameOwnerChanged` that gives the name an owner. An owner that fails is not
//! merely not talked to — it becomes [`Event::DaemonUntrusted`], which the
//! picker draws. Silence would leave the applet looking normal, which is
//! exactly what makes the takeover invisible.
//!
//! What it does not close, said plainly: the proxy is addressed to the
//! well-known name, so a request already in flight when the name changes hands
//! is delivered to whoever holds it on arrival. Closing that would mean
//! addressing the proxy to the *unique* name and rebuilding it on every
//! restart, which is the property two paragraphs up. Combined with everything
//! [`clippo_ipc::peer`] admits about itself, this is a speed bump. It is not a
//! boundary and must not be read as one.
//!
//! # Failing to own the applet name is not fatal
//!
//! If `com.nilfactor.ClippoApplet` is already owned — a second applet instance,
//! most likely — the panel icon and its popup still work. Only `clippo show`
//! goes to the other instance. Refusing to start would turn a duplicated panel
//! applet into a missing one.

use std::sync::Arc;

use async_trait::async_trait;
use clippo_ipc::peer::{Owner, PeerPolicy};
use clippo_ipc::{
    AppletFrontend, AppletInterface, ClippoProxy, EntrySummary, APPLET_BUS_NAME,
    APPLET_OBJECT_PATH, BUS_NAME,
};
use cosmic::iced::futures::{SinkExt, Stream, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use zbus::fdo;
use zbus::names::OwnedUniqueName;
use zeroize::Zeroizing;

use crate::model::EntryKey;

/// How many rows the popup asks for.
///
/// A cap rather than the whole history: the list is keyboard-driven and nobody
/// arrows through five hundred rows, while every row is a preview string over
/// the bus. `Search` ranks before it truncates, so the best matches are inside
/// this regardless of how big the history is.
pub const ROW_LIMIT: u32 = 200;

/// Something the UI wants the daemon to do.
#[derive(Clone)]
pub enum Request {
    /// Re-read the list for this query. An empty query is the whole history.
    Refresh(String),
    /// `Paste(id)` — copy, and have the daemon press the user's paste
    /// shortcut into whatever has focus once the picker is gone.
    ///
    /// There is no `Copy` beside this because nothing in the applet wants one:
    /// `Paste` copies first and always, so choosing an entry puts it on the
    /// clipboard whether or not the keystroke can be synthesised.
    Paste(i64),
    /// `Delete(id)`.
    Delete(i64),
    /// `Pin(id, pinned)`.
    Pin(i64, bool),
    /// `Reveal(id)` — the full value of one entry, on the user's instruction.
    Reveal(i64),
    /// `Thumbnail(id)` — the stored PNG for an image row.
    ///
    /// Carries the whole [`EntryKey`] rather than the id the member takes, so
    /// that the answer can be filed under a key that cannot have been reissued
    /// to a different entry in the meantime.
    Thumbnail(EntryKey),
}

/// Hand-written for the same reason [`Event`]'s is, one step weaker:
/// [`Request::Refresh`] carries what the user typed into the search field.
/// That is not clipboard content, but it is the one string in this pair that
/// comes from the keyboard — somebody searching for a fragment of a password
/// has typed a fragment of a password — and a derived `Debug` would put it in
/// the journal on the first `debug!(?request)` anybody adds.
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Request::Refresh(query) => write!(f, "Refresh({} chars)", query.chars().count()),
            Request::Paste(id) => write!(f, "Paste({id})"),
            Request::Delete(id) => write!(f, "Delete({id})"),
            Request::Pin(id, pinned) => write!(f, "Pin({id}, {pinned})"),
            Request::Reveal(id) => write!(f, "Reveal({id})"),
            Request::Thumbnail(key) => write!(f, "Thumbnail({key:?})"),
        }
    }
}

/// Something that happened, for the UI to fold in.
#[derive(Clone)]
pub enum Event {
    /// The worker is up; this is how the UI sends it [`Request`]s. Emitted
    /// once, before anything else.
    Ready(mpsc::Sender<Request>),
    /// The answer to a [`Request::Refresh`], already ranked by the daemon.
    ///
    /// Carries the query it was asked for, because two refreshes can be in
    /// flight at once — a keystroke sends one per character — and the UI has to
    /// be able to tell the answer it is waiting for from one it has already
    /// moved on from. See [`Model::accepts`][crate::model::Model::accepts].
    Entries(String, Vec<EntrySummary>),
    /// The answer to a [`Request::Reveal`]. Held only as long as its row stays
    /// focused — see [`crate::model`].
    ///
    /// [`Zeroizing`] from the moment it arrives rather than only once the model
    /// has it: this value is cloned by the runtime on its way through, and a
    /// plain `String` would leave every intermediate copy in freed memory for a
    /// core dump or a swap file to pick up. It cannot cover zbus's own
    /// deserialisation buffer, which is the one hop this crate does not own.
    Revealed(i64, Zeroizing<String>),
    /// The answer to a [`Request::Thumbnail`], or `None` when the entry has no
    /// thumbnail to give.
    ///
    /// Reported either way. The `None` is not just for the UI's benefit — it is
    /// what tells the applet a slot in the request queue has come free, so a
    /// list with more image rows than the queue holds finishes fetching instead
    /// of stopping at the first refusal.
    Thumbnail(EntryKey, Option<Vec<u8>>),
    /// `clippo show` called `Toggle` on the applet's interface.
    Toggle,
    /// The daemon is answering. Carries no data; the UI asks for a refresh.
    DaemonUp,
    /// The daemon is not answering.
    DaemonDown,
    /// Something owns the daemon's name and it is not a clippo binary.
    ///
    /// Carries the reason, because the picker prints it: a user looking at a
    /// panel applet has no journal in front of them, and "clippod is not
    /// running" would be a lie that hid the interesting case. Distinct from
    /// [`DaemonDown`][Self::DaemonDown] for the same reason — one of them is a
    /// daemon to start and the other is a process to look at.
    DaemonUntrusted(String),
    /// A call failed for a reason the journal should have.
    ///
    /// Not on screen: the [`Model`][crate::model::Model] has nowhere to put a
    /// per-call failure, and the two failures a user can act on — no daemon and
    /// no results — have states of their own. Anything else is a refusal on one
    /// member, which is logged with the member's name.
    Failed(String),
}

/// Hand-written so that a stray `debug!(?event)` cannot put a revealed password
/// in the journal.
///
/// The precedent is M4's `Debug` on `clippo_core::Entry`, which prints a hash
/// prefix rather than the hash. Same reasoning, stronger case: this variant
/// carries the plaintext itself, and it is reachable from `Message`'s derived
/// `Debug` through `Message::Bus`.
impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Event::Ready(_) => f.write_str("Ready"),
            // The row count and not the query, for the reason `Request`'s own
            // `Debug` does not print it either.
            Event::Entries(_, entries) => write!(f, "Entries({} rows)", entries.len()),
            Event::Revealed(id, value) => {
                write!(f, "Revealed({id}, {} chars)", value.chars().count())
            }
            Event::Thumbnail(key, Some(bytes)) => {
                write!(f, "Thumbnail({key:?}, {} bytes)", bytes.len())
            }
            Event::Thumbnail(key, None) => write!(f, "Thumbnail({key:?}, none)"),
            Event::Toggle => f.write_str("Toggle"),
            Event::DaemonUp => f.write_str("DaemonUp"),
            Event::DaemonDown => f.write_str("DaemonDown"),
            Event::DaemonUntrusted(why) => write!(f, "DaemonUntrusted({why:?})"),
            Event::Failed(message) => write!(f, "Failed({message:?})"),
        }
    }
}

/// The served half of `com.nilfactor.ClippoApplet`.
///
/// Forwarding rather than acting: this runs on the bus task, and the popup is
/// owned by the UI thread, so the only correct thing to do here is hand the
/// request over and return. See [`AppletFrontend::toggle`] on why returning
/// `Ok` is not a claim that a surface is on screen.
struct Toggler {
    toggles: mpsc::Sender<()>,
}

#[async_trait]
impl AppletFrontend for Toggler {
    async fn toggle(&self) -> fdo::Result<()> {
        self.toggles
            .send(())
            .await
            .map_err(|_| fdo::Error::Failed("the applet is shutting down".to_owned()))
    }
}

/// Where this task puts the [`Event`]s the UI folds in.
///
/// Named because `stream::channel` takes an `AsyncFnOnce`, and the item type of
/// an `AsyncFnOnce` argument is not inferred from the bound — the closure's
/// parameter has to say what it is or the whole call is ambiguous.
type Outbox = cosmic::iced::futures::channel::mpsc::Sender<Event>;

/// The subscription the applet runs for its whole life.
///
/// Returns a stream rather than taking a callback so that [`crate::app`] can
/// hand it straight to `Subscription::run`.
pub fn worker() -> impl Stream<Item = Event> {
    cosmic::iced::stream::channel(32, |mut output: Outbox| async move {
        let (requests, mut inbox) = mpsc::channel::<Request>(64);
        let (toggles, mut toggle_inbox) = mpsc::channel::<()>(8);

        // Before anything can fail: without this the UI has no way to ask for
        // its first list, and an applet that could not reach the bus would
        // never draw a row even after the bus came back.
        if output.send(Event::Ready(requests)).await.is_err() {
            return;
        }

        let connection = match zbus::Connection::session().await {
            Ok(connection) => connection,
            Err(error) => {
                warn!(%error, "clippo-applet could not reach the session bus");
                let _ = output
                    .send(Event::Failed(format!("no session bus: {error}")))
                    .await;
                let _ = output.send(Event::DaemonDown).await;
                return;
            }
        };

        serve_toggle(&connection, toggles).await;

        let clippo = match ClippoProxy::new(&connection).await {
            Ok(clippo) => clippo,
            Err(error) => {
                warn!(%error, "clippo-applet could not build the daemon proxy");
                let _ = output.send(Event::DaemonDown).await;
                return;
            }
        };

        // Both streams are match rules on the bus, not connections to the
        // daemon: they are created once and survive `clippod` restarting.
        let mut history = match clippo.receive_history_changed().await {
            Ok(stream) => stream,
            Err(error) => {
                warn!(%error, "clippo-applet could not subscribe to HistoryChanged");
                let _ = output
                    .send(Event::Failed(format!("no live updates: {error}")))
                    .await;
                // Like the two exits above it. This one takes the request
                // receiver down with it, so every later action is dropped —
                // and without this the applet would keep its optimistic
                // `Connected` state and draw "Nothing copied yet", which is
                // precisely the "your history is gone" reading the explicit
                // no-daemon state exists to prevent.
                let _ = output.send(Event::DaemonDown).await;
                return;
            }
        };
        let mut owners = watch_name(&connection, BUS_NAME).await;
        let policy = PeerPolicy::installed();

        // The daemon may have been running before the applet was; the signal
        // only fires on a *change*, so the starting state has to be asked for.
        // And it is asked as "who owns it", not "does anybody" — see the module
        // docs.
        let starting = daemon_owner(&connection, BUS_NAME, &policy).await;
        let mut trusted = matches!(starting, Event::DaemonUp);
        let _ = output.send(starting).await;

        loop {
            tokio::select! {
                Some(request) = inbox.recv() => {
                    // The refusal has teeth here rather than in the UI: an
                    // untrusted owner is sent nothing at all, whatever the
                    // picker happens to be drawing at the time.
                    if !trusted {
                        warn!(?request, "clippo-applet dropped a request; the daemon's name is held by a peer it does not recognise");
                        continue;
                    }
                    if serve_request(&clippo, request, &mut output).await.is_err() {
                        return;
                    }
                }
                Some(_) = history.next() => {
                    debug!("clippo-applet heard HistoryChanged");
                    // Only from an owner already checked. An impostor can emit
                    // this signal too, and acting on it would refresh a list out
                    // of a peer this applet has decided not to talk to.
                    if trusted && output.send(Event::DaemonUp).await.is_err() {
                        return;
                    }
                }
                Some(owner) = next_owner(&mut owners) => {
                    let event = match owner {
                        // Every appearance is re-checked, not just the first:
                        // the name changing hands mid-session is the whole
                        // attack, and a check that ran once at startup would
                        // miss it by construction.
                        Some(name) => judged(clippo_ipc::peer::check(&connection, &name, &policy).await),
                        None => Event::DaemonDown,
                    };
                    info!(state = ?event, "clippo-applet saw clippod change state");
                    trusted = matches!(event, Event::DaemonUp);
                    if output.send(event).await.is_err() {
                        return;
                    }
                }
                Some(()) = toggle_inbox.recv() => {
                    if output.send(Event::Toggle).await.is_err() {
                        return;
                    }
                }
                else => return,
            }
        }
    })
}

/// Export `Toggle` and take the applet's name, tolerating failure at both.
async fn serve_toggle(connection: &zbus::Connection, toggles: mpsc::Sender<()>) {
    let frontend: Arc<dyn AppletFrontend> = Arc::new(Toggler { toggles });
    if let Err(error) = connection
        .object_server()
        .at(APPLET_OBJECT_PATH, AppletInterface::new(frontend))
        .await
    {
        warn!(%error, "clippo-applet could not export its Toggle interface; `clippo show` will not work");
        return;
    }

    match connection.request_name(APPLET_BUS_NAME).await {
        Ok(()) => info!(name = APPLET_BUS_NAME, "clippo-applet is serving Toggle"),
        Err(error) => warn!(
            %error,
            name = APPLET_BUS_NAME,
            "clippo-applet could not take its bus name; another instance has it, and \
             `clippo show` will reach that one"
        ),
    }
}

/// A stream of "who owns this name now" changes.
///
/// The name is a parameter only so the tests can watch one of their own: the
/// whole suite runs on a single bus, and a test that took
/// `com.nilfactor.Clippo` would fail whichever other test wanted it. Everything
/// in this file passes [`BUS_NAME`].
async fn watch_name(
    connection: &zbus::Connection,
    name: &str,
) -> Option<fdo::NameOwnerChangedStream> {
    let dbus = match fdo::DBusProxy::new(connection).await {
        Ok(dbus) => dbus,
        Err(error) => {
            warn!(%error, "clippo-applet cannot watch for clippod restarting");
            return None;
        }
    };
    // Filtered at the bus rather than here: every name change in the session
    // would otherwise wake this task, which on a busy desktop is a lot of
    // wakeups to discard.
    match dbus
        .receive_name_owner_changed_with_args(&[(0, name)])
        .await
    {
        Ok(stream) => Some(stream),
        Err(error) => {
            warn!(%error, "clippo-applet cannot watch for clippod restarting");
            None
        }
    }
}

/// The unique name that now owns the daemon's name, or `None` when nothing
/// does.
///
/// The *name* rather than a bare "is it there": whoever holds it is the peer
/// the applet is about to send search queries to, and the only chance to ask
/// who that is comes with this signal.
///
/// Pending forever when the watch could not be set up, so that
/// [`tokio::select`] simply never picks this branch instead of spinning on a
/// stream that is immediately done.
async fn next_owner(
    owners: &mut Option<fdo::NameOwnerChangedStream>,
) -> Option<Option<OwnedUniqueName>> {
    let stream = match owners.as_mut() {
        Some(stream) => stream,
        None => std::future::pending().await,
    };
    let signal = stream.next().await?;
    let args = signal.args().ok()?;
    Some(args.new_owner().as_ref().map(|name| name.to_owned().into()))
}

/// Who owns the daemon's name right now, checked.
///
/// Not "does anybody own it". The distinction is the whole of F5: a name with
/// an owner is not a daemon, and the applet has no other moment at which it
/// could find that out before it starts typing into one.
async fn daemon_owner(connection: &zbus::Connection, name: &str, policy: &PeerPolicy) -> Event {
    match clippo_ipc::peer::owner_of(connection, name, policy).await {
        Ok(Owner::Absent) => Event::DaemonDown,
        Ok(Owner::Trusted(peer)) => {
            debug!(pid = peer.pid, exe = %peer.exe.display(), "clippo-applet checked the daemon");
            Event::DaemonUp
        }
        Ok(Owner::Untrusted(why)) => untrusted(why),
        // Asking failed, so the answer is unknown — and an unknown peer is one
        // this applet does not talk to. The alternative is trusting on a bus
        // error, which is the one direction that must not be the default.
        Err(error) => {
            warn!(%error, "clippo-applet could not find out who owns the daemon's name");
            Event::DaemonDown
        }
    }
}

/// One peer check into one [`Event`], so the connect-time path and the
/// `NameOwnerChanged` path cannot come to different conclusions.
fn judged(checked: Result<clippo_ipc::Peer, clippo_ipc::PeerError>) -> Event {
    match checked {
        Ok(peer) => {
            debug!(pid = peer.pid, exe = %peer.exe.display(), "clippo-applet checked the daemon");
            Event::DaemonUp
        }
        Err(why) => untrusted(why),
    }
}

/// A refused owner, logged and turned into something the picker can draw.
fn untrusted(why: clippo_ipc::PeerError) -> Event {
    warn!(%why, "clippo-applet refuses to talk to the peer holding the daemon's name");
    Event::DaemonUntrusted(why.to_string())
}

/// Make one call and report what came back.
///
/// Every arm reports failure the same way: a failed call is the only evidence
/// the applet gets that the daemon went away between the last signal and now,
/// so it lowers the connection state rather than being swallowed.
async fn serve_request(
    clippo: &ClippoProxy<'_>,
    request: Request,
    output: &mut Outbox,
) -> Result<(), ()> {
    let event = match request {
        Request::Refresh(query) => match clippo.search(&query, ROW_LIMIT).await {
            // `Search` rather than `List` even for an empty query: an empty
            // query matches everything, so this is `List` with a limit, and
            // routing both through one member is what stops the applet's
            // unfiltered order drifting from `clippo search`'s.
            Ok(entries) => Event::Entries(query, entries),
            Err(error) => down("Search", &error),
        },
        // The answer says whether the daemon actually pressed the key. The
        // applet does the same thing either way — the picker is already closed
        // and the entry is on the clipboard — and it has nowhere to report it,
        // so it is dropped here rather than carried into a `Message` nothing
        // would read. `clippod` logs the reason.
        Request::Paste(id) => match clippo.paste(id).await {
            Ok(_pressed) => Event::DaemonUp,
            Err(error) => down("Paste", &error),
        },
        Request::Delete(id) => match clippo.delete(id).await {
            Ok(()) => Event::DaemonUp,
            Err(error) => down("Delete", &error),
        },
        Request::Pin(id, pinned) => match clippo.pin(id, pinned).await {
            Ok(()) => Event::DaemonUp,
            Err(error) => down("Pin", &error),
        },
        Request::Reveal(id) => match clippo.reveal(id).await {
            Ok(value) => Event::Revealed(id, Zeroizing::new(value)),
            Err(error) => down("Reveal", &error),
        },
        Request::Thumbnail(key) => match clippo.thumbnail(key.id).await {
            Ok(bytes) => Event::Thumbnail(key, Some(bytes)),
            // Not `down`: an image stored without a thumbnail is a normal
            // answer, not a sick daemon, and treating it as one would drop the
            // whole list into the error state over one undecodable screenshot.
            // Still reported, because the UI is counting replies to know when
            // it can queue the next request.
            Err(error) => {
                debug!(id = key.id, %error, "no thumbnail for this entry");
                Event::Thumbnail(key, None)
            }
        },
    };

    output.send(event).await.map_err(|_| ())
}

/// A failed call, as an event.
///
/// Which failures mean "no daemon" is [`clippo_ipc::is_service_absent`]'s to
/// say. The CLI has to tell the same two apart to print `clippod is not
/// running`, and a second copy of the name list here is one that can drift.
fn down(member: &str, error: &zbus::Error) -> Event {
    warn!(member, %error, "clippo-applet call failed");
    if clippo_ipc::is_service_absent(error) {
        Event::DaemonDown
    } else {
        Event::Failed(format!("{member} failed: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refusal must not put the applet into the "daemon not running" state:
    /// deleting an id that another window already deleted is an ordinary race,
    /// and blanking the list over it would be wrong.
    #[test]
    fn a_refused_call_does_not_lower_the_connection_state() {
        let refused = zbus::Error::FDO(Box::new(fdo::Error::InvalidArgs("no entry 9".to_owned())));
        assert!(matches!(down("Delete", &refused), Event::Failed(_)));

        let absent = zbus::Error::FDO(Box::new(fdo::Error::ServiceUnknown(String::new())));
        assert!(matches!(down("Delete", &absent), Event::DaemonDown));
    }

    /// The `Debug` that has to be hand-written. `Message` derives `Debug` and
    /// contains this, so one `debug!(?message)` added later is the whole
    /// distance between a revealed password and the journal.
    #[test]
    fn debugging_an_event_never_prints_a_revealed_value() {
        let event = Event::Revealed(3, Zeroizing::new("hunter2".to_owned()));
        let rendered = format!("{event:?}");

        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert_eq!(rendered, "Revealed(3, 7 chars)");
    }

    /// The weaker half of the same rule: what the user typed is not clipboard
    /// content, but it is theirs, and `ask` does log a dropped request.
    #[test]
    fn debugging_a_request_never_prints_the_search_query() {
        let rendered = format!("{:?}", Request::Refresh("hunter2".to_owned()));

        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert_eq!(rendered, "Refresh(7 chars)");
        assert_eq!(format!("{:?}", Request::Pin(3, true)), "Pin(3, true)");
    }

    /// The same through the type that actually reaches a log line.
    #[test]
    fn debugging_the_message_that_wraps_it_is_no_different() {
        let message = crate::app::Message::Bus(Event::Revealed(3, "hunter2".to_owned().into()));
        assert!(!format!("{message:?}").contains("hunter2"));
    }

    /// A name of this test module's own, so a suite running on one shared bus
    /// never has two tests fighting over `com.nilfactor.Clippo`.
    const SCRATCH: &str = "com.nilfactor.ClippoAppletOwnerTest";

    /// Whether there is a session bus to talk to. `just test` and CI run the
    /// suite under `dbus-run-session`.
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

    /// F5, at the moment it happens: a name that gains an owner mid-session is
    /// re-checked, and an owner that fails becomes something the picker draws.
    ///
    /// The point of the test is the *re-check*. A check that ran once at
    /// startup would pass every other assertion here and still miss the attack
    /// entirely, because the whole of it is the name changing hands while the
    /// applet is already running.
    ///
    /// It waits on the signal stream rather than sleeping: the bus delivers
    /// `NameOwnerChanged` when it delivers it, and a fixed wait would be either
    /// slow or flaky depending on the machine.
    #[tokio::test]
    async fn an_owner_that_appears_mid_session_is_checked_rather_than_welcomed() {
        if !has_session_bus() {
            return;
        }

        let connection = zbus::Connection::session().await.expect("a connection");
        let mut owners = watch_name(&connection, SCRATCH).await;
        assert!(owners.is_some(), "the watch should have been set up");

        // Nothing owns it yet, so the starting state is "no daemon" — not
        // "untrusted", which is the distinction the third state exists for.
        assert!(matches!(
            daemon_owner(&connection, SCRATCH, &PeerPolicy::from_paths([])).await,
            Event::DaemonDown
        ));

        // Now something takes it. This test process is a real peer with a real
        // pid and a real exe, and it is not a clippo binary.
        let squatter = zbus::Connection::session().await.expect("a connection");
        squatter
            .request_name(SCRATCH)
            .await
            .expect("the scratch name");

        let owner =
            tokio::time::timeout(std::time::Duration::from_secs(5), next_owner(&mut owners))
                .await
                .expect("NameOwnerChanged should arrive within five seconds")
                .expect("the stream should not have ended")
                .expect("the name gained an owner, so the signal carries one");

        let refused =
            judged(clippo_ipc::peer::check(&connection, &owner, &PeerPolicy::from_paths([])).await);
        let Event::DaemonUntrusted(why) = refused else {
            panic!("an unrecognised owner must not be reported as a working daemon: {refused:?}");
        };
        // What the picker prints has to name the process, or there is nothing
        // for the user to go and look at.
        assert!(why.contains(&std::process::id().to_string()), "{why}");

        // The same owner against a list it is on: the refusal above is the
        // allowlist, not the applet refusing everything that ever appears.
        let me = std::env::current_exe().expect("an exe");
        let accepted = judged(
            clippo_ipc::peer::check(&connection, &owner, &PeerPolicy::from_paths([me])).await,
        );
        assert!(matches!(accepted, Event::DaemonUp), "{accepted:?}");
    }

    /// The two absences are different states, and the applet has to keep them
    /// apart: one is a daemon to start, the other is a process to look at.
    /// Collapsing them is exactly the silence that made the takeover invisible.
    #[test]
    fn a_refused_owner_is_not_reported_as_a_missing_daemon() {
        let event = untrusted(clippo_ipc::PeerError::Sandboxed {
            name: ":1.9".to_owned(),
            pid: 4321,
        });
        let Event::DaemonUntrusted(why) = event else {
            panic!("a refusal is its own state");
        };
        assert!(why.contains("4321"), "{why}");
    }

    /// The rest of the events are still legible — a `Debug` that said nothing
    /// would be safe and useless.
    #[test]
    fn the_other_events_still_say_what_they_are() {
        assert_eq!(format!("{:?}", Event::DaemonDown), "DaemonDown");
        assert_eq!(
            format!("{:?}", Event::Entries("hunter".to_owned(), vec![])),
            "Entries(0 rows)"
        );

        let key = EntryKey {
            id: 42,
            created_at: 1_000,
        };
        assert_eq!(
            format!("{:?}", Event::Thumbnail(key, Some(vec![0; 12]))),
            "Thumbnail(EntryKey { id: 42, created_at: 1000 }, 12 bytes)"
        );
        assert_eq!(
            format!("{:?}", Event::Thumbnail(key, None)),
            "Thumbnail(EntryKey { id: 42, created_at: 1000 }, none)"
        );
    }
}
