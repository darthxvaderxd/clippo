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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
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
async fn serve(backend: Arc<Recorder>) -> zbus::Result<Connection> {
    let connection = Connection::session().await?;
    connection
        .object_server()
        .at(OBJECT_PATH, ClippoInterface::new(backend))
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
