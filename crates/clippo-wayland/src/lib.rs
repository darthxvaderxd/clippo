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

/// Why a flavor the source advertised did not make it into the selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropReason {
    /// The source produced more than the configured per-flavor cap.
    OverCap {
        /// The cap, in bytes, that rejected it.
        cap: usize,
    },
    /// Reading the pipe failed.
    Io(String),
    /// The source never closed its end of the pipe in time.
    Stalled,
    /// The read could not be started at all.
    Setup(String),
}

impl fmt::Display for DropReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OverCap { cap } => write!(f, "exceeded the {cap} byte per-flavor cap"),
            Self::Io(e) => write!(f, "read failed: {e}"),
            Self::Stalled => write!(f, "source never closed the pipe"),
            Self::Setup(e) => write!(f, "read could not be started: {e}"),
        }
    }
}

/// A flavor clippo asked for but does not have.
///
/// Carried alongside the surviving flavors rather than dropped on the floor: a
/// missing flavor is a fact about the selection, and silently omitting it makes
/// "why did nothing get stored?" unanswerable from the outside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedFlavor {
    /// The MIME type as the source advertised it.
    pub mime: String,
    /// What went wrong.
    pub reason: DropReason,
}

/// Every interesting flavor of a single copy, captured together.
///
/// The watcher emits one of these per selection. It never emits a message per
/// flavor: a half-captured selection is worse than none, because downstream
/// would store an entry that pastes back incompletely.
///
/// A selection with no surviving [`flavors`](Self::flavors) is still emitted —
/// it carries what was advertised and what was dropped, which is the only
/// record of a copy clippo could not keep. Callers that only want storable
/// content check [`Selection::is_empty`].
#[derive(Clone, PartialEq, Eq)]
pub struct Selection {
    /// Which clipboard this came from.
    pub kind: SelectionKind,
    /// Every MIME type the offer advertised, in the order it advertised them,
    /// including the ones clippo never asked for.
    pub advertised: Vec<String>,
    /// The flavors that were read in full, in the order the source advertised
    /// them.
    pub flavors: Vec<Flavor>,
    /// The flavors clippo asked for and did not get.
    pub dropped: Vec<DroppedFlavor>,
}

impl Selection {
    /// The flavor with this exact MIME type, if it was captured.
    pub fn flavor(&self, mime: &str) -> Option<&Flavor> {
        self.flavors.iter().find(|flavor| flavor.mime == mime)
    }

    /// Whether nothing survived capture, leaving only the diagnostics.
    pub fn is_empty(&self) -> bool {
        self.flavors.is_empty()
    }

    /// Whether the source tagged this selection as a password-manager copy.
    ///
    /// Advertising the marker is the signal, so this answers yes even when the
    /// marker flavor itself was dropped.
    ///
    /// Scanning `flavors` as well is belt and braces: the watcher only ever
    /// fetches what was advertised, so the two lists cannot diverge here — but
    /// a `Selection` built by hand elsewhere could omit `advertised`.
    pub fn has_password_manager_hint(&self) -> bool {
        self.advertised
            .iter()
            .chain(self.flavors.iter().map(|flavor| &flavor.mime))
            .any(|mime| mime::is_password_manager_hint(mime))
    }

    /// Advertised MIME types clippo never asked for, deduplicated, in the order
    /// the source advertised them.
    pub fn skipped(&self) -> Vec<&str> {
        let mut skipped: Vec<&str> = Vec::new();
        for mime in &self.advertised {
            if !mime::is_interesting(mime) && !skipped.contains(&mime.as_str()) {
                skipped.push(mime);
            }
        }
        skipped
    }
}

impl fmt::Debug for Selection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Selection")
            .field("kind", &self.kind)
            .field("advertised", &self.advertised)
            .field("flavors", &self.flavors)
            .field("dropped", &self.dropped)
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
            advertised: vec![
                "text/plain".to_owned(),
                PASSWORD_MANAGER_HINT_MIME.to_owned(),
            ],
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
            dropped: Vec::new(),
        };
        assert_eq!(selection.flavor("text/plain").unwrap().data, b"s3cret");
        assert!(selection.flavor("text/html").is_none());
        assert!(selection.has_password_manager_hint());
        assert!(!selection.is_empty());
        assert!(!format!("{selection:?}").contains("s3cret"));
    }

    /// The marker's *presence* is the signal, so it still counts when the
    /// marker flavor itself never made it back.
    #[test]
    fn the_password_hint_counts_even_when_its_flavor_was_dropped() {
        let selection = Selection {
            kind: SelectionKind::Clipboard,
            advertised: vec![PASSWORD_MANAGER_HINT_MIME.to_owned()],
            flavors: Vec::new(),
            dropped: vec![DroppedFlavor {
                mime: PASSWORD_MANAGER_HINT_MIME.to_owned(),
                reason: DropReason::Stalled,
            }],
        };
        assert!(selection.has_password_manager_hint());
        assert!(selection.is_empty());
    }

    #[test]
    fn skipped_lists_uninteresting_advertised_flavors_once_in_order() {
        let selection = Selection {
            kind: SelectionKind::Clipboard,
            advertised: [
                "TIMESTAMP",
                "text/plain",
                "TARGETS",
                "TIMESTAMP",
                "text/rtf",
            ]
            .map(String::from)
            .to_vec(),
            flavors: Vec::new(),
            dropped: Vec::new(),
        };
        assert_eq!(
            selection.skipped(),
            ["TIMESTAMP", "TARGETS", "text/rtf"],
            "fetchable flavors are not 'skipped', and repeats are listed once"
        );
    }

    #[test]
    fn the_over_cap_reason_names_the_cap_that_rejected_the_flavor() {
        assert_eq!(
            DropReason::OverCap { cap: 1024 }.to_string(),
            "exceeded the 1024 byte per-flavor cap"
        );
    }
}
