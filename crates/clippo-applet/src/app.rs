//! The `cosmic::Application` itself: state, messages, and what each key does.
//!
//! Deliberately thin. The three things that are hard to get right live
//! elsewhere and are tested there — [`crate::model`] owns the selection and the
//! revealed value, [`crate::bus`] owns D-Bus, [`crate::surface`] owns the
//! popup. What is left here is the wiring, which is the part a compositor is
//! needed to exercise anyway.
//!
//! # Every action is a D-Bus call the CLI also makes
//!
//! M5 requires no second code path, and there is none: `Enter` sends
//! [`Request::Copy`], `Delete` sends [`Request::Delete`], `Ctrl+P` sends
//! [`Request::Pin`]. The applet has no store, no database handle and no
//! `clippo-store` dependency — it cannot reach the history except through the
//! same members `clippo copy`, `clippo rm` and `clippo pin` use.
//!
//! # Why the keys are read globally
//!
//! The search field has focus the whole time the picker is open — that is what
//! makes "type to filter" work without a mouse. So the navigation keys cannot
//! be attached to the list widget, which never has focus; they are read from
//! the runtime's event stream instead and dispatched here.
//!
//! One consequence is worth writing down: `Delete` is a list action *and* a
//! text-editing key. While the cursor sits at the end of the query — where it
//! is during ordinary typing — forward-delete does nothing to the text and the
//! binding is unambiguous. With the cursor moved into the middle of a query it
//! will do both. DESIGN.md specifies `Delete` for removal, so it keeps that
//! binding, and this is the cost.

use std::collections::{HashMap, HashSet};

use cosmic::app::{Core, Task};
use cosmic::iced::keyboard::{key::Named, Key, Modifiers};
use cosmic::iced::window::Id;
use cosmic::iced::{Event, Subscription};
use cosmic::widget::image;
use cosmic::Element;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::bus::{self, Request};
use crate::model::{Model, Status};
use crate::surface::Picker;
use crate::view;

/// The panel icon.
///
/// `edit-paste-symbolic` rather than a clippo-specific icon because M5 ships no
/// icon theme — M6 does. A name every icon theme has is better than a missing
/// glyph in the panel.
const PANEL_ICON: &str = "edit-paste-symbolic";

/// What the UI reacts to.
#[derive(Debug, Clone)]
pub enum Message {
    /// The panel icon was pressed.
    IconPressed,
    /// A surface came up, possibly ours.
    Opened(Id),
    /// A surface went away, possibly ours.
    Closed(Id),
    /// The search field changed.
    QueryChanged(String),
    /// A row was clicked.
    Chose(i64),
    /// A key that means something to the picker.
    Key(Action),
    /// Something happened on D-Bus.
    Bus(bus::Event),
}

/// The keyboard actions, named for what they do rather than for their keys.
///
/// Separated from the key that produces them so that [`Application::update`]
/// reads as a list of behaviours, and so rebinding is a change in one `match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `↑`
    Previous,
    /// `↓`
    Next,
    /// `Enter` — copy the selected entry and close.
    Activate,
    /// `Delete` — remove the selected entry.
    Remove,
    /// `Ctrl+P` — pin or unpin the selected entry.
    TogglePin,
    /// `Ctrl+R` — reveal the selected entry's real value.
    Reveal,
    /// `Escape` — close without doing anything.
    Dismiss,
}

/// The applet.
pub struct Clippo {
    core: Core,
    model: Model,
    picker: Picker,
    /// How to reach the daemon. `None` until the bus worker has started, which
    /// is the only window in which the UI can do nothing.
    requests: Option<mpsc::Sender<Request>>,
    /// Decoded thumbnails, by entry id.
    ///
    /// Not secret — a thumbnail is a downscale of a picture the user copied —
    /// so unlike a revealed value this is allowed to persist across popup
    /// opens, which is what stops reopening the picker re-fetching every image.
    thumbnails: HashMap<i64, image::Handle>,
    /// Entries a thumbnail has already been asked for, successfully or not.
    ///
    /// Without this an image stored without a thumbnail would be re-requested
    /// on every refresh, which for a history full of oversized screenshots is a
    /// call per row per keystroke.
    asked: HashSet<i64>,
}

impl Clippo {
    /// Hand a request to the bus worker.
    ///
    /// `try_send` because this runs in `update`, which is synchronous. A full
    /// queue means the daemon is not keeping up with the UI; dropping the
    /// request is right, since the next keystroke supersedes a refresh anyway
    /// and the alternative is blocking the frame.
    fn ask(&self, request: Request) {
        let Some(requests) = self.requests.as_ref() else {
            debug!(?request, "clippo-applet is not on the bus yet; dropping");
            return;
        };
        if let Err(error) = requests.try_send(request) {
            warn!(%error, "clippo-applet could not queue a request");
        }
    }

    /// Re-read the list for the current query.
    fn refresh(&self) {
        self.ask(Request::Refresh(self.model.query().to_owned()));
    }

    /// Copy the selected entry and put the picker away.
    ///
    /// Closing is not optional: the user asked for this value in order to paste
    /// it somewhere, and a picker still on screen is over the window they are
    /// about to paste into.
    fn activate(&mut self) -> Task<Message> {
        let Some(id) = self.model.selected_id() else {
            return Task::none();
        };
        self.ask(Request::Copy(id));
        self.dismiss()
    }

    /// Close the picker, forgetting anything revealed.
    fn dismiss(&mut self) -> Task<Message> {
        self.model.forget_revealed();
        self.picker.close()
    }

    /// Open the picker with an empty query.
    ///
    /// The query is cleared rather than kept: the picker is opened to find
    /// something, and a stale filter from ten minutes ago hides the entry the
    /// user almost certainly wants — the one they just copied.
    ///
    /// Focusing the search field is *not* done here, deliberately. A focus is a
    /// widget operation, and the runtime runs those against the interfaces of
    /// windows that already exist (`iced_winit`'s `Action::Widget` arm loops
    /// over the live ones). Batching it with the popup's creation would run it
    /// one frame too early, against a surface that does not exist yet, and it
    /// would be silently dropped — leaving a picker that has to be clicked
    /// before it can be typed into, which is the whole keyboard-first premise
    /// gone. So it waits for [`Message::Opened`].
    fn present(&mut self) -> Task<Message> {
        self.model.set_query(String::new());
        self.refresh();
        self.picker.show(&self.core)
    }

    /// Open or close.
    fn toggle(&mut self) -> Task<Message> {
        if self.picker.is_open() {
            self.dismiss()
        } else {
            self.present()
        }
    }

    /// Ask for any thumbnail an image row needs and does not have.
    fn fetch_thumbnails(&mut self) {
        let wanted: Vec<i64> = self
            .model
            .entries()
            .iter()
            .filter(|entry| entry.kind == "image" && !self.asked.contains(&entry.id))
            .map(|entry| entry.id)
            .collect();

        for id in wanted {
            self.asked.insert(id);
            self.ask(Request::Thumbnail(id));
        }
    }

    /// Fold in one thing that happened on the bus.
    fn on_bus(&mut self, event: bus::Event) -> Task<Message> {
        match event {
            bus::Event::Ready(sender) => {
                self.requests = Some(sender);
                self.refresh();
            }
            bus::Event::Entries(entries) => {
                self.model.set_entries(entries);
                self.fetch_thumbnails();
            }
            bus::Event::Revealed(id, value) => {
                // Straight into the model, which will only draw it while its
                // row stays selected. Nothing else holds a copy.
                self.model.set_revealed(id, value);
            }
            bus::Event::Thumbnail(id, bytes) => {
                self.thumbnails.insert(id, image::Handle::from_bytes(bytes));
            }
            bus::Event::Toggle => return self.toggle(),
            bus::Event::DaemonUp => {
                // Unconditional refresh: this arrives both from
                // `HistoryChanged` and from the daemon reappearing, and in
                // either case the list on screen is the stale one.
                self.model.set_status(Status::Connected);
                self.refresh();
            }
            bus::Event::DaemonDown => {
                self.model.set_status(Status::DaemonUnavailable);
                // A restarted daemon has a new database handle and may have
                // swept on startup, so nothing cached about the old one is
                // worth keeping.
                self.thumbnails.clear();
                self.asked.clear();
            }
            bus::Event::Failed(message) => {
                warn!(message, "clippo-applet: a call failed");
            }
        }
        Task::none()
    }

    /// Do what a key means.
    fn on_key(&mut self, action: Action) -> Task<Message> {
        // The panel window has keyboard focus at other times, and acting on
        // Delete because the user pressed it in some unrelated application
        // would be a destructive surprise.
        if !self.picker.is_open() {
            return Task::none();
        }

        let selected = self.model.selected_id();
        match action {
            Action::Previous => self.model.select_previous(),
            Action::Next => self.model.select_next(),
            Action::Activate => return self.activate(),
            Action::Dismiss => return self.dismiss(),
            Action::Remove => {
                if let Some(id) = selected {
                    // No confirmation, and none is wanted: this is one entry
                    // out of a rolling history, and `clippo clear` is the
                    // destructive one.
                    self.ask(Request::Delete(id));
                }
            }
            Action::TogglePin => {
                if let Some(entry) = self.model.selected_entry() {
                    self.ask(Request::Pin(entry.id, !entry.pinned));
                }
            }
            Action::Reveal => {
                if let Some(id) = selected {
                    self.ask(Request::Reveal(id));
                }
            }
        }
        Task::none()
    }
}

/// Translate a raw key press into an [`Action`].
///
/// A free function taking the event rather than a closure because
/// `listen_with` wants a plain `fn`, and separate from [`Clippo`] because it is
/// the one piece of key handling that can be unit tested.
fn action_for(key: &Key, modifiers: Modifiers) -> Option<Action> {
    // `macos_command` is not a thing here, but `control` alone must not match
    // when other modifiers are held: `Ctrl+Shift+P` is not `Ctrl+P`.
    let only_control = modifiers.control() && !modifiers.alt() && !modifiers.shift();

    match key {
        Key::Named(Named::ArrowUp) if !modifiers.control() => Some(Action::Previous),
        Key::Named(Named::ArrowDown) if !modifiers.control() => Some(Action::Next),
        Key::Named(Named::Enter) if !modifiers.control() => Some(Action::Activate),
        Key::Named(Named::Delete) if !modifiers.control() => Some(Action::Remove),
        Key::Named(Named::Escape) => Some(Action::Dismiss),
        Key::Character(c) if only_control && c.eq_ignore_ascii_case("p") => Some(Action::TogglePin),
        Key::Character(c) if only_control && c.eq_ignore_ascii_case("r") => Some(Action::Reveal),
        _ => None,
    }
}

impl cosmic::Application for Clippo {
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = "com.nilfactor.ClippoApplet";

    fn core(&self) -> &Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: ()) -> (Self, Task<Message>) {
        let applet = Clippo {
            core,
            model: Model::new(),
            picker: Picker::new(),
            requests: None,
            thumbnails: HashMap::new(),
            asked: HashSet::new(),
        };
        (applet, Task::none())
    }

    /// The compositor telling us a surface is gone — a click outside the popup,
    /// most often. Not the same as the applet closing it, and the only way it
    /// learns about that one.
    fn on_close_requested(&self, id: Id) -> Option<Message> {
        Some(Message::Closed(id))
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::IconPressed => self.toggle(),

            Message::Opened(id) if self.picker.id() == Some(id) => {
                // The picker's surface is on screen, so a focus operation now
                // has an interface to reach. This is the auto-focus: the field
                // is ready to type into without a click, which is what makes
                // "type to filter" the first thing that happens.
                cosmic::widget::text_input::focus(view::search_id())
            }
            Message::Opened(_) => Task::none(),

            Message::Closed(id) => {
                if self.picker.closed(id) {
                    // The popup is gone, so the revealed value goes with it —
                    // M5's "not after the popup closes", for the path where the
                    // compositor closed it rather than the applet.
                    self.model.forget_revealed();
                }
                Task::none()
            }

            Message::QueryChanged(query) => {
                self.model.set_query(query);
                // Straight to the daemon's `Search` on every keystroke rather
                // than filtering the rows already here: DESIGN.md has the
                // applet and the CLI rank identically, and they only do that if
                // one of them is not ranking.
                self.refresh();
                Task::none()
            }

            Message::Chose(id) => {
                self.model.select(id);
                self.activate()
            }

            Message::Key(action) => self.on_key(action),

            Message::Bus(event) => self.on_bus(event),
        }
    }

    fn view(&self) -> Element<'_, Message> {
        self.core
            .applet
            .icon_button(PANEL_ICON)
            .on_press(Message::IconPressed)
            .into()
    }

    /// The popup's contents.
    fn view_window(&self, id: Id) -> Element<'_, Message> {
        if self.picker.id() != Some(id) {
            return cosmic::widget::text("").into();
        }
        self.picker
            .content(&self.core, view::picker(&self.model, &self.thumbnails))
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            Subscription::run(bus::worker).map(Message::Bus),
            cosmic::iced::event::listen_with(|event, _status, window| {
                // `_status` is ignored on purpose: the search field has focus
                // whenever the picker is open and reports some of these as
                // captured, so honouring it would silently disable the arrows.
                match event {
                    Event::Keyboard(cosmic::iced::keyboard::Event::KeyPressed {
                        key,
                        modifiers,
                        ..
                    }) => action_for(&key, modifiers).map(Message::Key),
                    // The only route to knowing the popup's surface exists. The
                    // `Application` trait has `on_close_requested` for the other
                    // half of this and no counterpart for the opening, so it is
                    // read off the runtime's event stream instead.
                    Event::Window(cosmic::iced::window::Event::Opened { .. }) => {
                        Some(Message::Opened(window))
                    }
                    _ => None,
                }
            }),
        ])
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(named: Named) -> Key {
        Key::Named(named)
    }

    fn character(c: &str) -> Key {
        Key::Character(c.into())
    }

    #[test]
    fn the_arrows_move_the_selection() {
        assert_eq!(
            action_for(&key(Named::ArrowUp), Modifiers::default()),
            Some(Action::Previous)
        );
        assert_eq!(
            action_for(&key(Named::ArrowDown), Modifiers::default()),
            Some(Action::Next)
        );
    }

    #[test]
    fn every_binding_design_specifies_is_bound() {
        assert_eq!(
            action_for(&key(Named::Enter), Modifiers::default()),
            Some(Action::Activate)
        );
        assert_eq!(
            action_for(&key(Named::Delete), Modifiers::default()),
            Some(Action::Remove)
        );
        assert_eq!(
            action_for(&character("p"), Modifiers::CTRL),
            Some(Action::TogglePin)
        );
        assert_eq!(
            action_for(&character("r"), Modifiers::CTRL),
            Some(Action::Reveal)
        );
    }

    /// Caps lock, or a shifted binding pressed by habit, still reaches the
    /// action a user meant — but only when Shift is what changed the case.
    #[test]
    fn the_control_bindings_are_case_insensitive() {
        assert_eq!(
            action_for(&character("P"), Modifiers::CTRL),
            Some(Action::TogglePin)
        );
        assert_eq!(
            action_for(&character("R"), Modifiers::CTRL),
            Some(Action::Reveal)
        );
    }

    /// Plain `p` is a character the user is typing into the search field, and
    /// must not pin anything.
    #[test]
    fn an_unmodified_letter_is_typing_rather_than_a_binding() {
        assert_eq!(action_for(&character("p"), Modifiers::default()), None);
        assert_eq!(action_for(&character("r"), Modifiers::default()), None);
    }

    #[test]
    fn a_control_arrow_is_not_a_plain_arrow() {
        assert_eq!(action_for(&key(Named::ArrowUp), Modifiers::CTRL), None);
        assert_eq!(action_for(&key(Named::Enter), Modifiers::CTRL), None);
        assert_eq!(action_for(&key(Named::Delete), Modifiers::CTRL), None);
    }

    /// `Ctrl+Shift+P` is a different chord and belongs to whatever binds it,
    /// not to clippo.
    #[test]
    fn a_third_modifier_does_not_match_a_control_binding() {
        let both = Modifiers::CTRL | Modifiers::SHIFT;
        assert_eq!(action_for(&character("p"), both), None);
        assert_eq!(action_for(&character("r"), both), None);
    }

    #[test]
    fn escape_closes_regardless_of_modifiers() {
        assert_eq!(
            action_for(&key(Named::Escape), Modifiers::default()),
            Some(Action::Dismiss)
        );
        assert_eq!(
            action_for(&key(Named::Escape), Modifiers::CTRL),
            Some(Action::Dismiss)
        );
    }

    #[test]
    fn a_key_that_means_nothing_here_is_left_alone() {
        assert_eq!(action_for(&key(Named::Tab), Modifiers::default()), None);
        assert_eq!(action_for(&key(Named::F5), Modifiers::default()), None);
        assert_eq!(action_for(&character("a"), Modifiers::default()), None);
    }
}
