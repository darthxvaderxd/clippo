//! Where the picker is drawn — the one module that knows what kind of surface
//! it is.
//!
//! DESIGN.md's risk table asks for this by name:
//!
//! > **Applet popup may not be programmatically openable** in libcosmic →
//! > *Design M5 so swapping to a standalone picker window is cheap.*
//!
//! So the rest of the applet never says "popup" or "layer surface". It says
//! [`Picker::show`], [`Picker::close`] and [`Picker::is_open`], and gets a task
//! back. Everything about the surface — the settings, the window id, the
//! anchor — is behind those three methods and the [`Picker::content`] wrapper.
//! That is what kept the swap below to this file and four lines of `app.rs`:
//! nothing outside here calls `get_popup`, builds surface settings, or imports
//! from `iced_runtime`.
//!
//! # Why this is a layer surface and not an `xdg_popup`
//!
//! It was a popup until the gate in ROADMAP Verification §6 was run on a real
//! session, and the answer that check exists to get was **no**: the picker
//! opened, the search field drew a focus ring and a blinking caret, and not one
//! keystroke reached it. Neither typing nor the arrows did anything until the
//! popup was clicked in.
//!
//! The caret was the misleading part. `text_input::focus` sets the *widget's*
//! idea of focus, and the caret blinks off that alone — it says nothing about
//! whether the compositor is sending this surface any keys. It was not:
//!
//! - Keyboard focus on an `xdg_popup` comes from `xdg_popup::grab`, and a grab
//!   needs a serial from a recent input event. A picker opened by `Super+V`
//!   has none: the keypress went to the compositor's shortcut handler, not to
//!   this client. iced looks the serial up with `and_then` and simply skips the
//!   grab when it is missing
//!   (`iced/winit/src/platform_specific/wayland/event_loop/state.rs`), which is
//!   not fatal — but a popup with no grab is a popup with no keyboard.
//! - There is no second route to it. `window::Action::GainFocus` is winit's
//!   `focus_window`, which does nothing on Wayland, and `SctkPopupSettings` has
//!   no keyboard-interactivity field to set.
//!
//! `zwlr_layer_shell_v1` has exactly the thing a popup is missing:
//! `set_keyboard_interactivity`. [`KeyboardInteractivity::Exclusive`] asks for
//! keyboard focus when the surface is mapped, with no serial and no click
//! involved, which is what a picker opened from a global shortcut needs.
//!
//! An applet may do this even though it is a `cosmic-panel` client rather than
//! a direct one: the panel's embedded compositor advertises the layer-shell
//! global to its applets and proxies their layer surfaces out to `cosmic-comp`,
//! interactivity included (`cosmic-panel-bin`'s `xdg_shell_wrapper`, the
//! `WlrLayerShellHandler` impl and `set_keyboard_interactivity` in its
//! `compositor` handler).
//!
//! # What the swap costs
//!
//! A popup with a grab is dismissed by the compositor when the user clicks
//! outside it. A layer surface is not, so that had to be replaced: `Escape` and
//! `Enter` already closed the picker, the panel icon and a second `clippo show`
//! still toggle it, and `app.rs` now also closes on `LayerEvent::Unfocused` for
//! the case where the compositor moves focus away regardless.
//!
//! Position changes too. A popup is anchored under the applet's icon, and a
//! layer surface cannot be: it is placed against the *output*, by anchors and
//! margins that can express an edge and an offset but not a point. So the
//! picker is centred on screen, like the launcher and every other COSMIC
//! surface that is opened from the keyboard rather than pointed at.
//!
//! And the size is no longer the *surface's* to fit. A popup was sized to its
//! contents; a layer surface cannot be, whatever the settings field's doc
//! comment says, and stating a size in full is what stops it being drawn at all
//! under `cosmic-panel`. So the surface is the whole output and
//! [`Picker::content`] draws the picker into the middle of it — which leaves the
//! frame free to hug its contents as before, because that is now an ordinary
//! layout inside a surface far bigger than it, rather than a negotiation with
//! the compositor.
//!
//! [`Picker::show`] has the detail, and it is the part of this file most likely
//! to need revisiting: it is written against a `cosmic-panel` bug, not against
//! the protocol.

use cosmic::iced::alignment::{Horizontal, Vertical};
use cosmic::iced::platform_specific::shell::wayland::commands::layer_surface::{
    destroy_layer_surface, get_layer_surface, Anchor, KeyboardInteractivity, Layer,
};
use cosmic::iced::runtime::platform_specific::wayland::layer_surface::{
    IcedMargin, SctkLayerSurfaceSettings,
};
use cosmic::iced::widget::container;
use cosmic::iced::window::Id;
use cosmic::iced::{Border, Color, Length, Limits, Shadow};
use cosmic::widget;
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

/// What the layer surface is called on the wire.
///
/// Compositors key per-surface rules off this, and `cosmic-panel` passes it
/// through to `cosmic-comp` unchanged.
const NAMESPACE: &str = "clippo-picker";

/// The picker surface, open or closed.
#[derive(Debug, Default)]
pub struct Picker {
    /// The surface's window id while it is open.
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
    /// picker would leak the first one's window id, and the applet would then
    /// have a surface it can never destroy.
    pub fn show<M: 'static>(&mut self) -> cosmic::app::Task<M> {
        if self.is_open() {
            return cosmic::app::Task::none();
        }

        let id = Id::unique();
        self.open = Some(id);

        get_layer_surface(SctkLayerSurfaceSettings {
            id,
            // Above the panel, which is on `Top`. A picker that the panel drew
            // over would be cut off at exactly the edge it comes out of.
            layer: Layer::Overlay,
            // The whole reason this is not a popup. See the module docs.
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            input_zone: None,
            // All four edges, so the surface is the whole output and the picker
            // is centred inside it by [`Picker::content`] rather than by the
            // compositor. A layer surface is placed by its anchors and margins
            // and has no way to ask for "the middle", so the surface being the
            // screen is what makes the middle expressible at all.
            //
            // Spanning both axes is also what keeps the surface drawable — see
            // the `size` field below.
            anchor: Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
            output: Default::default(),
            namespace: NAMESPACE.to_owned(),
            // Nothing to step over. The picker no longer hangs off the panel's
            // edge, so it needs no clearance from it.
            margin: IcedMargin::default(),
            // Explicit, and not the `None` the field's own doc comment offers
            // ("providing None will autosize the layer surface to its
            // contents"). That is true only for an axis anchored to *both* of
            // its edges. On any other axis `SctkState::get_layer_surface`
            // rewrites the request to `Some(1)` before it ever reaches the
            // compositor:
            //
            //     size.1 = Some(size.1.unwrap_or(1).max(1));
            //
            // and the configure handler then feeds that straight back as the
            // new size (`handlers/shell/layer.rs`, `configure.new_size` is
            // `requested_size` whenever it is `Some`). Nothing measures the
            // contents afterwards.
            //
            // Both axes are anchored to both of their edges, so both are left
            // to the compositor and the surface comes back the size of the
            // output. That is not only so the picker can be centred in it — it
            // is what gets the surface drawn at all. `cosmic-panel` forwards our
            // request to `cosmic-comp` and then forwards the reply back to us
            // *only when the two differ*
            // (`xdg_shell_wrapper/client/handlers/layer_shell.rs`: it overwrites
            // `requested_size` with what we asked for and guards
            // `send_configure` behind `requested_size != configure.new_size`).
            // A fully-specified size comes back verbatim, so no configure is
            // ever delivered, so iced never renders and never attaches a
            // buffer — while `cosmic-comp` has already handed the surface the
            // exclusive keyboard focus. That is a picker that eats every
            // keystroke and shows nothing.
            //
            size: Some((None, None)),
            // Nothing is reserved. The picker is transient and sits over the
            // desktop; a non-zero zone would shove every other window aside for
            // as long as it was open.
            exclusive_zone: 0,
            // Unbounded, which is not the same thing as unconsidered. These are
            // the limits the *surface* is laid out within, not a hint about the
            // contents: capping them at the picker's own size caps the surface
            // at the picker's own size, and then there is no room around it for
            // "centred" to mean anything — the surface is 460 wide, the
            // container filling it is 460 wide, and the whole thing is drawn at
            // the corner the anchors put it in. That is what it did.
            //
            // So the surface is left free to be the output, and the picker's
            // size is applied to the picker, in [`Picker::content`].
            size_limits: Limits::NONE,
        })
    }

    /// Close it, doing nothing if it is already closed.
    pub fn close<M: 'static>(&mut self) -> cosmic::app::Task<M> {
        match self.open.take() {
            Some(id) => destroy_layer_surface(id),
            None => cosmic::app::Task::none(),
        }
    }

    /// Note that the compositor closed a surface, and say whether it was ours.
    ///
    /// The compositor can take a layer surface away without the applet asking
    /// — `zwlr_layer_surface_v1::closed`, which arrives when the output it was
    /// on goes — so this is how the picker learns it is no longer on screen.
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
    ///
    /// The surface is the whole output — [`Picker::show`] explains why it has to
    /// be — so this is what makes the picker picker-sized again: the frame is
    /// held to [`WIDTH`] and centred, and everything around it is left empty.
    ///
    /// Centring here rather than through the surface's anchors is not a
    /// preference. Layer surfaces are positioned by anchor and margin, which can
    /// express an edge and an offset from it but not "the middle of whatever
    /// output this lands on"; a full-output surface with the contents centred in
    /// it is how that gets said.
    ///
    /// # Why the frame is built here and not by `Context::popup_container`
    ///
    /// That helper is the obvious thing to reach for and it cannot be used, for
    /// two reasons that both come from it being written for popups.
    ///
    /// It returns an [`Autosize`], whose whole job is to resize the *surface* to
    /// its contents. On a popup that is the point. Here it fights the surface:
    /// whatever this function wraps around it, the `Autosize` inside shrinks the
    /// layer surface back down to the picker, and a surface smaller than its
    /// anchors gets dropped in a corner by the compositor. Centring cannot win
    /// against it, because by the time centring runs there is no space left to
    /// centre in.
    ///
    /// It also clamps the width to exactly 360 with
    /// `.limits(Limits::NONE.min_width(360.0).max_width(360.0))`, so [`WIDTH`]
    /// never applied while it was in use.
    ///
    /// The styling below is `popup_container`'s own, so the frame looks the same
    /// as it did — the background base colour for the theme's transparency, a
    /// `radius_m` corner, and a one-pixel divider border.
    ///
    /// [`Autosize`]: cosmic::widget::autosize::Autosize
    pub fn content<'a, M: 'static>(&self, contents: impl Into<Element<'a, M>>) -> Element<'a, M> {
        // Boxed into an `Element` between the two containers rather than nested
        // directly. Handing one container straight to the next keeps both widget
        // types in the outer one's; at `opt-level=3` with thin LTO that
        // compounds until it overflows rustc's stack in codegen — a SIGSEGV with
        // no diagnostic, while `cargo check` and a debug build are both clean.
        // `Element` is a boxed `dyn Widget`, so this erases the inner type and
        // stops it compounding.
        let picker: Element<'a, M> = widget::container(contents)
            .width(Length::Fixed(WIDTH))
            .max_height(MAX_HEIGHT)
            .class(cosmic::theme::Container::custom(|theme| {
                let cosmic = theme.cosmic();
                let background = cosmic.background(theme.transparent);
                container::Style {
                    text_color: Some(background.on.into()),
                    icon_color: Some(background.on.into()),
                    background: Some(Color::from(background.base).into()),
                    border: Border {
                        radius: cosmic.corner_radii.radius_m.into(),
                        width: 1.0,
                        color: background.divider.into(),
                    },
                    shadow: Shadow::default(),
                    snap: true,
                }
            }))
            .into();

        widget::container(picker)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Horizontal::Center)
            .align_y(Vertical::Center)
            .into()
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
