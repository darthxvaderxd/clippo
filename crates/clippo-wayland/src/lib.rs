//! Hand-rolled `ext_data_control_v1` / `zwlr_data_control_v1` client.
//!
//! M1a implements the *bind* and *watch* halves of the design: connect, bind a
//! data-control manager preferring the `ext` protocol, get a device for the
//! seat, and capture every interesting flavor of each selection atomically.
//! Serving offers back to the compositor arrives with the copy-back path at M3.
//!
//! ```no_run
//! # fn main() -> Result<(), clippo_wayland::Error> {
//! let (watcher, mut selections) = clippo_wayland::watch(Default::default())?;
//! while let Some(selection) = selections.blocking_recv() {
//!     println!("{} flavors", selection.flavors.len());
//! }
//! watcher.stop();
//! # Ok(())
//! # }
//! ```
//!
//! **Run this from a host terminal.** A Flatpak-proxied Wayland socket filters
//! out privileged protocols, so data-control is invisible from inside one; see
//! [`Error::NoDataControlManager`].

mod flavor;
mod mime;
mod protocol;
mod watch;

use std::fmt;
use std::time::Duration;

pub use mime::{
    is_interesting, is_password_manager_hint, INTERESTING_MIMES, PASSWORD_MANAGER_HINT_MIME,
};
pub use protocol::{EXT_PROTOCOL, WLR_PROTOCOL};
pub use watch::{watch, Watcher};

/// Default per-flavor size cap, matching the store's image cap in DESIGN.md.
pub const DEFAULT_MAX_FLAVOR_BYTES: usize = 8 * 1024 * 1024;

/// Default time a single flavor may take before it is abandoned.
pub const DEFAULT_FLAVOR_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Default number of captured selections that may queue up for the daemon.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 64;

/// Which clipboard a selection came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SelectionKind {
    /// The ordinary clipboard: Ctrl+C, Ctrl+V.
    Clipboard,
    /// The middle-click primary selection. Off by default.
    Primary,
}

/// One MIME flavor of a captured selection.
///
/// `Debug` deliberately prints the length rather than the bytes: clipboard
/// contents routinely include passwords, and a stray `{flavor:?}` in a log line
/// would leak them.
#[derive(Clone, PartialEq, Eq)]
pub struct Flavor {
    /// The MIME type the source advertised.
    pub mime: String,
    /// The bytes the source wrote for this flavor.
    pub data: Vec<u8>,
}

impl fmt::Debug for Flavor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Flavor")
            .field("mime", &self.mime)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// Every interesting flavor of a single copy, captured together.
///
/// The watcher emits one of these per selection. It never emits a message per
/// flavor: a half-captured selection is worse than none, because downstream
/// would store an entry that pastes back incompletely.
#[derive(Clone, PartialEq, Eq)]
pub struct Selection {
    /// Which clipboard this came from.
    pub kind: SelectionKind,
    /// The flavors, in the order the source advertised them.
    pub flavors: Vec<Flavor>,
}

impl Selection {
    /// The flavor with this exact MIME type, if it was captured.
    pub fn flavor(&self, mime: &str) -> Option<&Flavor> {
        self.flavors.iter().find(|flavor| flavor.mime == mime)
    }

    /// Whether the source tagged this selection as a password-manager copy.
    pub fn has_password_manager_hint(&self) -> bool {
        self.flavors
            .iter()
            .any(|flavor| mime::is_password_manager_hint(&flavor.mime))
    }
}

impl fmt::Debug for Selection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Selection")
            .field("kind", &self.kind)
            .field("flavors", &self.flavors)
            .finish()
    }
}

/// How the watcher behaves.
#[derive(Debug, Clone)]
pub struct WatchConfig {
    /// Capture the middle-click primary selection as well. **Off by default.**
    ///
    /// With this off on `zwlr_data_control_v1` the manager is bound at version
    /// 1, which has no primary-selection event, so no primary device capability
    /// is created and the compositor never offers one.
    ///
    /// With it on, the two clipboards share one capture slot: a selection that
    /// arrives while another is still being read supersedes it. Reads normally
    /// finish within one turn of the loop, so this only bites when a large
    /// image capture overlaps a mouse-drag selection.
    pub primary: bool,
    /// Largest a single flavor may be. Anything bigger is dropped, not
    /// truncated — a half a PNG is not worth storing.
    pub max_flavor_bytes: usize,
    /// How long a single selection's reads may take before the stragglers are
    /// abandoned. Guards against a source that takes the pipe and never closes
    /// it.
    pub flavor_read_timeout: Duration,
    /// How many captured selections may queue up for the daemon.
    pub channel_capacity: usize,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            primary: false,
            max_flavor_bytes: DEFAULT_MAX_FLAVOR_BYTES,
            flavor_read_timeout: DEFAULT_FLAVOR_READ_TIMEOUT,
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
        }
    }
}

/// What can go wrong bringing the watcher up.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No Wayland compositor to talk to.
    #[error("could not connect to the Wayland compositor")]
    Connect(#[from] wayland_client::ConnectError),

    /// The registry roundtrip failed.
    #[error("could not read the Wayland registry")]
    Registry(#[from] wayland_client::globals::GlobalError),

    /// Neither data-control protocol could be bound.
    #[error("{0}")]
    NoDataControlManager(String),

    /// The compositor advertised no seat to attach a device to.
    #[error("the compositor advertised no wl_seat, so there is no selection to watch")]
    NoSeat,

    /// The calloop event loop could not be set up.
    #[error("could not set up the event loop")]
    EventLoop(#[source] calloop::Error),

    /// The watcher thread could not be spawned.
    #[error("could not spawn the wayland watcher thread")]
    SpawnThread(#[source] std::io::Error),

    /// The watcher thread died before reporting whether it started.
    #[error("the wayland watcher thread stopped before it finished starting up")]
    WatcherStopped,
}

impl Error {
    /// Build the no-manager error, spelling out the cause that is nearly always
    /// the real one.
    ///
    /// On COSMIC both protocols are advertised by default, so their absence
    /// almost never means "the compositor cannot do this" — it means the socket
    /// we are on is a Flatpak proxy, which filters privileged protocols out.
    /// See DESIGN.md, "Environment constraints".
    pub(crate) fn no_data_control_manager() -> Self {
        let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "<unset>".to_owned());
        Self::NoDataControlManager(format!(
            "the compositor advertised neither {EXT_PROTOCOL} nor {WLR_PROTOCOL}, \
             so there is no way to watch the clipboard.\n\
             \n\
             cosmic-comp advertises both by default, so the likely cause is a \
             Flatpak-proxied Wayland socket, which filters out privileged \
             protocols including data-control. WAYLAND_DISPLAY is currently \
             {display}; it must be wayland-0, not wayland-1.\n\
             \n\
             Build and run clippo from a host terminal (cosmic-term), not from \
             RustRover's terminal or run configurations."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_capture_is_off_by_default() {
        assert!(!WatchConfig::default().primary);
    }

    #[test]
    fn the_no_manager_error_names_both_protocols_and_the_flatpak_cause() {
        let message = Error::no_data_control_manager().to_string();
        assert!(message.contains("ext_data_control_v1"), "{message}");
        assert!(message.contains("zwlr_data_control_v1"), "{message}");
        assert!(message.contains("Flatpak"), "{message}");
        assert!(message.contains("WAYLAND_DISPLAY"), "{message}");
        assert!(message.contains("wayland-0"), "{message}");
    }

    #[test]
    fn flavor_debug_does_not_print_clipboard_contents() {
        let flavor = Flavor {
            mime: "text/plain".to_owned(),
            data: b"hunter2".to_vec(),
        };
        let rendered = format!("{flavor:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("text/plain"), "{rendered}");
        assert!(rendered.contains('7'), "{rendered}");
    }

    #[test]
    fn a_selection_exposes_its_flavors_and_the_password_hint() {
        let selection = Selection {
            kind: SelectionKind::Clipboard,
            flavors: vec![
                Flavor {
                    mime: "text/plain".to_owned(),
                    data: b"s3cret".to_vec(),
                },
                Flavor {
                    mime: PASSWORD_MANAGER_HINT_MIME.to_owned(),
                    data: b"secret".to_vec(),
                },
            ],
        };
        assert_eq!(selection.flavor("text/plain").unwrap().data, b"s3cret");
        assert!(selection.flavor("text/html").is_none());
        assert!(selection.has_password_manager_hint());
        assert!(!format!("{selection:?}").contains("s3cret"));
    }
}
