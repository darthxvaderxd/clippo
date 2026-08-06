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
//! The other consequence is that the list has to be scrolled from here too. A
//! widget that never has focus never sees the arrows, so it has no idea the
//! selection moved and no reason to follow it — see
//! [`Clippo::keep_selection_visible`], and [`Clippo::note_list`] for the one
//! thing it needs the widget to tell it back.
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
use cosmic::iced::widget::scrollable::{scroll_to, AbsoluteOffset, Viewport};
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
    /// The list said where it is scrolled to and how big it is.
    ListScrolled(Viewport),
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
    /// What the list last said about itself, or `None` while it has never had
    /// more rows than fit. See [`Clippo::note_list`].
    list: Option<ListMetrics>,
}

/// What the list looks like right now, as far as this side knows.
///
/// Measurements rather than estimates: every figure here comes from the widget
/// itself, by way of [`Clippo::note_list`]. That is what a scroll has to be
/// decided against — where a row *should* go is arithmetic, but whether it is
/// on screen already is a fact about a layout only the widget has done.
#[derive(Debug, Clone, Copy, PartialEq)]
struct ListMetrics {
    /// How much of the list is on screen, in logical pixels.
    viewport: f32,
    /// Where the list is scrolled to, in logical pixels from the top of the
    /// content.
    offset: f32,
    /// Real pixels per estimated pixel: the height the list came out at over
    /// the height [`view::row_height`] predicts for the same rows.
    scale: f32,
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
        // A new surface gets a new widget tree, and a new scrollable starts at
        // the top however far down the last one was left. Said here so that the
        // first scroll after reopening is decided against where the list really
        // is rather than against where the closed one was.
        self.list = self.list.map(|list| ListMetrics {
            offset: 0.0,
            ..list
        });
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

    /// Take note of where the list is and how big it turned out to be.
    ///
    /// The one thing the widget tells this side about itself. iced publishes it
    /// whenever the viewport changes and the content overflows — from the
    /// redraw pass as much as from the wheel — so it arrives with the picker's
    /// first frame, before the user can press anything, and again after every
    /// scroll they make themselves.
    ///
    /// [`ListMetrics::scale`] is what makes [`view::row_height`]'s estimates
    /// usable. They are guesses at a widget tree this side cannot measure;
    /// dividing what the list really came out at by what they predict corrects
    /// whatever they are uniformly wrong by — a theme's font size, a padding
    /// guessed at. A list of one kind of row is then placed exactly however
    /// wrong the estimate was, and a mixed one is left with only the error in
    /// the *ratio* between an image row and a text row.
    ///
    /// The scale is the one figure a revealed row invalidates: it is drawn at
    /// whatever height its value needs, which is not a height
    /// [`view::row_height`] models, so a measurement taken while one is on
    /// screen says nothing about the size of an ordinary row. The position and
    /// the viewport are still facts, so only the scale is held over — and if
    /// there is no earlier one to hold over, the whole measurement is dropped
    /// rather than kept beside a made-up scale. That needs the first viewport
    /// this list ever publishes to arrive with a value already revealed, which
    /// takes a selection, which takes a frame that would have published one.
    fn note_list(&mut self, viewport: Viewport) {
        let scale = match self.model.revealed() {
            Some(_) => self.list.map(|list| list.scale),
            None => {
                let estimated = content_height(&self.row_heights());
                let scale = viewport.content_bounds().height / estimated;
                (estimated > 0.0 && scale.is_finite() && scale > 0.0).then_some(scale)
            }
        };
        let Some(scale) = scale else {
            return;
        };
        self.list = Some(ListMetrics {
            viewport: viewport.bounds().height,
            offset: viewport.absolute_offset().y,
            scale,
        });
    }

    /// The heights the list is drawing its rows at, in the order it draws them.
    ///
    /// Estimates — [`view::row_height`] says what of and why — and they are
    /// gathered here rather than in the geometry below so that the arithmetic
    /// stays a function of plain numbers, which is the part worth testing
    /// without a compositor.
    fn row_heights(&self) -> Vec<f32> {
        self.model
            .entries()
            .iter()
            .map(|entry| view::row_height(entry, self.thumbnails.get(entry).is_some()))
            .collect()
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
    /// Nothing happens until the list has reported itself, because every figure
    /// the decision needs is the widget's rather than the model's. That is not
    /// a gap: iced publishes the viewport from the redraw pass, so it is here
    /// before the first keystroke, and a list that has never reported one is a
    /// list that has never overflowed its popup — which is the case with
    /// nothing to scroll.
    ///
    /// The offset is *absolute*, and that is not an implementation detail. An
    /// offset stored as a fraction — which is what `snap_to` leaves behind — is
    /// re-resolved against the content every time the content changes height,
    /// so a list left at one slides as soon as a row grows. `Ctrl+R` grows one
    /// row by up to `REVEAL_LINES` × `REVEAL_CHARS` worth of text, and that
    /// slide would land the user in the middle of the value they asked to read
    /// instead of at its first line. A pixel offset leaves the rows above the
    /// revealed one exactly where they are and lets it expand downwards, which
    /// is the behaviour `REVEAL_LINES` is written around — so the reveal path
    /// issues no scroll of its own and does not need to.
    fn keep_selection_visible(&mut self) -> Task<Message> {
        let (Some(index), Some(list)) = (self.model.selected_index(), self.list) else {
            return Task::none();
        };
        let Some(offset) = scroll_offset(&self.row_heights(), index, list) else {
            return Task::none();
        };
        // Recorded as well as sent, because two arrow presses can be handled
        // between one frame and the next: the second would otherwise decide
        // against the position the first has already moved away from.
        self.list = Some(ListMetrics { offset, ..list });
        scroll_to(
            view::list_id(),
            AbsoluteOffset {
                // Left alone: the list only scrolls vertically, and naming a
                // horizontal offset would fight anything that ever changes it.
                x: None,
                y: Some(offset),
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
                    let landed = self.model.set_entries(entries);
                    self.thumbnails.prune(self.model.entries());
                    self.fetch_thumbnails();
                    // Only when the landing rule placed the highlight — a fresh
                    // ranking puts it on the top row of a list the user had
                    // arrowed halfway down, and deleting the selected row hands
                    // it to whatever moved up into the gap. Both leave it off
                    // screen unless the list follows.
                    //
                    // Not otherwise, and that is the case this reads for.
                    // `HistoryChanged` fires on every copy anyone makes, so a
                    // user who wheeled down the list without touching the
                    // arrows would be dragged back to their highlight by an
                    // unrelated copy in another window. The highlight has not
                    // moved there; the list has no business moving either.
                    //
                    // The model answers this rather than a comparison here,
                    // because neither of the two things this side can see says
                    // it. A fresh ranking that puts the already-selected entry
                    // first leaves the id alone while moving the highlight to
                    // the top; a copy in another window leaves the id alone and
                    // moves every row's *index* down by one without the
                    // highlight having moved at all.
                    if landed {
                        return self.keep_selection_visible();
                    }
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

/// How tall `heights` are drawn altogether, gaps included.
fn content_height(heights: &[f32]) -> f32 {
    let gaps = heights.len().saturating_sub(1) as f32 * view::ROW_SPACING;
    heights.iter().sum::<f32>() + gaps
}

/// Where row `index` starts, measured from the top of the content.
///
/// The rows above it, and one gap for each of them — the gap that separates the
/// last of them from this one included.
fn row_top(heights: &[f32], index: usize) -> f32 {
    let above = &heights[..index.min(heights.len())];
    above.iter().sum::<f32>() + above.len() as f32 * view::ROW_SPACING
}

/// Where the list has to be scrolled to for row `index` to be on screen, in
/// pixels from the top of the content — or `None` for a row that is on screen
/// already.
///
/// `None` rather than the offset it is already at, because the two are
/// different instructions and only one of them is right. A row that is visible
/// wants *no* scroll: the list moving under a highlight that did not need it to
/// move is the thing a user who reached for the wheel would notice, and it is
/// what leaves a position they chose alone while they arrow between two rows
/// that are both on screen. It is also what the task asks for in as many
/// words — the list follows the selection when the selection goes past the
/// visible portion of it.
///
/// The rule is the ordinary one: a row above the viewport is brought to the top
/// of it, a row below is brought to the bottom, and anything between is left
/// where it is. A row taller than the whole viewport wants both and can have
/// neither, and is shown from its top, because the top of a value is where
/// reading it starts.
///
/// `heights` are estimates and [`ListMetrics::scale`] is what turns them into
/// the pixels the other two figures are in — see [`Clippo::note_list`]. The
/// residual error is in the *ratio* between a thumbnailed row and a text row,
/// which is a few pixels over a list rather than the accumulated difference
/// between them that treating every row as one height would leave: fifty rows
/// of an image-heavy history are what put the highlight a whole row off screen.
fn scroll_offset(heights: &[f32], index: usize, list: ListMetrics) -> Option<f32> {
    let height = heights.get(index)? * list.scale;
    let top = row_top(heights, index) * list.scale;
    let bottom = top + height;

    // Nothing to scroll, whatever the arithmetic says: the whole list is on
    // screen. Worth checking rather than leaving to iced's own clamp, because
    // the metrics can be a frame behind a list that has just got shorter.
    if content_height(heights) * list.scale <= list.viewport {
        return None;
    }

    if top < list.offset {
        Some(top)
    } else if bottom > list.offset + list.viewport {
        // `min(top)` for the row that is taller than the viewport: bringing its
        // bottom into view would put its first line above the top of the popup.
        Some((bottom - list.viewport).min(top))
    } else {
        None
    }
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
            list: None,
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

            Message::ListScrolled(viewport) => {
                self.note_list(viewport);
                Task::none()
            }

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

    /// A list of the given row heights, seen through a popup `viewport` tall
    /// and scrolled to `offset`. `scale` is 1 unless a test is about it: the
    /// estimates are the pixels.
    fn seen(viewport: f32, offset: f32) -> ListMetrics {
        ListMetrics {
            viewport,
            offset,
            scale: 1.0,
        }
    }

    /// A text row and a thumbnailed image row, at the heights
    /// [`view::row_height`] gives them — see
    /// [`the_two_row_heights_are_the_ones_the_list_draws`]. Named rather than
    /// inlined because the difference between the two is what the interesting
    /// tests are about.
    const TEXT: f32 = 29.0;
    const IMAGE: f32 = 48.0;

    /// Which the fixtures above have to stay honest about, or the mixed-list
    /// test stops being about a list this applet draws.
    #[test]
    fn the_two_row_heights_are_the_ones_the_list_draws() {
        let text = clippo_ipc::EntrySummary {
            id: 1,
            created_at: 1,
            last_used_at: 1,
            kind: "text".to_owned(),
            preview: "hello".to_owned(),
            pinned: false,
            sensitive: false,
        };
        let image = clippo_ipc::EntrySummary {
            kind: crate::model::IMAGE_KIND.to_owned(),
            ..text.clone()
        };

        assert_eq!(view::row_height(&text, false), TEXT);
        assert_eq!(view::row_height(&image, true), IMAGE);
        assert_eq!(
            view::row_height(&image, false),
            TEXT,
            "an image row is a line of text tall until its thumbnail arrives"
        );
    }

    /// Rows above the viewport come to the top of it, rows below come to the
    /// bottom, and a row already on screen is left alone — which is what stops
    /// the list moving under a highlight that did not need it to.
    #[test]
    fn only_a_row_off_screen_moves_the_list() {
        let heights = [TEXT; 20];

        assert_eq!(scroll_offset(&heights, 0, seen(100.0, 0.0)), None);
        assert_eq!(scroll_offset(&heights, 2, seen(100.0, 0.0)), None);
        // Row 4 spans 124..153 against a viewport of 0..100.
        assert_eq!(scroll_offset(&heights, 4, seen(100.0, 0.0)), Some(53.0));
        // And back up: row 1 starts at 31, above a list scrolled to 200.
        assert_eq!(scroll_offset(&heights, 1, seen(100.0, 200.0)), Some(31.0));
    }

    /// The bug, as the property the fix has to have: arrowing from one end of
    /// the list to the other and back leaves the highlighted row fully on
    /// screen at every step.
    ///
    /// Rows of *mixed* height, because that is the case a list treated as one
    /// row height gets wrong — and gets wrong by more the further down it goes,
    /// since what accumulates is the difference between the real rows above the
    /// selection and the assumed ones. Ten previews above forty screenshots is
    /// an ordinary afternoon's history.
    #[test]
    fn every_row_of_a_mixed_list_is_fully_on_screen_once_arrowed_onto() {
        let heights: Vec<f32> = std::iter::repeat_n(TEXT, 10)
            .chain(std::iter::repeat_n(IMAGE, 40))
            .collect();
        let viewport = 450.0;
        let content = content_height(&heights);
        let mut offset = 0.0;

        let arrow_onto = |index: usize, offset: &mut f32| {
            if let Some(scrolled) = scroll_offset(&heights, index, seen(viewport, *offset)) {
                *offset = scrolled;
            }
            let top = row_top(&heights, index);
            let bottom = top + heights[index];
            assert!(
                *offset <= top && bottom <= *offset + viewport,
                "row {index} spans {top}..{bottom}, viewport {offset}..{}",
                *offset + viewport
            );
            assert!(
                (0.0..=content - viewport).contains(offset),
                "row {index} scrolled to {offset}, outside 0..={}",
                content - viewport
            );
        };

        for index in 0..heights.len() {
            arrow_onto(index, &mut offset);
        }
        for index in (0..heights.len()).rev() {
            arrow_onto(index, &mut offset);
        }
    }

    /// A list shorter than the popup has nowhere to go, whatever the metrics
    /// say — they can be a frame behind a list that has just got shorter.
    #[test]
    fn a_list_that_fits_does_not_scroll() {
        assert_eq!(scroll_offset(&[TEXT; 3], 2, seen(450.0, 0.0)), None);
        assert_eq!(scroll_offset(&[TEXT], 0, seen(450.0, 120.0)), None);
        assert_eq!(scroll_offset(&[], 0, seen(450.0, 0.0)), None);
    }

    /// A row number past the end is a bug elsewhere, and must not panic on the
    /// slice or scroll to a position nothing is at.
    #[test]
    fn a_row_number_past_the_end_scrolls_nowhere() {
        assert_eq!(scroll_offset(&[TEXT; 5], 99, seen(60.0, 0.0)), None);
        assert_eq!(row_top(&[TEXT; 5], 99), row_top(&[TEXT; 5], 5));
    }

    /// The estimates are only ever a proportion: the list says how tall it
    /// really came out, and everything here is scaled by what that makes of
    /// them. A theme with larger text scrolls further for the same row.
    #[test]
    fn the_measured_scale_moves_the_row_with_it() {
        let heights = [TEXT; 20];
        let doubled = ListMetrics {
            scale: 2.0,
            ..seen(100.0, 0.0)
        };

        // Row 4 spans 248..306 once doubled, so the bottom of it is 206.
        assert_eq!(scroll_offset(&heights, 4, doubled), Some(206.0));
        // And row 1, at 62, is off the bottom of the same viewport where it was
        // comfortably inside it before.
        assert_eq!(scroll_offset(&heights, 1, doubled), Some(20.0));
        assert_eq!(scroll_offset(&heights, 1, seen(100.0, 0.0)), None);
    }

    /// A row taller than the popup cannot be shown whole, and the end of it to
    /// show is the top: a revealed value read from its middle is not read.
    #[test]
    fn a_row_taller_than_the_popup_is_shown_from_its_top() {
        let heights = [TEXT, 900.0, TEXT];

        assert_eq!(scroll_offset(&heights, 1, seen(450.0, 0.0)), Some(31.0));
        assert_eq!(scroll_offset(&heights, 1, seen(450.0, 600.0)), Some(31.0));
    }

    /// The gaps between the rows are part of where a row is. Fifty of them is
    /// three rows' worth of drift in a list that ignored them.
    #[test]
    fn the_gap_between_rows_counts_towards_where_a_row_is() {
        assert_eq!(row_top(&[TEXT; 4], 0), 0.0);
        assert_eq!(row_top(&[TEXT; 4], 1), TEXT + view::ROW_SPACING);
        assert_eq!(row_top(&[TEXT; 4], 3), 3.0 * (TEXT + view::ROW_SPACING));
        assert_eq!(
            content_height(&[TEXT; 4]),
            4.0 * TEXT + 3.0 * view::ROW_SPACING
        );
        assert_eq!(content_height(&[]), 0.0);
        assert_eq!(content_height(&[TEXT]), TEXT);
    }

    #[test]
    fn a_key_that_means_nothing_here_is_left_alone() {
        assert_eq!(action_for(&key(Named::Tab), Modifiers::default()), None);
        assert_eq!(action_for(&key(Named::F5), Modifiers::default()), None);
        assert_eq!(action_for(&character("a"), Modifiers::default()), None);
    }
}
