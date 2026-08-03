//! The interface and the proxy, over a real bus.
//!
//! The unit tests in the crate check the types. This checks the thing that can
//! only go wrong at runtime: that the member names, argument types and return
//! types the `#[interface]` side serves are the ones the `#[proxy]` side asks
//! for. Both are generated from the same declarations, so this is really a test
//! that nothing has been added on one side only — which is the failure D-Bus
//! reports as `InvalidArgs` or `UnknownMethod` at the moment a user clicks
//! something, with no compile error anywhere.
//!
//! It needs a session bus. CI and `just test` run the suite under
//! `dbus-run-session`; without one, the test says why it did nothing rather
//! than reporting a pass it did not earn.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use clippo_ipc::peer::{self, PeerPolicy};
use clippo_ipc::{
    ClippoBackend, ClippoInterface, ClippoProxy, EntrySummary, BUS_NAME, OBJECT_PATH,
};
use futures_util::StreamExt;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::{fdo, Connection};

/// A backend that records what it was called with and answers canned data.
#[derive(Default)]
struct Recorder {
    calls: Mutex<Vec<String>>,
    paused: AtomicBool,
}

impl Recorder {
    fn record(&self, call: String) {
        self.calls
            .lock()
            .expect("the recorder is not poisoned")
            .push(call);
    }

    fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .expect("the recorder is not poisoned")
            .clone()
    }
}

fn entry(id: i64, preview: &str) -> EntrySummary {
    EntrySummary {
        id,
        created_at: 1,
        last_used_at: 2,
        kind: "text".to_owned(),
        preview: preview.to_owned(),
        pinned: true,
        sensitive: true,
    }
}

#[async_trait]
impl ClippoBackend for Recorder {
    async fn list(&self, limit: u32, offset: u32) -> fdo::Result<Vec<EntrySummary>> {
        self.record(format!("list({limit}, {offset})"));
        Ok(vec![entry(7, "listed")])
    }

    async fn search(&self, query: &str, limit: u32) -> fdo::Result<Vec<EntrySummary>> {
        self.record(format!("search({query:?}, {limit})"));
        Ok(vec![entry(8, "found")])
    }

    async fn copy(&self, id: i64) -> fdo::Result<()> {
        self.record(format!("copy({id})"));
        Ok(())
    }

    async fn paste(&self, id: i64) -> fdo::Result<bool> {
        self.record(format!("paste({id})"));
        Ok(true)
    }

    async fn delete(&self, id: i64) -> fdo::Result<()> {
        self.record(format!("delete({id})"));
        Err(fdo::Error::InvalidArgs(format!("no entry {id}")))
    }

    async fn pin(&self, id: i64, pinned: bool) -> fdo::Result<()> {
        self.record(format!("pin({id}, {pinned})"));
        Ok(())
    }

    async fn clear(&self, include_pinned: bool) -> fdo::Result<()> {
        self.record(format!("clear({include_pinned})"));
        Ok(())
    }

    async fn reveal(&self, id: i64) -> fdo::Result<String> {
        self.record(format!("reveal({id})"));
        Ok("the whole value".to_owned())
    }

    async fn thumbnail(&self, id: i64) -> fdo::Result<Vec<u8>> {
        self.record(format!("thumbnail({id})"));
        // Not a real PNG: this test is about the wire, and `ay` carrying bytes
        // through unchanged is the only claim it can make about them.
        Ok(vec![0x89, b'P', b'N', b'G', 0x00, 0xff])
    }

    async fn set_paused(&self, paused: bool) -> fdo::Result<()> {
        self.record(format!("set_paused({paused})"));
        self.paused.store(paused, Ordering::Relaxed);
        Ok(())
    }

    async fn paused(&self) -> fdo::Result<bool> {
        self.record("paused()".to_owned());
        Ok(self.paused.load(Ordering::Relaxed))
    }
}

/// Serve the interface, taking the well-known name the way `clippod` does.
///
/// With this test binary on the caller allowlist, because `Paste` checks its
/// caller and a test process is not one of clippo's installed binaries. That is
/// the *subject* of the tests further down; here it would only stop this one
/// from reaching the wire, which is what it is about.
async fn serve(backend: Arc<Recorder>) -> zbus::Result<Connection> {
    let me = std::env::current_exe().expect("this test process has an exe");
    let connection = Connection::session().await?;
    connection
        .object_server()
        .at(
            OBJECT_PATH,
            ClippoInterface::with_policy(backend, PeerPolicy::from_paths([me])),
        )
        .await?;
    let reply = connection
        .request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into())
        .await?;
    assert_eq!(
        reply,
        RequestNameReply::PrimaryOwner,
        "the test bus should be a fresh one with nothing else on it"
    );
    Ok(connection)
}

/// Whether there is a session bus to talk to.
fn has_session_bus() -> bool {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
        return true;
    }
    eprintln!(
        "skipping: no DBUS_SESSION_BUS_ADDRESS. Run the suite under `dbus-run-session -- cargo \
         test`, as `just test` and CI do."
    );
    false
}

#[tokio::test]
async fn every_member_carries_its_arguments_and_its_answer_across_the_bus() {
    if !has_session_bus() {
        return;
    }

    let backend = Arc::new(Recorder::default());
    let connection = serve(Arc::clone(&backend))
        .await
        .expect("the interface should serve on the session bus");
    let clippo = ClippoProxy::new(&connection)
        .await
        .expect("the proxy should find it");

    let listed = clippo.list(10, 5).await.expect("List");
    assert_eq!(listed, vec![entry(7, "listed")]);

    let found = clippo.search("needle", 3).await.expect("Search");
    assert_eq!(found, vec![entry(8, "found")]);

    clippo.copy(1).await.expect("Copy");
    assert!(
        clippo.paste(1).await.expect("Paste"),
        "Paste answers a bool"
    );
    clippo.pin(2, true).await.expect("Pin");
    clippo.clear(false).await.expect("Clear");
    assert_eq!(clippo.reveal(3).await.expect("Reveal"), "the whole value");

    // `ay` is the one member with a non-trivial body type, and a byte array is
    // exactly where D-Bus marshalling is easiest to get subtly wrong — a high
    // bit dropped here would show up as a corrupt thumbnail in the applet and
    // nowhere else.
    assert_eq!(
        clippo.thumbnail(9).await.expect("Thumbnail"),
        vec![0x89, b'P', b'N', b'G', 0x00, 0xff]
    );

    clippo.set_paused(true).await.expect("SetPaused");
    assert!(clippo.paused().await.expect("Paused"));

    // The one member the backend refuses: an error has to survive the trip as
    // an error, not as a dropped call.
    let refused = clippo.delete(4).await.expect_err("Delete should refuse");
    assert!(
        matches!(&refused, zbus::Error::MethodError(name, _, _) if name.contains("InvalidArgs")),
        "{refused:?}"
    );

    assert_eq!(
        backend.calls(),
        [
            "list(10, 5)",
            "search(\"needle\", 3)",
            "copy(1)",
            "paste(1)",
            "pin(2, true)",
            "clear(false)",
            "reveal(3)",
            "thumbnail(9)",
            "set_paused(true)",
            "paused()",
            "delete(4)",
        ]
    );
}

/// Serve the interface at its path on a connection of its own, with a chosen
/// caller policy, and hand back a proxy pointed at that connection's unique
/// name.
///
/// Two connections rather than one, because the thing under test is the
/// *sender* of the message: with one connection the header would name the
/// server itself, which is not a case that happens and not the case that
/// matters.
async fn serve_for_caller(
    backend: Arc<Recorder>,
    callers: PeerPolicy,
) -> zbus::Result<(Connection, ClippoProxy<'static>)> {
    let server = Connection::session().await?;
    server
        .object_server()
        .at(OBJECT_PATH, ClippoInterface::with_policy(backend, callers))
        .await?;

    let client = Connection::session().await?;
    let proxy = ClippoProxy::builder(&client)
        .destination(server.unique_name().expect("a unique name").to_owned())?
        .build()
        .await?;
    Ok((server, proxy))
}

/// F1: `Paste` from a peer that is not one of clippo's binaries is refused
/// before the backend hears about it.
///
/// The caller here is the test process — a real peer with a real pid and a real
/// `/proc/<pid>/exe`, resolved by the daemon over a real bus from the message
/// header. Nothing about the refusal is simulated except the contents of the
/// allowlist.
#[tokio::test]
async fn paste_from_a_peer_that_is_not_a_clippo_binary_is_refused() {
    if !has_session_bus() {
        return;
    }

    let backend = Arc::new(Recorder::default());
    let (_server, clippo) = serve_for_caller(
        Arc::clone(&backend),
        PeerPolicy::from_paths([PathBuf::from("/nonexistent/clippo")]),
    )
    .await
    .expect("the interface should serve");

    let refused = clippo.paste(1).await.expect_err("Paste should be refused");
    assert!(
        matches!(&refused, zbus::Error::MethodError(name, _, _) if name.contains("AccessDenied")),
        "{refused:?}"
    );

    // The message names the exe it saw, which is what makes the journal line
    // beside it actionable.
    let described = format!("{refused:?}");
    let me = std::env::current_exe().expect("an exe");
    assert!(described.contains(&me.display().to_string()), "{described}");

    assert_eq!(
        backend.calls(),
        Vec::<String>::new(),
        "a refused Paste must not reach the backend at all — the point is that nothing was typed"
    );
}

/// The other half, and the one that shows the refusal above is the allowlist
/// rather than `Paste` being broken: the same caller, over the same bus,
/// against a list its exe is on.
#[tokio::test]
async fn paste_from_an_allowlisted_peer_goes_through() {
    if !has_session_bus() {
        return;
    }

    let backend = Arc::new(Recorder::default());
    let me = std::env::current_exe().expect("this test process has an exe");
    let (_server, clippo) = serve_for_caller(Arc::clone(&backend), PeerPolicy::from_paths([me]))
        .await
        .expect("the interface should serve");

    assert!(
        clippo.paste(1).await.expect("Paste"),
        "Paste answers a bool"
    );
    assert_eq!(backend.calls(), ["paste(1)"]);
}

/// `Paste` is the only member gated, deliberately — it is the only one whose
/// effect leaves clippo's own data. Everything else answers a caller the
/// allowlist would refuse, because refusing `List` would break the CLI without
/// making anything safer: a peer that can read the history can read the
/// database too.
#[tokio::test]
async fn the_caller_check_is_on_paste_and_nowhere_else() {
    if !has_session_bus() {
        return;
    }

    let backend = Arc::new(Recorder::default());
    let (_server, clippo) = serve_for_caller(
        Arc::clone(&backend),
        PeerPolicy::from_paths([PathBuf::from("/nonexistent/clippo")]),
    )
    .await
    .expect("the interface should serve");

    clippo.list(1, 0).await.expect("List");
    clippo.search("x", 1).await.expect("Search");
    clippo.copy(1).await.expect("Copy");
    clippo.reveal(1).await.expect("Reveal");
    assert!(clippo.paste(1).await.is_err(), "only Paste is gated");

    assert_eq!(
        backend.calls(),
        ["list(1, 0)", "search(\"x\", 1)", "copy(1)", "reveal(1)"]
    );
}

/// F5, from the frontends' side: the helper both of them use, over a real bus,
/// against a name this test really owns.
#[tokio::test]
async fn a_name_owner_that_is_not_a_clippo_binary_is_reported_as_untrusted() {
    if !has_session_bus() {
        return;
    }

    // A name of this test's own. Taking `com.nilfactor.Clippo` here would make
    // whichever other test wanted it fail, since the whole suite shares one bus.
    const SCRATCH: &str = "com.nilfactor.ClippoIpcOwnerTest";
    let squatter = Connection::session().await.expect("a connection");
    squatter
        .request_name(SCRATCH)
        .await
        .expect("the scratch name");

    let asking = Connection::session().await.expect("a connection");

    let refused = peer::owner_of(&asking, SCRATCH, &PeerPolicy::from_paths([]))
        .await
        .expect("asking the bus should work");
    assert!(matches!(refused, peer::Owner::Untrusted(_)), "{refused:?}");

    // The same owner, on a list it is on.
    let me = std::env::current_exe().expect("an exe");
    let accepted = peer::owner_of(&asking, SCRATCH, &PeerPolicy::from_paths([me]))
        .await
        .expect("asking the bus should work");
    match accepted {
        peer::Owner::Trusted(found) => assert_eq!(found.pid, std::process::id()),
        other => panic!("the owner is this process and it is on the list: {other:?}"),
    }

    // And nothing at all is a third answer, not a refusal.
    let absent = peer::owner_of(
        &asking,
        "com.nilfactor.ClippoIpcNobodyOwnsThis",
        &PeerPolicy::from_paths([]),
    )
    .await
    .expect("asking about an unowned name is not an error");
    assert!(matches!(absent, peer::Owner::Absent), "{absent:?}");
}

#[tokio::test]
async fn history_changed_reaches_a_listening_frontend() {
    if !has_session_bus() {
        return;
    }

    let connection = Connection::session()
        .await
        .expect("a connection for the frontend");
    // Not `serve`: the name is taken by the other test's connection when both
    // run on one bus, so this one talks to the object at its path directly.
    connection
        .object_server()
        .at(
            OBJECT_PATH,
            ClippoInterface::new(Arc::new(Recorder::default())),
        )
        .await
        .expect("serve");

    let proxy = ClippoProxy::builder(&connection)
        .destination(connection.unique_name().expect("a unique name").to_owned())
        .expect("destination")
        .build()
        .await
        .expect("the proxy should build");
    let mut changes = proxy
        .receive_history_changed()
        .await
        .expect("subscribing to HistoryChanged");

    let emitter = clippo_ipc::emitter(&connection).expect("an emitter");
    ClippoInterface::history_changed(&emitter)
        .await
        .expect("emitting HistoryChanged");

    let received = tokio::time::timeout(Duration::from_secs(5), changes.next())
        .await
        .expect("HistoryChanged should arrive within five seconds");
    assert!(
        received.is_some(),
        "the signal stream should not have ended"
    );
}
