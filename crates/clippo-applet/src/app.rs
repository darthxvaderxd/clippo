//! The `cosmic::Application` itself: state, messages, and what each key does.
//!
//! Deliberately thin. The three things that are hard to get right live
//! elsewhere and are tested there — [`crate::model`] owns the selection and the
//! revealed value, [`crate::bus`] owns D-Bus, [`crate::surface`] owns the
//! picker's surface. What is left here is the wiring, which is the part a
//! compositor is needed to exercise anyway.
//!
//! # Every action is a D-Bus call the CLI also makes
//!
//! M5 requires no second code path, and there is none: `Enter` sends
//! [`Request::Paste`], `Delete` sends [`Request::Delete`], `Ctrl+P` sends
//! [`Request::Pin`]. The applet has no store, no database handle and no
//! `clippo-store` dependency — it cannot reach the history except through the
//! same members `clippo paste`, `clippo rm` and `clippo pin` use.
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

use cosmic::app::{Core, Task};
use cosmic::iced::event::{wayland, PlatformSpecific};
use cosmic::iced::keyboard::{key::Named, Key, Modifiers};
use cosmic::iced::widget::scrollable::{snap_to, RelativeOffset};
use cosmic::iced::window::Id;
use cosmic::iced::{Event, Subscription};
use cosmic::{widget, Element};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::bus::{self, Request};
use crate::model::{Model, Status};
use crate::surface::Picker;
use crate::thumbs::Thumbnails;
use crate::view;

/// The panel icon.
///
/// M6 ships this one, at `res/icons/hicolor/symbolic/apps/`, and `just install`
/// puts it in the icon theme.
const PANEL_ICON: &str = "com.nilfactor.Clippo-symbolic";

/// What to draw when clippo's own icon is not in the theme.
///
/// Which is the ordinary case for a build run straight out of the repo: the
/// applet is perfectly usable without `just install`, and a missing glyph in
/// the panel would be the first thing anyone doing that saw. `edit-paste` is a
/// freedesktop standard name, so every icon theme has one.
const PANEL_ICON_FALLBACK: &str = "edit-paste-symbolic";

/// What the UI reacts to.
#[derive(Debug, Clone)]
pub enum Message {
    /// The panel icon was pressed.
    IconPressed,
    /// A surface came up, possibly ours.
    Opened(Id),
    /// A surface went away, possibly ours.
    Closed(Id),
    /// A surface lost keyboard focus, possibly ours.
    Unfocused(Id),
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
/// Separated from the key that produces them so that [`cosmic::Application::update`]
/// reads as a list of behaviours, and so rebinding is a change in one `match`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `↑`
    Previous,
    /// `↓`
    Next,
    /// `Enter` — paste the selected entry where the cursor is, and close.
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
    /// Decoded thumbnails and what has been asked for.
    ///
    /// Not secret — a thumbnail is a downscale of a picture the user copied —
    /// so unlike a revealed value this is allowed to persist across popup
    /// opens, which is what stops reopening the picker re-fetching every image.
    /// [`Thumbnails`] owns the rules for that; this file only decides *when* to
    /// ask.
    thumbnails: Thumbnails,
}

impl Clippo {
    /// Hand a request to the bus worker, saying whether it was taken.
    ///
    /// `try_send` because this runs in `update`, which is synchronous. A full
    /// queue means the daemon is not keeping up with the UI; dropping the
    /// request is right, since the next keystroke supersedes a refresh anyway
    /// and the alternative is blocking the frame.
    ///
    /// The return value matters for anything that also records *that* it asked.
    /// Marking after the send is correct whether or not a slot happens to free
    /// up in the meantime — `cosmic::SingleThreadExecutor` is a one-worker tokio
    /// runtime on its own thread, so the bus worker does run concurrently with
    /// this — because the failure it prevents is one-sided: a caller that marks
    /// first and sends second turns one full queue into a permanent gap, while
    /// marking second can at worst ask twice.
    fn ask(&self, request: Request) -> bool {
        let Some(requests) = self.requests.as_ref() else {
            debug!(?request, "clippo-applet is not on the bus yet; dropping");
            return false;
        };
        match requests.try_send(request) {
            Ok(()) => true,
            Err(error) => {
                warn!(%error, "clippo-applet could not queue a request");
                false
            }
        }
    }

    /// Re-read the list for the current query.
    fn refresh(&self) {
        self.ask(Request::Refresh(self.model.query().to_owned()));
    }

    /// Re-read the list, but only when there is something on screen to update.
    ///
    /// The popup is closed almost all of the time, and `HistoryChanged` fires
    /// on every copy anyone makes. Refreshing regardless would spend a ranked
    /// `Search`, up to [`bus::ROW_LIMIT`] marshalled preview strings and a
    /// `Thumbnail` round trip per copied screenshot on a picker nobody is
    /// looking at. Nothing is lost by waiting: [`Clippo::present`] refreshes as
    /// it opens, so the first thing on screen is always current.
    fn refresh_if_visible(&self) {
        if self.picker.is_open() {
            self.refresh();
        }
    }

    /// Paste the selected entry into whatever was focused, and put the picker
    /// away.
    ///
    /// Closing is not optional and, since `Paste`, is not only tidiness: the
    /// daemon presses the user's paste shortcut into whatever holds keyboard
    /// focus, and while the picker is up that is the picker. It has to be gone
    /// before the keystroke lands, which is why the daemon waits before
    /// pressing — see `clippod`'s `FOCUS_SETTLE`. Nothing here can do better
    /// than that: the applet knows when it asked the compositor to destroy the
    /// surface, not when the compositor moved focus back.
    ///
    /// The request goes out *before* the close rather than after, and the order
    /// is deliberate. There is no "closed" to wait for that would help — the
    /// daemon's wait is what covers the gap — and sending afterwards would mean
    /// keeping something alive to send from after the surface that owns the
    /// selection is gone.
    ///
    /// It closes before the `Paste` can have failed, which is the cost of that:
    /// a refusal reaches the journal and not the user, who finds out by pasting
    /// the wrong thing. Holding the picker open until the answer came back
    /// would put a frame's delay on every copy to catch a case that only arises
    /// when the daemon has just died, and the model has nowhere to show the
    /// message anyway — see [`bus::Event::Failed`].
    fn activate(&mut self) -> Task<Message> {
        let Some(id) = self.model.selected_id() else {
            return Task::none();
        };
        self.ask(Request::Paste(id));
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
        // And the highlight goes back to the top, for the same reason the query
        // does not survive: a picker reopened ten minutes later should be
        // pointing at the entry the user just copied, not at row nine of the
        // list they were looking at then.
        self.model.restart();
        let opening = self.picker.show();
        // After `show` rather than before it, so that a picker which could not
        // open — no panel surface yet — does not spend a `Search` and a burst
        // of `Thumbnail` calls on a list nobody is going to see.
        self.refresh_if_visible();
        opening
    }

    /// Open or close.
    fn toggle(&mut self) -> Task<Message> {
        if self.picker.is_open() {
            self.dismiss()
        } else {
            self.present()
        }
    }

    /// Scroll the list so that the highlighted row is on screen.
    ///
    /// The list scrolls itself for the mouse and for nothing else, so without
    /// this the highlight walks off the bottom of the popup on the sixth or
    /// seventh `↓` and the user is arrowing blind. The selection is still
    /// moving and `Enter` still copies whatever it is on; the only way to see
    /// which row that is is to reach for the mouse and scroll, which is the
    /// keyboard-first premise gone at the point it matters most.
    ///
    /// A *relative* snap rather than a pixel offset because neither figure a
    /// pixel offset needs is known here: how tall a row is, and how much of the
    /// list the popup is showing. `snap_to` stores the fraction and resolves it
    /// against the real bounds when the list is next laid out, so this is right
    /// whatever height the panel gives the popup — and it does not have to run
    /// after a frame the way an absolute offset computed from stale bounds
    /// would.
    fn keep_selection_visible(&self) -> Task<Message> {
        let Some(index) = self.model.selected_index() else {
            return Task::none();
        };
        snap_to(
            view::list_id(),
            RelativeOffset {
                // Left alone: the list only scrolls vertically, and naming a
                // horizontal offset would fight anything that ever changes it.
                x: None,
                y: Some(selection_offset(index, self.model.entries().len())),
            },
        )
    }

    /// Ask for any thumbnail an image row needs and does not have.
    ///
    /// The entry is marked as asked only once [`Clippo::ask`] says the worker
    /// took the request, which is [`Thumbnails`]'s rule and the reason it is a
    /// separate call. The loop then stops at the first refusal, which costs at
    /// most one round trip: the bus worker is on its own thread and may free a
    /// slot mid-loop, so carrying on could have got another request through —
    /// but anything left unmarked comes back from [`Thumbnails::wanted`] the
    /// next time this runs, and [`Clippo::on_bus`] runs it on every reply.
    fn fetch_thumbnails(&mut self) {
        for key in self.thumbnails.wanted(self.model.entries()) {
            if !self.ask(Request::Thumbnail(key)) {
                break;
            }
            self.thumbnails.asked(key);
        }
    }

    /// Fold in one thing that happened on the bus.
    fn on_bus(&mut self, event: bus::Event) -> Task<Message> {
        match event {
            bus::Event::Ready(sender) => {
                self.requests = Some(sender);
                // Only if something is on screen. At startup nothing is — the
                // panel draws an icon, not a picker — but the icon can be
                // clicked before the worker is up, and that click's own refresh
                // was dropped for want of a sender.
                self.refresh_if_visible();
            }
            bus::Event::Entries(query, entries) => {
                // Dropped rather than drawn when the user has typed since it was
                // asked for: a superseded ranking is on its way out anyway, and
                // showing it costs the *next* one its landing rule. See
                // [`Model::accepts`].
                if self.model.accepts(&query) {
                    self.model.set_entries(entries);
                    self.thumbnails.prune(self.model.entries());
                    self.fetch_thumbnails();
                    // The landing rule has just chosen a row, and the list is
                    // still scrolled wherever it was: a fresh ranking puts the
                    // highlight on the top row under a list the user had
                    // arrowed halfway down, and a deletion moves every row
                    // below the gap. Both leave the highlight off screen
                    // unless the list follows it.
                    return self.keep_selection_visible();
                }
            }
            bus::Event::Revealed(id, value) => {
                // Straight into the model, which will only draw it while its
                // row stays selected. Nothing else holds a copy.
                self.model.set_revealed(id, value);
            }
            bus::Event::Thumbnail(key, bytes) => {
                self.thumbnails.store(key, bytes);
                // A reply means a slot in the request queue has come free, so
                // anything `fetch_thumbnails` had to leave behind can go now.
                // This is what makes a list with more image rows than the queue
                // holds finish rather than stopping at the first batch.
                self.fetch_thumbnails();
            }
            bus::Event::Toggle => return self.toggle(),
            bus::Event::DaemonUp => {
                // This arrives from `HistoryChanged`, from the daemon
                // reappearing, and from every successful call — so on a busy
                // desktop it is frequent, and the picker is closed for almost
                // all of it. The list is stale either way; it only needs
                // re-reading when somebody can see it.
                self.model.set_status(Status::Connected);
                self.refresh_if_visible();
            }
            bus::Event::DaemonDown => {
                self.model.set_status(Status::DaemonUnavailable);
                // A restarted daemon has a new database handle and may have
                // swept on startup, so nothing cached about the old one is
                // worth keeping.
                self.thumbnails.clear();
            }
            bus::Event::DaemonUntrusted(why) => {
                // On screen, not only in the journal. The reason this is worth
                // a state of its own is that the alternative — reconnecting and
                // looking normal — is what makes a peer taking the daemon's
                // name invisible to the person it is being taken from.
                warn!(
                    why,
                    "clippo-applet is refusing to talk to the daemon's name owner"
                );
                self.model.set_status(Status::DaemonUntrusted(why));
                self.thumbnails.clear();
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
            Action::Previous => {
                self.model.select_previous();
                return self.keep_selection_visible();
            }
            Action::Next => {
                self.model.select_next();
                return self.keep_selection_visible();
            }
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

/// How far down its travel the list has to sit for row `index` of `rows` to be
/// on screen, as the fraction `snap_to` takes.
///
/// The rows are spread evenly over the travel: the first row is the list at the
/// top, the last row is the list at the bottom, and row *i* of *n* is
/// *i*/(*n*−1) of the way between them. That is not an approximation for rows
/// of one height — it is exactly enough. The list is `n·h` tall in a viewport
/// of `V`, so this offset is `i/(n-1)·(n·h - V)`, and the row spans `i·h` to
/// `(i+1)·h`; both "the row's top is at or below the offset" and "its bottom is
/// at or above the offset plus `V`" reduce to `V ≥ h`, which is to say the
/// popup is showing at least one whole row.
///
/// Rows are not quite one height — an image row is a thumbnail tall where a
/// text row is a line of text, and a revealed row is taller than either — so a
/// list mixing them lands a few pixels out rather than exactly. That is the
/// difference between a highlight at the edge of the popup and one just inside
/// it, not between a visible highlight and an invisible one, and it is the
/// price of not having to measure a widget tree from here.
///
/// A side effect worth naming: this scrolls a little on *every* move rather
/// than only when the highlight would leave the popup. That is the intended
/// reading of "the scrollbar moves with the selection" — the bar tracks where
/// the user is in the history, the way dragging it does.
fn selection_offset(index: usize, rows: usize) -> f32 {
    // Nothing to travel over, and `rows - 1` would divide by zero.
    if rows < 2 {
        return 0.0;
    }
    // Clamped rather than trusted: `snap_to` clamps the fraction anyway, and a
    // row number from outside the list is a bug that should scroll to the end
    // rather than produce a NaN offset.
    index.min(rows - 1) as f32 / (rows - 1) as f32
}

/// Translate a raw key press into an [`Action`].
///
/// A free function taking the event rather than a closure because
/// `listen_with` wants a plain `fn`, and separate from [`Clippo`] because it is
/// the one piece of key handling that can be unit tested.
fn action_for(key: &Key, modifiers: Modifiers) -> Option<Action> {
    // `macos_command` is not a thing here, but `control` alone must not match
    // when other modifiers are held: `Ctrl+Shift+P` is not `Ctrl+P`, and nor is
    // `Super+Ctrl+P` — Logo is the modifier a desktop's own shortcuts are on,
    // so a chord that includes it belongs to whatever bound it.
    let only_control =
        modifiers.control() && !modifiers.alt() && !modifiers.shift() && !modifiers.logo();

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
            thumbnails: Thumbnails::new(),
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
                    // The picker is gone, so the revealed value goes with it —
                    // M5's "not after the popup closes", for the path where the
                    // compositor closed it rather than the applet.
                    self.model.forget_revealed();
                }
                Task::none()
            }

            // Stands in for the click-outside dismissal an `xdg_popup` grab did
            // for free — see [`crate::surface`]. Routed through `dismiss` rather
            // than through `Picker::closed` because the surface is still there:
            // it has to be destroyed, not just forgotten.
            Message::Unfocused(id) if self.picker.id() == Some(id) => self.dismiss(),
            Message::Unfocused(_) => Task::none(),

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
        // Spelled out rather than `applet.icon_button(PANEL_ICON)`, which is
        // otherwise exactly this, because that helper takes a name and so
        // cannot carry a fallback. Everything else here — symbolic, and the
        // panel's suggested size — is what it does.
        let handle = widget::icon::from_name(PANEL_ICON)
            .symbolic(true)
            .size(self.core.applet.suggested_size(true).0)
            .fallback(Some(widget::icon::IconFallback::Names(vec![
                PANEL_ICON_FALLBACK.into(),
            ])));

        self.core
            .applet
            .icon_button_from_handle(handle.into())
            .on_press(Message::IconPressed)
            .into()
    }

    /// The popup's contents.
    fn view_window(&self, id: Id) -> Element<'_, Message> {
        if self.picker.id() != Some(id) {
            return cosmic::widget::text("").into();
        }
        self.picker
            .content(view::picker(&self.model, &self.thumbnails))
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
                    // The only route to knowing the picker's surface exists. The
                    // `Application` trait has `on_close_requested` for the other
                    // half of this and no counterpart for the opening, so it is
                    // read off the runtime's event stream instead.
                    Event::Window(cosmic::iced::window::Event::Opened { .. }) => {
                        Some(Message::Opened(window))
                    }
                    // A layer surface is not dismissed by a click outside it the
                    // way a popup with a grab was, so losing keyboard focus is
                    // what stands in for that.
                    //
                    // It has to be read from the Wayland event rather than from
                    // `window::Event::Unfocused`, which iced only emits for a
                    // real window: a layer surface's keyboard leave arrives as
                    // `LayerEvent::Unfocused` and nothing else
                    // (`iced/winit/src/platform_specific/wayland/sctk_event.rs`,
                    // the `KeyboardEventVariant::Leave` arm, which matches on
                    // the surface kind).
                    //
                    // The picker being closed by the *compositor* needs nothing
                    // here: `LayerEvent::Done` is one of the two events libcosmic
                    // turns into `on_close_requested`, so it already arrives as
                    // `Message::Closed`.
                    Event::PlatformSpecific(PlatformSpecific::Wayland(wayland::Event::Layer(
                        wayland::LayerEvent::Unfocused,
                        _,
                        id,
                    ))) => Some(Message::Unfocused(id)),
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

    /// Logo especially: `Super` is where a desktop puts its own shortcuts, and
    /// `Super+V` is the one clippo asks the user to bind. `Super+Ctrl+P` is
    /// somebody else's chord.
    #[test]
    fn a_super_chord_is_not_a_control_binding() {
        let with_logo = Modifiers::CTRL | Modifiers::LOGO;
        assert_eq!(action_for(&character("p"), with_logo), None);
        assert_eq!(action_for(&character("r"), with_logo), None);
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

    /// The two ends, which are the ones the user notices: arrowing to the last
    /// row must put the list at the bottom, and arrowing back to the first must
    /// put it at the top.
    #[test]
    fn the_ends_of_the_list_are_the_ends_of_the_travel() {
        assert_eq!(selection_offset(0, 20), 0.0);
        assert_eq!(selection_offset(19, 20), 1.0);
        assert_eq!(selection_offset(1, 3), 0.5);
    }

    /// A list that fits, or one row, has nowhere to scroll to — and `rows - 1`
    /// is the division this must not do.
    #[test]
    fn a_list_with_nothing_to_scroll_stays_where_it_is() {
        assert_eq!(selection_offset(0, 1), 0.0);
        assert_eq!(selection_offset(0, 0), 0.0);
        assert_eq!(selection_offset(4, 1), 0.0);
    }

    /// The bug, as a property: every row of a long list has an offset that is a
    /// real fraction, and moving down the list only ever moves the list down.
    #[test]
    fn every_row_of_a_long_list_has_somewhere_to_scroll_to() {
        let rows = 200;
        let mut previous = -1.0;

        for index in 0..rows {
            let offset = selection_offset(index, rows);
            assert!(offset.is_finite(), "row {index}");
            assert!((0.0..=1.0).contains(&offset), "row {index}: {offset}");
            assert!(
                offset > previous,
                "row {index} scrolls no further than {previous}"
            );
            previous = offset;
        }
    }

    /// An index past the end is a bug elsewhere; it must not become a NaN
    /// offset, which `snap_to` would clamp into nothing sensible.
    #[test]
    fn a_row_number_past_the_end_scrolls_to_the_end() {
        assert_eq!(selection_offset(99, 5), 1.0);
    }

    #[test]
    fn a_key_that_means_nothing_here_is_left_alone() {
        assert_eq!(action_for(&key(Named::Tab), Modifiers::default()), None);
        assert_eq!(action_for(&key(Named::F5), Modifiers::default()), None);
        assert_eq!(action_for(&character("a"), Modifiers::default()), None);
    }
}
