//! `clippo-applet` — the libcosmic panel applet: search, pins, images and live
//! updates.
//!
//! A panel icon that opens a keyboard-driven picker over the history `clippod`
//! holds. It owns no clipboard state of its own: every row it draws came from
//! `List`/`Search`, and every action it takes is a call on the same
//! `com.nilfactor.Clippo` members `clippo` uses from a terminal.
//!
//! ```text
//!   clippod  ──  com.nilfactor.Clippo  ──▶  clippo-applet
//!      ▲                                          │
//!      └──────  HistoryChanged (signal)  ─────────┘
//!
//!   clippo show  ──  com.nilfactor.ClippoApplet  ──▶  clippo-applet
//! ```
//!
//! The modules, in the order they are worth reading:
//!
//! - [`model`] — the selection and the revealed value, with no libcosmic in it.
//! - [`bus`] — the one background task that talks to D-Bus, both directions.
//! - [`surface`] — the popup, and the only file that knows it is one.
//! - [`view`] — rows, masks, badges, thumbnails.
//! - [`app`] — the `cosmic::Application` that wires those together.
//!
//! # Running it by hand
//!
//! The applet is normally started by `cosmic-panel`. To see its output, run it
//! from a host terminal — not from a Flatpak, for the reason DESIGN.md gives
//! about the proxied Wayland socket:
//!
//! ```sh
//! RUST_LOG=debug cargo run -p clippo-applet
//! ```

mod app;
mod bus;
mod model;
mod surface;
mod view;

use tracing_subscriber::EnvFilter;

fn main() -> cosmic::iced::Result {
    // stderr rather than journald: an applet is a child of `cosmic-panel`, so
    // its stderr already lands in the journal under that unit, and a second
    // journald connection would only split its output across two places.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    cosmic::applet::run::<app::Clippo>(())
}
