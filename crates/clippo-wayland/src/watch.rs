//! The watcher thread: one Wayland connection, one calloop event loop, a
//! `tokio::sync::mpsc` channel of events out to the daemon, and a command
//! channel back in for the copy-back path.
//!
//! # Why the offer half lives on this thread too
//!
//! Serving a paste means answering a `send` event on the same Wayland
//! connection the captures arrive on, so the source belongs where the device
//! is. The daemon therefore never touches a Wayland object: it sends a
//! [`Command::Offer`] down a `calloop` channel — see [`WaylandClipboard`] — and
//! this loop does the protocol work. That keeps every `wayland_client` proxy on
//! one thread, which is what the library's `Dispatch` model wants anyway.

use std::collections::{HashMap, VecDeque};
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc as sync_mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use calloop::channel::{self, Event as ChannelEvent};
use calloop::generic::{Generic, NoIoDrop};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken};
use calloop_wayland_source::WaylandSource;
use clippo_core::display::one_line;
use rustix::io::Errno;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, EventQueue, QueueHandle};

use crate::flavor::{PendingSelection, Push};
use crate::offer::{self, BlobWriter, OfferedFlavor, WriteProgress};
use crate::protocol::{DataControlSink, Device, Manager, Offer, Source};
use crate::{
    mime, Clipboard, DropReason, Error, Flavor, OfferError, SelectionKind, WatchConfig, WatchEvent,
};

/// How much we take off a pipe per `read`. Bounded so that a flavor cannot
/// overshoot its cap by more than one chunk before we notice.
const READ_CHUNK: usize = 64 * 1024;

/// How many copy-back commands may queue up before the daemon is told the
/// watcher is not keeping up.
///
/// One is enough in practice — a command is handled on the next turn of the
/// loop — so a full channel means the loop is not turning at all, which is a
/// failure worth reporting rather than blocking a D-Bus method on.
const COMMAND_QUEUE: usize = 16;

/// How much of a MIME type reaches the journal, and in what form.
///
/// Every MIME string in a `warn!` below is one a client chose: a type it
/// advertised, or one it asked to paste. That makes it the same hostile input a
/// preview is — a copied `ESC [ 2 J` or `U+202E` acts on whoever reads the log
/// just as it acts on whoever reads a list — and [`one_line`] is the escaping
/// the previews already get, rather than a second one written here. The width
/// is generous next to the ~25 characters of the longest type clippo wants.
const MIME_LOG_CHARS: usize = 128;

/// A MIME type as it goes into the journal.
///
/// One call, so that every `warn!` carrying a client's string renders it the
/// same way and a test can pin what they render. The escaping itself is
/// [`one_line`]'s — `clippo_core::display`'s, shared with the previews — and
/// not a second implementation of it.
fn for_log(mime: &str) -> String {
    one_line(mime, MIME_LOG_CHARS)
}

/// How many *uninteresting* MIME types one offer may advertise before clippo
/// stops recording them.
///
/// The list is kept for the life of the offer and travels to the daemon as
/// `Selection::advertised`, so its size is a per-selection allocation a client
/// picks. Only the seven of [`mime::INTERESTING_MIMES`] are ever read from a
/// pipe, but the rest are worth reporting — "clippo kept none of it, and here
/// is what was on offer" is what makes a missed capture diagnosable. Real
/// sources advertise dozens (a rich-text editor offering every flavor of a
/// copied selection lands in the tens), so a ceiling of 256 sits an order of
/// magnitude above the busiest honest source.
///
/// This bounds the *diagnostic* list only — see [`AdvertisedMimes::push`], which
/// keeps an interesting type past the ceiling rather than truncating flat, so
/// the ceiling can never decide what clippo captures. The total is therefore
/// this plus [`mime::INTERESTING_MIMES`]`.len()`.
///
/// On size: a MIME string is itself bounded by the Wayland message the source
/// sends it in, a few kilobytes, so the worst case here is on the order of a
/// megabyte. That is transient — the daemon does not store `advertised` — and
/// small beside the seven flavors it may buy at the per-flavor cap.
const MAX_ADVERTISED_MIMES: usize = 256;

/// A running watcher thread.
///
/// Dropping this does *not* stop the thread — call [`Watcher::stop`], or drop
/// the [`WatchEvent`] receiver, which the watcher takes as its cue to shut down.
#[derive(Debug)]
pub struct Watcher {
    signal: LoopSignal,
    thread: JoinHandle<()>,
    protocol: &'static str,
    clipboard: WaylandClipboard,
}

impl Watcher {
    /// Which of the two data-control protocols this watcher bound.
    pub fn protocol(&self) -> &'static str {
        self.protocol
    }

    /// A handle for putting an entry back on the clipboard.
    ///
    /// Cheap to clone and safe to hold anywhere: it is a channel sender, and
    /// every Wayland object stays on the watcher thread.
    pub fn clipboard(&self) -> WaylandClipboard {
        self.clipboard.clone()
    }

    /// Ask the watcher to shut down. Returns once it has.
    ///
    /// Anything clippo put on the clipboard goes with it — the source dies with
    /// the connection, which is what the compositor uses to decide the
    /// selection is gone. See [`WaylandClipboard::offer`].
    pub fn stop(self) {
        self.signal.stop();
        self.signal.wakeup();
        if self.thread.join().is_err() {
            error!("the wayland watcher thread panicked");
        }
    }
}

/// Something the daemon asks the watcher thread to do.
#[derive(Debug)]
pub(crate) enum Command {
    /// Take the clipboard, advertising these flavors.
    Offer(Vec<Flavor>),
}

/// The [`Clipboard`] the real compositor is behind.
///
/// Clone it freely; every clone talks to the one watcher thread.
#[derive(Debug, Clone)]
pub struct WaylandClipboard {
    commands: channel::SyncSender<Command>,
}

impl Clipboard for WaylandClipboard {
    /// Put these flavors on the clipboard and keep them there.
    ///
    /// Returns as soon as the command is queued, not once the compositor has
    /// acted on it: the reply would be a round trip through the loop, and there
    /// is nothing a caller could usefully do with the difference.
    ///
    /// **The daemon owning the selection is what keeps it populated.** Wayland
    /// has no "clipboard" that outlives its owner: the bytes live in this
    /// process and are written to a pipe on each paste, so when `clippod` exits
    /// the clipboard empties. That is the protocol working as designed, not a
    /// clippo bug — see the README.
    fn offer(&self, flavors: Vec<Flavor>) -> Result<(), OfferError> {
        if flavors.is_empty() {
            return Err(OfferError::NothingToOffer);
        }
        self.commands
            .try_send(Command::Offer(flavors))
            .map_err(|error| match error {
                sync_mpsc::TrySendError::Full(_) => OfferError::Busy,
                sync_mpsc::TrySendError::Disconnected(_) => OfferError::WatcherStopped,
            })
    }
}

/// Start watching the clipboard.
///
/// Connects, binds a data-control manager, and hands back a receiver that
/// yields one [`WatchEvent`] per captured selection — carrying every
/// interesting flavor of that selection together — and one when another
/// application takes the clipboard away from us.
///
/// Connection and binding happen on the watcher thread but are waited for here,
/// so a missing data-control protocol is reported to the caller rather than
/// logged and forgotten.
pub fn watch(config: WatchConfig) -> Result<(Watcher, mpsc::Receiver<WatchEvent>), Error> {
    let (tx, rx) = mpsc::channel(config.channel_capacity.max(1));
    let (commands, command_source) = channel::sync_channel::<Command>(COMMAND_QUEUE);
    let (ready_tx, ready_rx) = sync_mpsc::sync_channel::<Result<Started, Error>>(1);

    let thread = std::thread::Builder::new()
        .name("clippo-wayland".to_owned())
        .spawn(move || run(config, tx, command_source, &ready_tx))
        .map_err(Error::SpawnThread)?;

    match ready_rx.recv() {
        Ok(Ok(started)) => Ok((
            Watcher {
                signal: started.signal,
                thread,
                protocol: started.protocol,
                clipboard: WaylandClipboard { commands },
            },
            rx,
        )),
        Ok(Err(error)) => {
            let _ = thread.join();
            Err(error)
        }
        // The thread went away without reporting either way.
        Err(_) => {
            let _ = thread.join();
            Err(Error::WatcherStopped)
        }
    }
}

/// What a successful startup reports back to [`watch`].
struct Started {
    signal: LoopSignal,
    protocol: &'static str,
}

/// Everything bound before the event loop exists.
struct Bound {
    conn: Connection,
    queue: EventQueue<WatchState>,
    manager: Manager,
    device: Device,
    seat: WlSeat,
}

fn connect_and_bind(config: &WatchConfig) -> Result<Bound, Error> {
    let conn = Connection::connect_to_env()?;
    let (globals, queue) = wayland_client::globals::registry_queue_init::<WatchState>(&conn)?;
    let qh = queue.handle();

    let manager = Manager::bind(&globals, &qh, config.primary)?;

    // `bind` takes the first seat the compositor advertised. COSMIC has one;
    // if that ever stops being true, the clipboard still belongs to the seat
    // the user is typing on, which is the first one.
    let seat = globals
        .bind::<WlSeat, _, _>(&qh, 1..=1, ())
        .map_err(|_| Error::NoSeat)?;

    let device = manager.device(&seat, &qh);

    Ok(Bound {
        conn,
        queue,
        manager,
        device,
        seat,
    })
}

fn run(
    config: WatchConfig,
    tx: mpsc::Sender<WatchEvent>,
    commands: channel::Channel<Command>,
    ready: &sync_mpsc::SyncSender<Result<Started, Error>>,
) {
    let bound = match connect_and_bind(&config) {
        Ok(bound) => bound,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    let mut event_loop: EventLoop<'static, WatchState> = match EventLoop::try_new() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            let _ = ready.send(Err(Error::EventLoop(error)));
            return;
        }
    };
    let handle = event_loop.handle();
    let signal = event_loop.get_signal();

    let conn = bound.conn.clone();
    let qh = bound.queue.handle();
    if let Err(error) = WaylandSource::new(conn.clone(), bound.queue).insert(handle.clone()) {
        let _ = ready.send(Err(Error::EventLoop(error.error)));
        return;
    }
    if let Err(error) = handle.insert_source(commands, |event, _, state| match event {
        ChannelEvent::Msg(command) => state.on_command(command),
        // Every `WaylandClipboard` has been dropped. Captures carry on; there
        // is simply nobody left who can ask for a copy-back.
        ChannelEvent::Closed => debug!("nothing can ask clippo to set the clipboard any more"),
    }) {
        let _ = ready.send(Err(Error::EventLoop(error.error)));
        return;
    }

    let protocol = bound.manager.protocol();
    if config.primary && !bound.manager.supports_primary() {
        warn!(
            protocol,
            "primary selection is enabled but this data-control protocol does not offer it"
        );
    }
    let mut state = WatchState {
        conn,
        qh,
        handle,
        signal: signal.clone(),
        config,
        tx,
        offers: HashMap::new(),
        pending: None,
        generation: 0,
        offered: None,
        writes: HashMap::new(),
        write_order: VecDeque::new(),
        next_write: 0,
        manager: bound.manager,
        device: bound.device,
        _seat: bound.seat,
    };

    if ready.send(Ok(Started { signal, protocol })).is_err() {
        // Nobody is listening for selections; do not bother starting.
        state.device.destroy();
        return;
    }

    info!(protocol, "watching the clipboard");
    if let Err(error) = event_loop.run(None, &mut state, |_state| {}) {
        error!(%error, "the wayland event loop stopped");
    }
    state.shutdown();
}

/// Live state of the watcher, shared by every calloop and Wayland callback.
pub(crate) struct WatchState {
    conn: Connection,
    qh: QueueHandle<WatchState>,
    handle: LoopHandle<'static, WatchState>,
    signal: LoopSignal,
    config: WatchConfig,
    tx: mpsc::Sender<WatchEvent>,
    /// Offers introduced by `data_offer` but not yet claimed by a `selection`.
    offers: HashMap<ObjectId, OfferState>,
    /// The selection currently being read, if any.
    pending: Option<Pending>,
    generation: u64,
    /// What clippo itself has on the clipboard, if anything.
    offered: Option<OwnedOffer>,
    /// Pastes still being written, by id. A write that finishes inside its own
    /// callback takes its own entry out, for the reason [`Pending::read_tokens`]
    /// gives.
    writes: HashMap<u64, PendingWrite>,
    /// The same ids in the order they started, so the oldest is the one evicted
    /// when too many pastes are outstanding at once.
    write_order: VecDeque<u64>,
    next_write: u64,
    manager: Manager,
    device: Device,
    _seat: WlSeat,
}

struct OfferState {
    offer: Offer,
    mimes: AdvertisedMimes,
}

/// What one offer advertised, bounded.
///
/// Its own type rather than a bare `Vec` so the ceiling is applied in one place
/// and can be tested without a compositor — an [`Offer`] cannot be built
/// without one.
#[derive(Default)]
struct AdvertisedMimes {
    mimes: Vec<String>,
    /// Whether the ceiling has been reached, so it is reported once per offer
    /// rather than once per type discarded past it.
    overflowed: bool,
}

impl AdvertisedMimes {
    /// Remember one advertised MIME type, discarding it only if the list is
    /// full *and* clippo would not have read it.
    ///
    /// The ceiling bounds a diagnostic list; it must not decide what clippo
    /// captures, and a flat truncation would. This list is the input to
    /// [`interesting_flavors`], so a truncated interesting type is a copy
    /// silently not captured — and it is the input to
    /// `Selection::has_password_manager_hint`, so a truncated
    /// `x-kde-passwordManagerHint` is secret-detection rule 1 not firing and a
    /// marked credential stored with an unmasked preview. Neither is reachable
    /// from an honest source, which is exactly why it would go unnoticed.
    ///
    /// So past the ceiling the interesting types still get in. The tail that
    /// buys is bounded by the argument [`interesting_flavors`] makes: an
    /// interesting type normalises to one of [`mime::INTERESTING_MIMES`], so
    /// once the tail holds all seven every further one [`mime::same`]-matches
    /// something already in it. Deduping against the tail alone rather than the
    /// whole list is what keeps that scan bounded while the client's input is
    /// not; the cost is that a type also present in the first
    /// [`MAX_ADVERTISED_MIMES`] can appear twice, which is one entry and which
    /// [`interesting_flavors`] dedups again anyway.
    fn push(&mut self, mime: String) {
        if self.mimes.len() < MAX_ADVERTISED_MIMES {
            self.mimes.push(mime);
            return;
        }
        let tail = &self.mimes[MAX_ADVERTISED_MIMES..];
        if mime::is_interesting(&mime) && !tail.iter().any(|seen| mime::same(seen, &mime)) {
            self.mimes.push(mime);
            return;
        }
        // Once per offer: the number of types past the ceiling is the client's
        // to choose, and the journal is a shared resource.
        if !std::mem::replace(&mut self.overflowed, true) {
            warn!(
                ceiling = MAX_ADVERTISED_MIMES,
                first_ignored = %for_log(&mime),
                "an offer advertised more MIME types than clippo records; ignoring the rest, \
                 except any flavor clippo would read"
            );
        }
    }

    fn into_vec(self) -> Vec<String> {
        self.mimes
    }
}

/// The clipboard as clippo currently owns it.
struct OwnedOffer {
    source: Source,
    flavors: Vec<OfferedFlavor>,
}

/// One paste being written out, and its registrations in the loop.
struct PendingWrite {
    mime: String,
    writer: BlobWriter,
    token: RegistrationToken,
    timeout: Option<RegistrationToken>,
}

struct Pending {
    selection: PendingSelection,
    /// The offer stays alive until its last flavor has been read.
    offer: Offer,
    /// Loop registrations for the flavor reads still in the loop, by slot.
    ///
    /// A read that resolves itself takes its own entry out: calloop cannot
    /// unregister a source from inside that source's own callback, so the
    /// `PostAction::Remove` it returns has to be the thing that removes it.
    read_tokens: HashMap<usize, RegistrationToken>,
    timeout_token: Option<RegistrationToken>,
}

impl DataControlSink for WatchState {
    fn offer_created(&mut self, offer: Offer) {
        trace!(offer = ?offer.id(), "new data offer");
        self.offers.insert(
            offer.id(),
            OfferState {
                offer,
                mimes: AdvertisedMimes::default(),
            },
        );
    }

    fn offer_mime(&mut self, offer: &ObjectId, mime: String) {
        match self.offers.get_mut(offer) {
            Some(state) => state.mimes.push(mime),
            // Escaped like the `warn!`s: this is the one log site a MIME string
            // reaches without `is_interesting` having vouched for its bytes.
            None => trace!(
                ?offer,
                mime = %for_log(&mime),
                "mime advertised for an offer we do not track"
            ),
        }
    }

    fn selection_changed(&mut self, kind: SelectionKind, offer: Option<Offer>) {
        if kind == SelectionKind::Primary && !self.config.primary {
            // Only reachable on ext_data_control_v1, whose single version folds
            // primary into the same device; see `Manager::can_suppress_primary`.
            debug_assert!(
                !self.manager.can_suppress_primary(),
                "a primary selection arrived on a protocol that should not send one"
            );
            trace!("ignoring a primary selection: primary capture is disabled");
            self.retire_offers(None);
            return;
        }

        self.abandon_pending("a newer selection arrived");

        let Some(offer) = offer else {
            trace!(?kind, "selection cleared");
            self.retire_offers(None);
            return;
        };

        let id = offer.id();
        let advertised = self.retire_offers(Some(&id));
        let wanted = interesting_flavors(&advertised);

        if wanted.is_empty() {
            debug!(
                ?kind,
                advertised = advertised.len(),
                "selection has no flavor clippo can use"
            );
            // Still emitted, with nothing but the advertised list: "the
            // clipboard changed and clippo kept none of it" is a fact worth
            // reporting, and `clippo-watch` exists to report it.
        }
        self.start_reads(kind, offer, advertised, wanted);
    }

    fn device_finished(&mut self) {
        warn!("the compositor retired our data-control device");
        self.signal.stop();
        self.signal.wakeup();
    }

    fn source_send(&mut self, source: &ObjectId, mime: String, fd: OwnedFd) {
        let Some(offered) = self.offered.as_ref() else {
            // A source we have already replaced or given up. Dropping the fd
            // closes it, which the receiver reads as an empty flavor.
            trace!(%mime, "a paste arrived for a selection clippo no longer owns");
            return;
        };
        if &offered.source.id() != source {
            trace!(%mime, "a paste arrived for a superseded source");
            return;
        }
        let Some(blob) = offer::blob_for(&offered.flavors, &mime) else {
            // Only reachable if an application asks for something we never
            // advertised, which the compositor should not forward.
            warn!(
                mime = %for_log(&mime),
                "refusing a paste of a flavor clippo did not offer"
            );
            return;
        };
        self.begin_write(mime, blob, fd);
    }

    fn source_cancelled(&mut self, source: &ObjectId) {
        let Some(offered) = self.offered.as_ref() else {
            return;
        };
        if &offered.source.id() != source {
            // One of ours, but one a later `Copy` already replaced. The
            // replacement is what owns the clipboard, so this is not a loss.
            trace!("a superseded source was cancelled");
            return;
        }
        debug!("another application took the clipboard from clippo");
        // Taken, not just cleared: the source must be destroyed, and leaving it
        // in `offered` would make the next `send` look like one of ours.
        if let Some(offered) = self.offered.take() {
            offered.source.destroy();
        }
        let _ = self.conn.flush();
        // In-flight pastes are deliberately left alone. An application that
        // asked for a flavor a moment before the clipboard changed still asked
        // for it, and each write is bounded by its own timeout anyway.
        self.emit(WatchEvent::SelectionLost);
    }
}

impl WatchState {
    /// Open one pipe per interesting flavor and drive them all from the loop.
    ///
    /// `wanted` may be empty, in which case the selection completes immediately
    /// and is emitted carrying only `advertised`.
    fn start_reads(
        &mut self,
        kind: SelectionKind,
        offer: Offer,
        advertised: Vec<String>,
        wanted: Vec<String>,
    ) {
        self.generation += 1;
        let generation = self.generation;
        let cap = self.config.max_flavor_bytes;

        let mut selection = PendingSelection::new(generation, kind, advertised);
        let mut read_tokens = HashMap::with_capacity(wanted.len());

        for mime in wanted {
            let slot = selection.expect_flavor(mime.clone(), cap);
            match self.begin_read(&offer, &mime, generation, slot) {
                Ok(token) => {
                    read_tokens.insert(slot, token);
                }
                Err(reason) => {
                    warn!(
                        mime = %for_log(&mime),
                        %reason,
                        "could not start a flavor read"
                    );
                    selection.drop_flavor(slot, DropReason::Setup(reason));
                }
            }
        }

        // Nothing left open means nothing to time out; arming the timer anyway
        // would only make the emit below immediately tear it down again.
        let timeout_token = if selection.is_complete() {
            None
        } else {
            self.handle
                .insert_source(
                    Timer::from_duration(self.config.flavor_read_timeout),
                    move |_instant, _, state| {
                        state.on_read_timeout(generation);
                        TimeoutAction::Drop
                    },
                )
                .map_err(|error| {
                    warn!(error = %error.error, "could not arm the flavor read timeout");
                })
                .ok()
        };

        trace!(
            ?kind,
            generation,
            reads = read_tokens.len(),
            "reading a selection"
        );
        self.pending = Some(Pending {
            selection,
            offer,
            read_tokens,
            timeout_token,
        });
        self.emit_if_complete();
    }

    /// Hand the source a pipe and register the read end with the loop.
    fn begin_read(
        &mut self,
        offer: &Offer,
        mime: &str,
        generation: u64,
        slot: usize,
    ) -> Result<RegistrationToken, String> {
        let (read_fd, write_fd) =
            rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).map_err(|e| e.to_string())?;

        // Only our end is non-blocking. O_NONBLOCK is per open file description
        // and the two ends of a pipe have their own, so the source still gets a
        // plain blocking write and never has to cope with EAGAIN.
        let flags = rustix::fs::fcntl_getfl(&read_fd).map_err(|e| e.to_string())?;
        rustix::fs::fcntl_setfl(&read_fd, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(|e| e.to_string())?;

        offer.receive(mime, write_fd.as_fd());
        // The request is buffered until flushed, and the fd goes out with it.
        // Dropping the write end first would send a closed fd; not dropping it
        // at all would mean never seeing EOF, because we would be holding the
        // pipe open ourselves.
        self.conn.flush().map_err(|e| e.to_string())?;
        drop(write_fd);

        let source = Generic::new(read_fd, Interest::READ, Mode::Level);
        self.handle
            .insert_source(source, move |_readiness, fd, state| {
                Ok(state.on_flavor_readable(generation, slot, fd))
            })
            .map_err(|error| error.error.to_string())
    }

    /// A flavor's pipe has data, has hit EOF, or has broken.
    fn on_flavor_readable(
        &mut self,
        generation: u64,
        slot: usize,
        fd: &NoIoDrop<OwnedFd>,
    ) -> PostAction {
        let action = match self.pending.as_mut() {
            Some(pending) if pending.selection.generation() == generation => {
                let action = read_flavor(&mut pending.selection, slot, fd);
                if action == PostAction::Remove {
                    // We are removing ourselves; nobody else should try to.
                    pending.read_tokens.remove(&slot);
                }
                action
            }
            // A selection we have already given up on.
            _ => PostAction::Remove,
        };
        if action == PostAction::Remove {
            self.emit_if_complete();
        }
        action
    }

    /// A source took the pipe and then went quiet.
    fn on_read_timeout(&mut self, generation: u64) {
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        if pending.selection.generation() != generation {
            self.pending = Some(pending);
            return;
        }
        // The timer removes itself by returning `TimeoutAction::Drop`.
        pending.timeout_token = None;
        let stalled = pending.selection.abandon_open(DropReason::Stalled);
        if stalled > 0 {
            warn!(
                stalled,
                timeout = ?self.config.flavor_read_timeout,
                "giving up on flavors whose source never closed the pipe"
            );
        }
        for (_slot, token) in pending.read_tokens.drain() {
            self.handle.remove(token);
        }
        self.pending = Some(pending);
        self.emit_if_complete();
    }

    /// Publish the pending selection once, and only once every flavor is in.
    fn emit_if_complete(&mut self) {
        if !self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.selection.is_complete())
        {
            return;
        }
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        self.clear_sources(&mut pending);
        pending.offer.destroy();

        let selection = pending.selection.into_selection();
        for dropped in &selection.dropped {
            warn!(
                mime = %for_log(&dropped.mime),
                reason = %dropped.reason,
                "dropped a flavor"
            );
        }
        trace!(
            kind = ?selection.kind,
            flavors = selection.flavors.len(),
            dropped = selection.dropped.len(),
            "captured a selection"
        );

        self.emit(WatchEvent::Captured(selection));
    }

    /// Hand one event to the daemon, or say why it went nowhere.
    fn emit(&mut self, event: WatchEvent) {
        match self.tx.try_send(event) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!("dropping a clipboard event: the daemon is not draining the channel")
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                info!("the event receiver is gone, stopping the watcher");
                self.signal.stop();
                self.signal.wakeup();
            }
        }
    }

    /// Give up on a half-read selection.
    fn abandon_pending(&mut self, why: &str) {
        let Some(mut pending) = self.pending.take() else {
            return;
        };
        debug!(why, "abandoning a selection that was still being read");
        self.clear_sources(&mut pending);
        pending.offer.destroy();
    }

    /// Unregister whatever a pending selection still has in the loop.
    ///
    /// Reads that already resolved took their own tokens out, so what is left
    /// here is only sources that are not currently running a callback.
    fn clear_sources(&mut self, pending: &mut Pending) {
        for (_slot, token) in pending.read_tokens.drain() {
            self.handle.remove(token);
        }
        if let Some(token) = pending.timeout_token.take() {
            self.handle.remove(token);
        }
    }

    /// Destroy every tracked offer except `keep`, returning `keep`'s MIME types.
    ///
    /// The protocol requires the previous offer to be destroyed when a new one
    /// supersedes it, and an offer that never becomes a selection is ours to
    /// clean up too.
    fn retire_offers(&mut self, keep: Option<&ObjectId>) -> Vec<String> {
        let mut kept = Vec::new();
        for (id, state) in std::mem::take(&mut self.offers) {
            if Some(&id) == keep {
                kept = state.mimes.into_vec();
            } else {
                state.offer.destroy();
            }
        }
        kept
    }

    /// Do what the daemon asked.
    fn on_command(&mut self, command: Command) {
        match command {
            Command::Offer(flavors) => self.take_selection(flavors),
        }
    }

    /// Put an entry's flavors on the clipboard and keep them there.
    ///
    /// The order matters and is fixed by both protocols: every `offer` must
    /// come *before* `set_selection` — a later one is an `invalid_offer`
    /// protocol error, which kills the connection — and the source we are
    /// replacing is destroyed only *after* the new one has the selection, so
    /// there is no instant in which the clipboard is empty.
    fn take_selection(&mut self, flavors: Vec<Flavor>) {
        let offered = offer::offered_flavors(flavors);
        if offered.is_empty() {
            warn!("clippo was asked to put an entry with no usable flavor on the clipboard");
            return;
        }

        let flavors = offered.len();
        let source = self.manager.create_source(&self.qh);
        for flavor in &offered {
            source.offer(&flavor.mime);
        }
        self.device.set_selection(Some(&source));

        let superseded = self.offered.replace(OwnedOffer {
            source,
            flavors: offered,
        });
        if let Some(superseded) = superseded {
            superseded.source.destroy();
        }
        if let Err(error) = self.conn.flush() {
            // The request is still buffered, so the next flush — the next turn
            // of the loop — will carry it. Worth a line either way: a clipboard
            // that did not change is exactly what a user would report.
            warn!(%error, "could not flush the request that puts clippo's entry on the clipboard");
        }
        debug!(flavors, "clippo took the clipboard");
    }

    /// Start writing one flavor into a paste's pipe.
    ///
    /// The first push happens here rather than on the next turn of the loop:
    /// most pastes are a line of text and fit in the pipe whole, so the common
    /// case costs no registration at all.
    fn begin_write(&mut self, mime: String, blob: Arc<Vec<u8>>, fd: OwnedFd) {
        let mut writer = BlobWriter::new(blob);

        // Ours alone: the fd came from the compositor and nothing else holds
        // it, so making it non-blocking cannot surprise another reader.
        if let Err(error) = set_nonblocking(&fd) {
            warn!(%mime, %error, "dropping a paste clippo could not make non-blocking");
            return;
        }

        match writer.pump(fd.as_fd()) {
            WriteProgress::Done => {
                trace!(%mime, "answered a paste in one write");
                return;
            }
            WriteProgress::Failed(error) => {
                debug!(%mime, %error, "a paste ended before clippo could answer it");
                return;
            }
            WriteProgress::Blocked => {}
        }

        // The receiver is not keeping up. Park the rest on the loop, having
        // first made room: an unbounded pile of half-written pastes is an
        // unbounded pile of file descriptors.
        self.make_room_for_a_write();

        let id = self.next_write;
        self.next_write += 1;
        let source = Generic::new(fd, Interest::WRITE, Mode::Level);
        let token = match self
            .handle
            .insert_source(source, move |_readiness, fd, state| {
                Ok(state.on_write_ready(id, fd))
            }) {
            Ok(token) => token,
            Err(error) => {
                warn!(%mime, error = %error.error, "dropping a paste clippo could not register");
                return;
            }
        };

        let timeout = self
            .handle
            .insert_source(
                Timer::from_duration(self.config.paste_write_timeout),
                move |_instant, _, state| {
                    state.on_write_timeout(id);
                    TimeoutAction::Drop
                },
            )
            .map_err(|error| warn!(error = %error.error, "could not arm a paste's timeout"))
            .ok();

        trace!(%mime, remaining = writer.remaining(), "a paste is being read slowly");
        self.writes.insert(
            id,
            PendingWrite {
                mime,
                writer,
                token,
                timeout,
            },
        );
        self.write_order.push_back(id);
    }

    /// A paste's pipe has room again, or its receiver has gone.
    fn on_write_ready(&mut self, id: u64, fd: &NoIoDrop<OwnedFd>) -> PostAction {
        // Taken out rather than borrowed: the resolved arms need `&mut self`
        // to unregister the timeout, and a write that is still going is put
        // straight back.
        let Some(mut write) = self.writes.remove(&id) else {
            return PostAction::Remove;
        };
        match write.writer.pump(fd.as_fd()) {
            WriteProgress::Blocked => {
                self.writes.insert(id, write);
                PostAction::Continue
            }
            WriteProgress::Done => {
                trace!(mime = %write.mime, "finished answering a slow paste");
                // We are removing ourselves by returning `Remove`; only the
                // timer, which is a different source, is ours to take out.
                self.forget_write(id, write, false);
                PostAction::Remove
            }
            WriteProgress::Failed(error) => {
                debug!(mime = %write.mime, %error, "gave up on a paste");
                self.forget_write(id, write, false);
                PostAction::Remove
            }
        }
    }

    /// A receiver took a flavor's pipe and then stopped reading it.
    fn on_write_timeout(&mut self, id: u64) {
        let Some(write) = self.writes.remove(&id) else {
            return;
        };
        warn!(
            mime = %write.mime,
            remaining = write.writer.remaining(),
            timeout = ?self.config.paste_write_timeout,
            "giving up on a paste the application stopped reading"
        );
        // The timer removes itself by returning `TimeoutAction::Drop`; the
        // write's own source is not running, so it is ours to remove.
        self.forget_write(id, write, true);
    }

    /// Unregister what a finished paste still has in the loop and close its fd.
    ///
    /// `remove_source` is false when the write's own callback is what is
    /// resolving it: calloop cannot unregister a source from inside that
    /// source's own callback, so the `PostAction::Remove` it returns has to be
    /// the thing that removes it — and that is also what closes the fd.
    fn forget_write(&mut self, id: u64, write: PendingWrite, remove_source: bool) {
        if remove_source {
            self.handle.remove(write.token);
        }
        if let Some(timeout) = write.timeout {
            self.handle.remove(timeout);
        }
        self.write_order.retain(|queued| *queued != id);
    }

    /// Make sure one more paste can be parked without the pile growing forever.
    ///
    /// Evicting the oldest rather than refusing the newest is deliberate: the
    /// oldest is the one that has already had the longest to read and has not,
    /// so it is the likeliest to be a receiver that never will.
    fn make_room_for_a_write(&mut self) {
        while self.writes.len() >= self.config.max_pending_pastes {
            let Some(oldest) = self.write_order.pop_front() else {
                // The queue and the map disagree, which cannot happen — but
                // looping forever on an empty queue is not the way to find out.
                debug_assert!(
                    self.writes.is_empty(),
                    "a parked paste with no place in the queue"
                );
                return;
            };
            let Some(write) = self.writes.remove(&oldest) else {
                continue;
            };
            warn!(
                mime = %write.mime,
                remaining = write.writer.remaining(),
                limit = self.config.max_pending_pastes,
                "dropping the oldest unfinished paste to make room for a new one"
            );
            self.forget_write(oldest, write, true);
        }
    }

    /// Give the clipboard up, so the compositor is not left pointing at a
    /// source whose process is going away.
    fn release_selection(&mut self) {
        let Some(offered) = self.offered.take() else {
            return;
        };
        self.device.set_selection(None);
        offered.source.destroy();
        debug!("clippo gave the clipboard up");
    }

    fn shutdown(&mut self) {
        self.abandon_pending("the watcher is shutting down");
        self.retire_offers(None);
        // Half-written pastes are not unregistered one by one: the loop is
        // about to be dropped, which closes every pipe still in it, and that
        // closing is what tells each receiver it is not getting the rest.
        self.release_selection();
        self.device.destroy();
        let _ = self.conn.flush();
    }
}

/// Make our end of a pipe non-blocking, so a receiver that stops reading costs
/// us a registration instead of the whole event loop.
fn set_nonblocking(fd: &OwnedFd) -> Result<(), Errno> {
    let flags = rustix::fs::fcntl_getfl(fd)?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)
}

/// Drain a flavor's pipe without blocking.
///
/// Returns [`PostAction::Continue`] when the pipe is merely empty for now, and
/// [`PostAction::Remove`] once the flavor is resolved — by EOF, by an error, or
/// by blowing its cap.
///
/// Neither of the two ways a source can misbehave holds the loop up. One that
/// stops writing without closing never produces the EOF, but the pipe stops
/// being readable, so it costs one registered fd and no loop time. One that
/// writes without end is bounded by the cap: this drains at most
/// `max_flavor_bytes / READ_CHUNK` chunks before the cap fires and the flavor
/// is dropped.
fn read_flavor(
    selection: &mut PendingSelection,
    slot: usize,
    fd: &NoIoDrop<OwnedFd>,
) -> PostAction {
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        match rustix::io::read(fd.as_fd(), &mut chunk) {
            Ok(0) => {
                selection.finish(slot);
                return PostAction::Remove;
            }
            Ok(bytes) => match selection.push(slot, &chunk[..bytes]) {
                Push::Accepted => {}
                // Over the cap, or the slot was already resolved: either way
                // there is no reason to keep reading this pipe.
                Push::OverCap | Push::Closed => return PostAction::Remove,
            },
            Err(Errno::INTR) => {}
            // `Errno::WOULDBLOCK` is the same value as `AGAIN` on Linux.
            Err(Errno::AGAIN) => return PostAction::Continue,
            Err(error) => {
                selection.drop_flavor(slot, DropReason::Io(error.to_string()));
                return PostAction::Remove;
            }
        }
    }
}

/// The advertised flavors clippo wants, in the order the source offered them,
/// with repeats removed.
///
/// "Repeat" is [`mime::same`]'s question rather than string equality's, and the
/// difference is the whole bound on this function. [`mime::is_interesting`]
/// normalises before it compares, so `text/plain`, `"text/plain "` and
/// `"te xt/plain"` are all interesting; whitespace may be inserted anywhere any
/// number of times, so an exact-match dedup let one source turn a single flavor
/// into as many entries as it cared to advertise — and every entry costs a pipe,
/// a loop registration and a `FlavorBuffer` with the *full* per-flavor cap.
/// `same` is the comparison the paste path already asks ([`offer::blob_for`]),
/// and asking it here collapses those spellings to one entry.
///
/// That also takes the inner scan with it: `wanted` is bounded, so this is
/// linear in what was advertised rather than quadratic in it.
fn interesting_flavors(advertised: &[String]) -> Vec<String> {
    let mut wanted: Vec<String> = Vec::new();
    let mut refused = 0_usize;
    for mime in advertised {
        if !mime::is_interesting(mime) || wanted.iter().any(|seen| mime::same(seen, mime)) {
            continue;
        }
        // Belt and braces. With `same` above this is unreachable — an
        // interesting type normalises to one of `INTERESTING_MIMES`, so a
        // full `wanted` already holds a match for it — but that bound is a
        // property of a list someone may extend, and this one is arithmetic.
        if wanted.len() == mime::INTERESTING_MIMES.len() {
            refused += 1;
            continue;
        }
        wanted.push(mime.clone());
    }
    // One line per selection, not one per flavor: the whole point of the
    // ceiling is that a source advertising thousands of them cannot turn the
    // journal into the resource it exhausts instead.
    if refused > 0 {
        warn!(
            refused,
            kept = wanted.len(),
            "a selection advertised more distinct interesting flavors than clippo reads"
        );
    }
    wanted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_interesting_flavors_are_selected_for_reading() {
        let advertised = [
            "TIMESTAMP",
            "text/html",
            "text/plain;charset=utf-8",
            "text/plain",
            "SAVE_TARGETS",
            "application/x-qt-image",
        ]
        .map(String::from);
        assert_eq!(
            interesting_flavors(&advertised),
            ["text/html", "text/plain;charset=utf-8", "text/plain"]
        );
    }

    #[test]
    fn repeated_flavors_are_read_once() {
        let advertised = ["text/plain", "text/plain", "image/png"].map(String::from);
        assert_eq!(
            interesting_flavors(&advertised),
            ["text/plain", "image/png"]
        );
    }

    #[test]
    fn an_offer_with_nothing_useful_yields_no_reads() {
        let advertised = ["TIMESTAMP", "MULTIPLE"].map(String::from);
        assert!(interesting_flavors(&advertised).is_empty());
    }

    /// Whitespace-permuted spellings of one type, all distinct as strings and
    /// all accepted by `is_interesting`.
    fn permutations(base: &str, count: usize) -> Vec<String> {
        let positions = base.len() + 1;
        (0..count)
            .map(|n| {
                let (head, tail) = base.split_at(n % positions);
                let spaces = " ".repeat(n / positions + 1);
                format!("{head}{spaces}{tail}")
            })
            .collect()
    }

    /// Walk what `start_reads` walks — one real pipe and one capped
    /// [`FlavorBuffer`] per wanted flavor — and report how many pipes that took.
    ///
    /// The finding is about the number of pipes, loop registrations and buffers
    /// one selection can force, so the count that matters is this one and not
    /// `wanted.len()` on its own. There is no compositor here, so the source
    /// side of each pipe is written by the test; everything after `expect_flavor`
    /// is the code `read_flavor` runs.
    fn read_a_selection(advertised: &[String]) -> (usize, crate::Selection) {
        let wanted = interesting_flavors(advertised);
        let mut selection = PendingSelection::new(1, SelectionKind::Clipboard, advertised.to_vec());
        let mut pipes: Vec<OwnedFd> = Vec::new();

        for mime in &wanted {
            let (read_fd, write_fd) = rustix::pipe::pipe().expect("a pipe");
            let slot = selection.expect_flavor(mime.clone(), 1024);
            rustix::io::write(write_fd.as_fd(), b"payload").expect("a write");
            drop(write_fd);

            let mut chunk = [0u8; 64];
            loop {
                match rustix::io::read(read_fd.as_fd(), &mut chunk) {
                    Ok(0) => {
                        selection.finish(slot);
                        break;
                    }
                    Ok(bytes) => {
                        selection.push(slot, &chunk[..bytes]);
                    }
                    Err(Errno::INTR) => {}
                    Err(error) => panic!("reading a flavor: {error}"),
                }
            }
            // Held open to the end, so `pipes.len()` is what the descriptor
            // table saw at once rather than a count of reuses.
            pipes.push(read_fd);
        }

        (pipes.len(), selection.into_selection())
    }

    #[test]
    fn the_spellings_of_one_flavor_collapse_to_one_read() {
        let advertised = [
            "text/plain",
            "text/plain ",
            " text/plain",
            "te xt/plain",
            "TEXT/plain",
            "text/pl a in",
        ]
        .map(String::from);
        // `is_interesting` accepts all six; the dedup has to agree with it.
        assert!(advertised.iter().all(|mime| mime::is_interesting(mime)));
        assert_eq!(interesting_flavors(&advertised), ["text/plain"]);
    }

    #[test]
    fn thousands_of_permutations_open_no_more_pipes_than_there_are_flavors() {
        let mut advertised = permutations("text/plain", 3000);
        advertised.extend(permutations("image/png", 3000));
        advertised.extend(permutations("TIMESTAMP", 3000));
        assert_eq!(advertised.len(), 9000);

        let wanted = interesting_flavors(&advertised);
        assert!(
            wanted.len() <= mime::INTERESTING_MIMES.len(),
            "wanted {wanted:?}"
        );

        // The assertion the finding is actually about: 9 000 advertised types
        // buy the source two pipes, two registrations and two capped buffers.
        let (pipes, selection) = read_a_selection(&advertised);
        assert_eq!(pipes, 2);
        assert_eq!(selection.flavors.len(), 2);
        assert!(selection.dropped.is_empty());
    }

    #[test]
    fn an_offer_cannot_advertise_past_the_ceiling() {
        let mut advertised = AdvertisedMimes::default();
        for n in 0..MAX_ADVERTISED_MIMES {
            advertised.push(format!("application/x-private-{n}"));
        }
        assert!(!advertised.overflowed);

        for n in MAX_ADVERTISED_MIMES..MAX_ADVERTISED_MIMES + 5000 {
            advertised.push(format!("application/x-private-{n}"));
        }
        assert!(advertised.overflowed, "the ceiling should have been hit");

        let kept = advertised.into_vec();
        assert_eq!(kept.len(), MAX_ADVERTISED_MIMES);
        assert_eq!(kept[0], "application/x-private-0");
        assert_eq!(
            kept[MAX_ADVERTISED_MIMES - 1],
            format!("application/x-private-{}", MAX_ADVERTISED_MIMES - 1)
        );
    }

    /// The ceiling bounds the diagnostic list; it must not decide what clippo
    /// captures. A flat truncation would, and silently: the copy is simply not
    /// there, and the only trace is a `warn!` about advertised types.
    #[test]
    fn the_ceiling_never_costs_a_flavor_clippo_would_have_read() {
        let mut advertised = AdvertisedMimes::default();
        for n in 0..MAX_ADVERTISED_MIMES {
            advertised.push(format!("application/x-private-{n}"));
        }
        // Every interesting type arrives only after the ceiling is full, in a
        // spelling `is_interesting` accepts but an exact match would not.
        for known in mime::INTERESTING_MIMES {
            advertised.push(format!(" {} ", known.to_ascii_uppercase()));
        }
        let kept = advertised.into_vec();

        assert_eq!(
            kept.len(),
            MAX_ADVERTISED_MIMES + mime::INTERESTING_MIMES.len()
        );
        // Rule 1 of secret detection reads this list, not the flavors.
        assert!(
            kept.iter()
                .any(|mime| mime::same(mime, clippo_core::secrets::PASSWORD_MANAGER_HINT_MIME)),
            "the password-manager marker was truncated away"
        );
        // Composed the way the watcher composes it: what `offer_mime` kept is
        // what `interesting_flavors` selects from.
        let wanted = interesting_flavors(&kept);
        assert_eq!(wanted.len(), mime::INTERESTING_MIMES.len(), "{wanted:?}");
    }

    /// The rescue above cannot itself become the unbounded quantity: it is one
    /// entry per *distinct* interesting type, by the same `same` argument
    /// `interesting_flavors` makes.
    #[test]
    fn interesting_types_past_the_ceiling_are_bounded_too() {
        let mut advertised = AdvertisedMimes::default();
        for n in 0..MAX_ADVERTISED_MIMES {
            advertised.push(format!("application/x-private-{n}"));
        }
        for spelling in permutations("text/plain", 5000) {
            advertised.push(spelling);
        }
        assert_eq!(advertised.into_vec().len(), MAX_ADVERTISED_MIMES + 1);
    }

    #[test]
    fn a_mime_type_reaches_the_journal_escaped() {
        let hostile = "text/plain\u{1b}[2J\u{202e}nialp/txet\u{200b}";
        let logged = for_log(hostile);
        assert!(
            !logged
                .chars()
                .any(|c| c.is_control() || clippo_core::display::is_invisible_or_reordering(c)),
            "{logged}"
        );
        assert!(logged.contains("\\u{1b}"), "{logged}");
        assert!(logged.contains("\\u{202e}"), "{logged}");
        assert!(logged.contains("\\u{200b}"), "{logged}");
        // Bounded too: the length is the client's to choose, the journal's is
        // not.
        assert!(for_log(&"x".repeat(10_000)).chars().count() <= MIME_LOG_CHARS);
    }
}
