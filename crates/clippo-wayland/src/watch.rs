//! The watcher thread: one Wayland connection, one calloop event loop, and a
//! `tokio::sync::mpsc` channel out to the daemon.

use std::collections::HashMap;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc as sync_mpsc;
use std::thread::JoinHandle;

use calloop::generic::{Generic, NoIoDrop};
use calloop::timer::{TimeoutAction, Timer};
use calloop::{EventLoop, Interest, LoopHandle, LoopSignal, Mode, PostAction, RegistrationToken};
use calloop_wayland_source::WaylandSource;
use rustix::io::Errno;
use tokio::sync::mpsc;
use tracing::{debug, error, info, trace, warn};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, EventQueue};

use crate::flavor::{PendingSelection, Push};
use crate::protocol::{DataControlSink, Device, Manager, Offer};
use crate::{mime, DropReason, Error, Selection, SelectionKind, WatchConfig};

/// How much we take off a pipe per `read`. Bounded so that a flavor cannot
/// overshoot its cap by more than one chunk before we notice.
const READ_CHUNK: usize = 64 * 1024;

/// A running watcher thread.
///
/// Dropping this does *not* stop the thread — call [`Watcher::stop`], or drop
/// the [`Selection`] receiver, which the watcher takes as its cue to shut down.
#[derive(Debug)]
pub struct Watcher {
    signal: LoopSignal,
    thread: JoinHandle<()>,
    protocol: &'static str,
}

impl Watcher {
    /// Which of the two data-control protocols this watcher bound.
    pub fn protocol(&self) -> &'static str {
        self.protocol
    }

    /// Ask the watcher to shut down. Returns once it has.
    pub fn stop(self) {
        self.signal.stop();
        self.signal.wakeup();
        if self.thread.join().is_err() {
            error!("the wayland watcher thread panicked");
        }
    }
}

/// Start watching the clipboard.
///
/// Connects, binds a data-control manager, and hands back a receiver that
/// yields one message per captured selection, carrying every interesting
/// flavor of that selection together.
///
/// Connection and binding happen on the watcher thread but are waited for here,
/// so a missing data-control protocol is reported to the caller rather than
/// logged and forgotten.
pub fn watch(config: WatchConfig) -> Result<(Watcher, mpsc::Receiver<Selection>), Error> {
    let (tx, rx) = mpsc::channel(config.channel_capacity.max(1));
    let (ready_tx, ready_rx) = sync_mpsc::sync_channel::<Result<Started, Error>>(1);

    let thread = std::thread::Builder::new()
        .name("clippo-wayland".to_owned())
        .spawn(move || run(config, tx, &ready_tx))
        .map_err(Error::SpawnThread)?;

    match ready_rx.recv() {
        Ok(Ok(started)) => Ok((
            Watcher {
                signal: started.signal,
                thread,
                protocol: started.protocol,
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
    tx: mpsc::Sender<Selection>,
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
    if let Err(error) = WaylandSource::new(conn.clone(), bound.queue).insert(handle.clone()) {
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
        handle,
        signal: signal.clone(),
        config,
        tx,
        offers: HashMap::new(),
        pending: None,
        generation: 0,
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
    handle: LoopHandle<'static, WatchState>,
    signal: LoopSignal,
    config: WatchConfig,
    tx: mpsc::Sender<Selection>,
    /// Offers introduced by `data_offer` but not yet claimed by a `selection`.
    offers: HashMap<ObjectId, OfferState>,
    /// The selection currently being read, if any.
    pending: Option<Pending>,
    generation: u64,
    manager: Manager,
    device: Device,
    _seat: WlSeat,
}

struct OfferState {
    offer: Offer,
    mimes: Vec<String>,
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
                mimes: Vec::new(),
            },
        );
    }

    fn offer_mime(&mut self, offer: &ObjectId, mime: String) {
        match self.offers.get_mut(offer) {
            Some(state) => state.mimes.push(mime),
            None => trace!(?offer, %mime, "mime advertised for an offer we do not track"),
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
                    warn!(%mime, %reason, "could not start a flavor read");
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
            warn!(mime = %dropped.mime, reason = %dropped.reason, "dropped a flavor");
        }
        trace!(
            kind = ?selection.kind,
            flavors = selection.flavors.len(),
            dropped = selection.dropped.len(),
            "captured a selection"
        );

        match self.tx.try_send(selection) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!("dropping a captured selection: the daemon is not draining the channel")
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                info!("selection receiver is gone, stopping the watcher");
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
                kept = state.mimes;
            } else {
                state.offer.destroy();
            }
        }
        kept
    }

    fn shutdown(&mut self) {
        self.abandon_pending("the watcher is shutting down");
        self.retire_offers(None);
        self.device.destroy();
        let _ = self.conn.flush();
    }
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
fn interesting_flavors(advertised: &[String]) -> Vec<String> {
    let mut wanted: Vec<String> = Vec::new();
    for mime in advertised {
        if mime::is_interesting(mime) && !wanted.iter().any(|seen| seen == mime) {
            wanted.push(mime.clone());
        }
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
}
