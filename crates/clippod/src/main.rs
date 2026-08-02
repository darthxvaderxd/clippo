//! `clippod` — the clipboard daemon.
//!
//! It owns the three things a frontend must not: the encrypted history, the
//! Wayland connection that watches the clipboard, and the `com.nilfactor.Clippo`
//! service that lets a CLI or an applet get at either. DESIGN.md's architecture
//! diagram is this binary in the middle of it.
//!
//! ```text
//!                  ┌──────────────────────────────────────┐
//!    Wayland  ───▶ │ clippod  (systemd --user)            │
//!    data-control  │  ├─ clippo-wayland  watch + offer    │
//!                  │  ├─ clippo-core     detect/mask      │
//!                  │  └─ clippo-store    SQLCipher + blobs│
//!                  └──────────────┬───────────────────────┘
//!                                 │ D-Bus  com.nilfactor.Clippo
//! ```
//!
//! # The shape of a run
//!
//! 1. Read the config, get the database key and open the store.
//! 2. Load every preview into memory — [`cache`], where search happens.
//! 3. Connect to the **session** bus, export the object, *then* take the
//!    well-known name. In that order, so there is no instant in which the name
//!    exists and the object behind it does not.
//! 4. Sweep whatever went stale while the daemon was not running.
//! 5. Start the Wayland watcher on its own thread, hand the daemon the
//!    clipboard handle it serves `Copy` from, and record what it captures.
//! 6. Run until SIGTERM or Ctrl-C.
//!
//! Nothing before step 3 changes an entry: opening the store creates a schema
//! or converts an old file's vacuum mode, and step 2 only reads. That is what
//! makes a second `clippod` harmless — it exits at the name request without
//! having deleted a row from the database the first one is serving out of,
//! which the first one's cache would not notice until the next copy.
//!
//! # Two threads, one runtime
//!
//! The Wayland client is not async and does not go on the tokio runtime. It
//! runs its own `calloop` loop on its own thread, exactly as M1a built it, and
//! sends one message per captured selection down a `tokio::sync::mpsc` channel
//! that the capture task here reads. That boundary is deliberate: `calloop` and
//! tokio are two event loops, and folding one into the other means driving a
//! Wayland connection from a work-stealing executor for no gain.
//!
//! # The daemon owns the clipboard
//!
//! `Copy(id)` does not hand an entry to some system clipboard and walk away —
//! Wayland has no such thing. It makes *this process* the owner of the
//! selection, and every paste is answered by writing the bytes down a pipe from
//! here. **So the clipboard empties when `clippod` exits.** That is the
//! protocol working as designed, not a clippo bug; `Restart=on-failure` in
//! `res/clippod.service` narrows the window and the README says so plainly,
//! because a user who did not know would report it.
//!
//! It also means clippo hears its own copy-back come back round as a capture —
//! see [`echo`], the guard that keeps it out of the history.
//!
//! # Secrets never leave here unmasked
//!
//! Detection runs once, at capture, and a suspected secret's preview is stored
//! already masked — `List` and `Search` hand out `entries.preview` and have
//! nothing else to hand out. The whole value lives in the `flavors` table and
//! leaves through two doors only: `Reveal`, which a user asks for by name, and
//! `Copy`, which puts the real bytes on the clipboard so a masked entry pastes
//! correctly. See [`preview`].
//!
//! # Running it
//!
//! **From a host terminal, not from a Flatpak sandbox.** A proxied Wayland
//! socket filters out data-control, and clippod will exit saying so. See
//! DESIGN.md, "Environment constraints".

mod cache;
mod daemon;
mod echo;
mod logging;
mod preview;

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use clippo_core::{Config, Timestamp};
use clippo_ipc::{ClippoInterface, BUS_NAME, OBJECT_PATH};
use clippo_store::{key, Store};
use clippo_wayland::{WatchConfig, WatchEvent};
use tokio::signal::unix::{signal, SignalKind};
use tracing::{error, info, warn};
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::names::WellKnownName;
use zbus::Connection;

use crate::daemon::{Daemon, Signals};

fn main() -> ExitCode {
    let logging = logging::init();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            error!(error = %error, "clippod could not start a tokio runtime");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run(logging)) {
        Ok(()) => {
            info!("clippod stopped");
            ExitCode::SUCCESS
        }
        Err(error) => {
            // `{:#}` so the whole chain is one line: the context this file
            // added, then the error underneath it.
            error!("clippod stopped: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(logging: &str) -> anyhow::Result<()> {
    info!(
        version = env!("CARGO_PKG_VERSION"),
        logging, "clippod is starting"
    );

    let config = Config::load().context("clippo could not read its configuration")?;
    info!(
        max_entries = config.max_entries,
        max_age_days = config.max_age_days,
        max_image_bytes = config.max_image_bytes,
        capture_primary = config.capture_primary,
        entropy_rule = config.secrets.entropy_rule,
        "clippo's configuration"
    );

    let (database_key, key_source) = key::acquire()
        .await
        .context("clippo could not get the key to its history database")?;
    info!(source = %key_source, "clippo has its database key");

    let store = Store::open_default(&database_key)
        .context("clippo could not open its history database")?
        .with_config(&config);
    // The key has done its job; the connection holds what it needs. Dropping it
    // here rather than at the end of `run` keeps the material out of memory for
    // the entire lifetime of a daemon that runs for weeks.
    drop(database_key);
    info!(path = %store.path().display(), "clippo opened its history database");

    let connection = Connection::session().await.context(
        "clippo could not connect to the session bus. clippod is a session service: it needs \
         DBUS_SESSION_BUS_ADDRESS, which a desktop session sets and a bare login shell may not",
    )?;
    let signals = Signals::bus(
        clippo_ipc::emitter(&connection).context("clippo could not prepare its D-Bus signals")?,
    );
    let daemon = Daemon::new(store, signals, config.secrets.clone())
        .context("clippo could not load its history")?;

    // Export first, take the name second: a caller that resolves the name and
    // immediately calls must find the object already there.
    connection
        .object_server()
        .at(OBJECT_PATH, ClippoInterface::new(Arc::clone(&daemon) as _))
        .await
        .with_context(|| format!("clippo could not export its interface at {OBJECT_PATH}"))?;
    acquire_name(&connection).await?;
    info!(name = BUS_NAME, path = OBJECT_PATH, "clippo is serving");

    // The first write of the run, and deliberately the first thing after the
    // name: a second clippod is refused above, so it exits without having
    // deleted rows from a database the running daemon is serving out of. Its
    // cache would not learn about that until somebody copied something, so it
    // would go on listing entries that no longer exist.
    daemon
        .sweep_retention(Timestamp::now())
        .await
        .context("clippo could not apply its retention limits")?;

    // Only now start capturing. A second clippod must die at the name request,
    // before it has watched a clipboard or changed a row.
    let (watcher, mut events) = clippo_wayland::watch(watch_config(&config))
        .context("clippo could not watch the clipboard")?;
    info!(
        protocol = watcher.protocol(),
        "clippo is watching the clipboard"
    );
    // `Copy` works from here on. Before this the object is exported but there
    // is no compositor connection behind it, and it says so.
    daemon.connect_clipboard(Arc::new(watcher.clipboard()));

    let capture = tokio::spawn({
        let daemon = Arc::clone(&daemon);
        async move {
            while let Some(event) = events.recv().await {
                match event {
                    WatchEvent::Captured(selection) => daemon.capture(selection).await,
                    WatchEvent::SelectionLost => daemon.selection_lost().await,
                }
            }
        }
    });

    let outcome = tokio::select! {
        signal = shutdown_signal() => {
            info!(signal, "clippod is shutting down");
            Ok(())
        }
        // The channel only closes when the watcher thread has stopped, which it
        // does not do on its own. Something is wrong with the Wayland
        // connection, and a daemon that stays up recording nothing is the
        // silent failure this whole design is trying to avoid — so exit and let
        // `Restart=on-failure` try again.
        _ = capture => Err(anyhow::anyhow!(
            "the wayland watcher stopped, so clippo would no longer record anything"
        )),
    };

    // Blocking, and deliberately so: it joins the watcher thread, and the point
    // of joining is that the compositor connection is closed before the process
    // is. Nothing else is running by now.
    watcher.stop();
    outcome
}

/// How the watcher is configured, from the user's config.
fn watch_config(config: &Config) -> WatchConfig {
    let default = WatchConfig::default();
    WatchConfig {
        primary: config.capture_primary,
        // The store refuses an image over `max_image_bytes`, so reading more
        // than that is work thrown away — but the cap here applies to *every*
        // flavor, and `max_image_bytes` is not a statement about how much text
        // a user may copy. Hence the larger of the two: raising the image limit
        // raises what is read, lowering it does not start dropping documents.
        max_flavor_bytes: usize::try_from(config.max_image_bytes)
            .unwrap_or(usize::MAX)
            .max(default.max_flavor_bytes),
        ..default
    }
}

/// Take `com.nilfactor.Clippo`, or explain why this process must not run.
///
/// `DoNotQueue` is the whole point. Without it a second `clippod` sits in the
/// queue, owning nothing, watching the clipboard and writing to the same
/// database as the first — a silent no-op second instance, which is exactly
/// what a user would never think to look for. With it, the second process says
/// what happened and exits non-zero.
async fn acquire_name(connection: &Connection) -> anyhow::Result<()> {
    let name = WellKnownName::try_from(BUS_NAME).expect("clippo's bus name is a valid one");
    let taken = || {
        anyhow::anyhow!(
            "another process already owns {BUS_NAME} on this session bus, so it is already \
             serving the clipboard history — this one would record nothing anybody could \
             read. Check `systemctl --user status clippod`, and stop the other clippod \
             before starting this one"
        )
    };

    match connection
        .request_name_with_flags(name, RequestNameFlags::DoNotQueue.into())
        .await
    {
        Ok(RequestNameReply::PrimaryOwner | RequestNameReply::AlreadyOwner) => Ok(()),
        // `DoNotQueue` makes `InQueue` unreachable, and `Exists` is the taken
        // case; both mean this process is not the owner.
        Ok(_) => Err(taken()),
        Err(zbus::Error::NameTaken) => Err(taken()),
        Err(error) => Err(error).context(format!("clippo could not request the name {BUS_NAME}")),
    }
}

/// Resolve when the session asks clippod to stop, naming the signal that did it.
///
/// `SIGTERM` is what `systemctl --user stop clippod` sends; `SIGINT` is Ctrl-C
/// in the terminal DESIGN.md says to run this from.
async fn shutdown_signal() -> &'static str {
    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(terminate) => Some(terminate),
        Err(error) => {
            warn!(
                error = %error,
                "clippo could not listen for SIGTERM, so `systemctl --user stop` will have to \
                 kill it rather than let it shut down cleanly"
            );
            None
        }
    };

    let sigterm = async {
        match terminate.as_mut() {
            Some(terminate) => {
                terminate.recv().await;
            }
            // Nothing to wait for; leave the other arm to win.
            None => std::future::pending().await,
        }
    };

    tokio::select! {
        result = tokio::signal::ctrl_c() => {
            if let Err(error) = result {
                warn!(error = %error, "clippo could not listen for Ctrl-C");
            }
            "SIGINT"
        }
        () = sigterm => "SIGTERM",
    }
}
