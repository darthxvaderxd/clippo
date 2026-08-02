//! Where the picker is drawn — the one module that knows it is a popup.
//!
//! DESIGN.md's risk table asks for this by name:
//!
//! > **Applet popup may not be programmatically openable** in libcosmic →
//! > *Design M5 so swapping to a standalone picker window is cheap.*
//!
//! So the rest of the applet never says "popup". It says [`Picker::show`],
//! [`Picker::close`] and [`Picker::is_open`], and gets a task back. Everything
//! about `xdg_popup` — the settings, the parent surface, the window id, the
//! anchor — is behind those three methods and the [`Picker::content`] wrapper.
//! Swapping to a floating window means rewriting this file and nothing else:
//! `app.rs` has no `Id` in it, no `get_popup`, and no import from
//! `iced_runtime`.
//!
//! # Whether it can be opened programmatically: yes
//!
//! Checked against this pinned revision before the list UI was written, which
//! is the order the ticket asks for. Two things had to be true, and both are:
//!
//! 1. **A popup can be created from any message**, not only from a button
//!    press. `get_popup` is an ordinary `Task` returned from `update`, so the
//!    message that triggers it can just as well have come from the D-Bus
//!    subscription as from a click. That is what [`Picker::show`] does, and
//!    `Toggle` and the panel icon reach it through the one `Clippo::toggle`
//!    identically.
//! 2. **A missing input serial is not fatal.** `xdg_popup::grab` needs a serial
//!    from a recent input event, and a popup opened from `clippo show` has
//!    none — the keypress went to the compositor's shortcut handler, not to
//!    this client. iced skips the grab in that case rather than refusing the
//!    popup or killing the connection
//!    (`iced/winit/src/platform_specific/wayland/event_loop/state.rs`, the
//!    `if grab` block: the serial is looked up with `and_then`, and nothing
//!    happens when it is `None`).
//!
//! So the fallback is not taken, and DESIGN.md's decisions section records
//! that. What is *not* settled by reading the source is the consequence of
//! point 2: without a grab the compositor is under no obligation to give the
//! popup keyboard focus, and a picker that cannot be typed into is no use.
//! There is no compositor in the development environment, so that is a
//! host-terminal check — ROADMAP Verification §6. This module is arranged the
//! way it is so that the answer being "no" costs one file.

use cosmic::app::Core;
use cosmic::iced::window::Id;
use cosmic::iced::Limits;
use cosmic::Element;

/// How wide the picker is, in logical pixels.
///
/// Wider than the 360 an applet popup defaults to, because the rows are
/// clipboard previews rather than settings labels and the default cuts them at
/// about forty characters. Fixed rather than reactive so the list does not
/// change width as the user types and the results get shorter.
const WIDTH: f32 = 460.0;

/// The tallest the picker gets before the list scrolls.
const MAX_HEIGHT: f32 = 560.0;

/// The picker surface, open or closed.
#[derive(Debug, Default)]
pub struct Picker {
    /// The popup's window id while it is open.
    open: Option<Id>,
}

impl Picker {
    /// A closed picker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the picker is on screen.
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// The open picker's window id, for the caller to match against
    /// `view_window`.
    pub fn id(&self) -> Option<Id> {
        self.open
    }

    /// Open it, doing nothing if it is already open.
    ///
    /// Idempotent because `Toggle` is not the only way in: re-opening an open
    /// popup would leak the first one's window id, and the applet would then
    /// have a surface it can never destroy.
    pub fn show<M: 'static>(&mut self, core: &Core) -> cosmic::app::Task<M> {
        if self.is_open() {
            return cosmic::app::Task::none();
        }

        let Some(parent) = core.main_window_id() else {
            // No panel surface to hang a popup off. Nothing useful to do, and
            // nothing broken either — the panel has not finished starting.
            tracing::warn!("clippo-applet has no main window yet; not opening the picker");
            return cosmic::app::Task::none();
        };

        let id = Id::unique();
        self.open = Some(id);

        let mut settings = core.applet.get_popup_settings(parent, id, None, None, None);
        settings.positioner.size_limits = Limits::NONE
            .min_width(WIDTH)
            .max_width(WIDTH)
            .min_height(100.0)
            .max_height(MAX_HEIGHT);

        cosmic::iced::platform_specific::shell::wayland::commands::popup::get_popup(settings)
    }

    /// Close it, doing nothing if it is already closed.
    pub fn close<M: 'static>(&mut self) -> cosmic::app::Task<M> {
        match self.open.take() {
            Some(id) => {
                cosmic::iced::platform_specific::shell::wayland::commands::popup::destroy_popup(id)
            }
            None => cosmic::app::Task::none(),
        }
    }

    /// Note that the compositor closed a surface, and say whether it was ours.
    ///
    /// A popup with a grab goes away on a click outside it without the applet
    /// asking, so this is how the picker learns it is no longer on screen.
    /// Returning whether it matched is what lets the caller drop a revealed
    /// value only when the picker really closed.
    pub fn closed(&mut self, id: Id) -> bool {
        if self.open == Some(id) {
            self.open = None;
            return true;
        }
        false
    }

    /// Wrap the picker's contents in whatever the host surface needs.
    ///
    /// The rounded, themed container an applet popup is expected to be. It is
    /// here rather than in `view` so that a host swap changes the frame without
    /// touching the list.
    pub fn content<'a, M: 'static>(
        &self,
        core: &Core,
        contents: impl Into<Element<'a, M>>,
    ) -> Element<'a, M> {
        core.applet.popup_container(contents).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bookkeeping half, which is all of this that can be tested without a
    /// compositor: the id is remembered on open and forgotten on close, so the
    /// applet never holds a window id for a surface that is gone.
    #[test]
    fn a_new_picker_is_closed() {
        assert!(!Picker::new().is_open());
        assert_eq!(Picker::new().id(), None);
    }

    #[test]
    fn a_surface_the_picker_does_not_own_is_not_its_own_closing() {
        let mut picker = Picker::new();
        assert!(!picker.closed(Id::unique()));

        picker.open = Some(Id::unique());
        assert!(!picker.closed(Id::unique()), "some other window closed");
        assert!(picker.is_open(), "so the picker is still open");
    }

    #[test]
    fn the_compositor_closing_the_picker_is_noticed_once() {
        let mut picker = Picker::new();
        let id = Id::unique();
        picker.open = Some(id);

        assert!(picker.closed(id));
        assert!(!picker.is_open());
        assert!(!picker.closed(id), "already closed; not ours a second time");
    }

    /// Closing a closed picker must not emit a destroy for a window id that no
    /// longer exists.
    #[test]
    fn closing_a_closed_picker_does_nothing() {
        let mut picker = Picker::new();
        let _: cosmic::app::Task<()> = picker.close();
        assert!(!picker.is_open());
    }
}
