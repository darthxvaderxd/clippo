//! The `com.nilfactor.ClippoApplet` surface — one member, `Toggle`.
//!
//! This is the *second* interface in the project and it points the other way.
//! Everything in [`crate::service`] is served by `clippod` and called by the
//! frontends; this one is served by `clippo-applet` and called by `clippo show`.
//! The daemon neither serves nor calls it, and deliberately so: the popup
//! belongs to the process that has a panel surface, and a daemon that could
//! open it would need to know something about the panel it has no business
//! knowing.
//!
//! It lives beside the daemon's interface for the reason given there — a
//! signature that drifts between the caller and the callee is a runtime
//! `InvalidArgs` at the moment the user presses their shortcut, with nothing at
//! compile time to catch it. One declaration, both sides generated from it.
//!
//! # Why a separate bus name
//!
//! [`APPLET_BUS_NAME`] is not [`crate::BUS_NAME`]. Two processes cannot own one
//! name, and the failure modes are genuinely different: no daemon means there
//! is no history to show, while no applet means the panel is not running the
//! applet — so `clippo show` can say which of the two is missing instead of
//! reporting one bus error for both.
//!
//! # `Toggle`, called by a subcommand named `show`
//!
//! Deliberate, and DESIGN.md specifies both spellings. The member is what the
//! surface does — a second press of `Super+V` puts the popup away — while the
//! subcommand is what a user means when they bind a key to it. Renaming either
//! to match the other would make one of them lie.

use std::sync::Arc;

use async_trait::async_trait;
use zbus::fdo;

/// Well-known bus name the applet owns while it is running.
pub const APPLET_BUS_NAME: &str = "com.nilfactor.ClippoApplet";

/// Object path the applet exports [`AppletInterface`] on.
pub const APPLET_OBJECT_PATH: &str = "/com/nilfactor/ClippoApplet";

/// Name of the applet interface, which happens to equal [`APPLET_BUS_NAME`].
pub const APPLET_INTERFACE_NAME: &str = "com.nilfactor.ClippoApplet";

/// What a frontend has to be able to do to serve `com.nilfactor.ClippoApplet`.
///
/// One method, so this trait exists for testability rather than for breadth:
/// it lets the applet's toggle plumbing be driven without a bus, the same way
/// [`ClippoBackend`][crate::ClippoBackend] lets the daemon's members be.
#[async_trait]
pub trait AppletFrontend: Send + Sync + 'static {
    /// Open the picker if it is closed, close it if it is open.
    ///
    /// Returning `Ok` means the request reached the UI, not that a surface is
    /// now on screen — the compositor decides that, and it happens after this
    /// call has already returned. A caller that reported "popup open" on the
    /// strength of this would be guessing.
    async fn toggle(&self) -> fdo::Result<()>;
}

/// The object served at [`APPLET_OBJECT_PATH`].
pub struct AppletInterface {
    frontend: Arc<dyn AppletFrontend>,
}

impl AppletInterface {
    /// Serve `com.nilfactor.ClippoApplet` out of `frontend`.
    pub fn new(frontend: Arc<dyn AppletFrontend>) -> Self {
        Self { frontend }
    }
}

#[zbus::interface(name = "com.nilfactor.ClippoApplet")]
impl AppletInterface {
    /// `Toggle()`.
    async fn toggle(&self) -> fdo::Result<()> {
        self.frontend.toggle().await
    }
}

/// A handle on a running `clippo-applet`.
///
/// Calling this when the panel is not running the applet fails with
/// `ServiceUnknown`, which is worth reporting as "the applet is not running"
/// rather than as a raw bus error — it is the one failure the user can act on,
/// and the fix (add clippo to the panel) is not the same as the fix for an
/// absent daemon.
#[zbus::proxy(
    interface = "com.nilfactor.ClippoApplet",
    default_service = "com.nilfactor.ClippoApplet",
    default_path = "/com/nilfactor/ClippoApplet"
)]
pub trait ClippoApplet {
    /// Open the picker if it is closed, close it if it is open.
    fn toggle(&self) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_applet_object_path_matches_its_bus_name() {
        assert_eq!(
            APPLET_OBJECT_PATH,
            format!("/{}", APPLET_BUS_NAME.replace('.', "/"))
        );
    }

    /// The applet must not try to own the daemon's name: only one process can,
    /// and the loser of that race is whichever started second.
    #[test]
    fn the_applet_and_the_daemon_are_different_names() {
        assert_ne!(APPLET_BUS_NAME, crate::BUS_NAME);
        assert_ne!(APPLET_OBJECT_PATH, crate::OBJECT_PATH);
    }

    /// What the trait is for, exercised rather than asserted: the served side
    /// runs against a frontend that is not the applet and not on a bus.
    struct Counter {
        toggles: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl AppletFrontend for Counter {
        async fn toggle(&self) -> fdo::Result<()> {
            self.toggles
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        }
    }

    #[tokio::test]
    async fn the_served_interface_forwards_a_toggle_to_its_frontend() {
        let counter = Arc::new(Counter {
            toggles: std::sync::atomic::AtomicUsize::new(0),
        });
        let interface = AppletInterface::new(counter.clone());

        interface.toggle().await.unwrap();
        interface.toggle().await.unwrap();

        assert_eq!(
            counter.toggles.load(std::sync::atomic::Ordering::Relaxed),
            2
        );
    }

    /// And a frontend that refuses is reported as a refusal rather than
    /// swallowed — `clippo show` prints what came back.
    #[tokio::test]
    async fn a_frontend_that_refuses_is_passed_through() {
        struct Shutting;

        #[async_trait]
        impl AppletFrontend for Shutting {
            async fn toggle(&self) -> fdo::Result<()> {
                Err(fdo::Error::Failed("the applet is shutting down".to_owned()))
            }
        }

        let interface = AppletInterface::new(Arc::new(Shutting));

        let error = interface.toggle().await.unwrap_err();
        assert!(error.to_string().contains("shutting down"), "{error}");
    }
}
