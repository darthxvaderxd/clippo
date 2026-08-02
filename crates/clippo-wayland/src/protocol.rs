//! The one module that knows there are two data-control protocols.
//!
//! `ext_data_control_v1` and `zwlr_data_control_v1` are the same protocol twice
//! over, and they will drift apart as the staging version stabilises. Every
//! protocol-specific type is named here and nowhere else; the rest of the crate
//! sees [`Manager`], [`Device`] and [`Offer`], and receives events through the
//! [`DataControlSink`] trait.

use std::os::fd::BorrowedFd;

use wayland_client::backend::ObjectId;
use wayland_client::globals::{BindError, GlobalList};
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{
    delegate_noop, event_created_child, Connection, Dispatch, Proxy, QueueHandle,
};

use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
};

use crate::watch::WatchState;
use crate::{Error, SelectionKind};

/// The interface names, spelled once, for logs and for the failure message.
pub const EXT_PROTOCOL: &str = "ext_data_control_v1";
/// See [`EXT_PROTOCOL`].
pub const WLR_PROTOCOL: &str = "zwlr_data_control_v1";

/// `ext_data_control_v1` has exactly one version so far.
const EXT_VERSION: u32 = 1;
/// The `zwlr` version everything we need for the clipboard proper lives in.
const WLR_BASE_VERSION: u32 = 1;
/// `zwlr_data_control_v1` gained primary-selection support in version 2.
const WLR_PRIMARY_VERSION: u32 = 2;

/// Where a selection came from.
///
/// Kept protocol-agnostic so callers never have to care which of the two
/// interfaces delivered the event.
pub(crate) use crate::SelectionKind as Kind;

/// Events from the compositor, translated out of protocol terms.
///
/// [`WatchState`] implements this; the `Dispatch` glue below is the only thing
/// that calls it.
pub(crate) trait DataControlSink {
    /// A new offer object exists; its MIME types follow.
    fn offer_created(&mut self, offer: Offer);
    /// The offer advertises one more MIME type.
    fn offer_mime(&mut self, offer: &ObjectId, mime: String);
    /// A selection changed. `None` means the selection was cleared.
    fn selection_changed(&mut self, kind: SelectionKind, offer: Option<Offer>);
    /// The device is inert and must be torn down.
    fn device_finished(&mut self);
}

/// A bound data-control manager, whichever protocol provided it.
#[derive(Debug)]
pub(crate) enum Manager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

impl Manager {
    /// Bind a manager, preferring `ext` and falling back to `zwlr`.
    ///
    /// `primary` decides which `zwlr` version we ask for: without it we bind
    /// version 1, which has no `primary_selection` event at all, so the
    /// compositor never offers us one. `ext_data_control_v1` has no such
    /// version split — see [`Manager::can_suppress_primary`].
    pub(crate) fn bind(
        globals: &GlobalList,
        qh: &QueueHandle<WatchState>,
        primary: bool,
    ) -> Result<Self, Error> {
        match globals.bind::<ExtDataControlManagerV1, _, _>(qh, EXT_VERSION..=EXT_VERSION, ()) {
            Ok(manager) => {
                tracing::info!(protocol = EXT_PROTOCOL, "bound data-control manager");
                return Ok(Self::Ext(manager));
            }
            Err(BindError::NotPresent) => {
                tracing::debug!("{EXT_PROTOCOL} not advertised, falling back to {WLR_PROTOCOL}");
            }
            Err(BindError::UnsupportedVersion) => {
                tracing::warn!(
                    "{EXT_PROTOCOL} is advertised at an unsupported version, \
                     falling back to {WLR_PROTOCOL}"
                );
            }
        }

        let wanted = if primary {
            WLR_PRIMARY_VERSION
        } else {
            WLR_BASE_VERSION
        };
        match globals.bind::<ZwlrDataControlManagerV1, _, _>(qh, WLR_BASE_VERSION..=wanted, ()) {
            Ok(manager) => {
                tracing::info!(
                    protocol = WLR_PROTOCOL,
                    version = manager.version(),
                    "bound data-control manager"
                );
                Ok(Self::Wlr(manager))
            }
            Err(_) => Err(Error::no_data_control_manager()),
        }
    }

    /// Create the seat's device. This is what starts the flow of events.
    pub(crate) fn device(&self, seat: &WlSeat, qh: &QueueHandle<WatchState>) -> Device {
        match self {
            Self::Ext(manager) => Device::Ext(manager.get_data_device(seat, qh, ())),
            Self::Wlr(manager) => Device::Wlr(manager.get_data_device(seat, qh, ())),
        }
    }

    /// The interface name, for logging.
    pub(crate) fn protocol(&self) -> &'static str {
        match self {
            Self::Ext(_) => EXT_PROTOCOL,
            Self::Wlr(_) => WLR_PROTOCOL,
        }
    }

    /// Whether the bound protocol lets us decline primary selection outright.
    ///
    /// `zwlr` version 1 does: no `primary_selection` event exists, so with
    /// primary disabled the compositor never creates a primary offer for us and
    /// there is nothing to ignore. `ext_data_control_v1` folds primary into its
    /// only version, so there the event still arrives and [`WatchState`] drops
    /// the offer on receipt without ever opening a pipe for it.
    pub(crate) fn can_suppress_primary(&self) -> bool {
        match self {
            Self::Ext(_) => false,
            Self::Wlr(manager) => manager.version() < WLR_PRIMARY_VERSION,
        }
    }

    /// Whether the compositor will actually deliver primary selections.
    pub(crate) fn supports_primary(&self) -> bool {
        match self {
            Self::Ext(_) => true,
            Self::Wlr(manager) => manager.version() >= WLR_PRIMARY_VERSION,
        }
    }
}

/// The seat's data-control device.
#[derive(Debug)]
pub(crate) enum Device {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

impl Device {
    pub(crate) fn destroy(&self) {
        match self {
            Self::Ext(device) => device.destroy(),
            Self::Wlr(device) => device.destroy(),
        }
    }
}

/// An offer the compositor handed us, holding one selection's flavors.
#[derive(Debug, Clone)]
pub(crate) enum Offer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl Offer {
    /// Stable identity, used to match `offer` events to the right offer.
    pub(crate) fn id(&self) -> ObjectId {
        match self {
            Self::Ext(offer) => offer.id(),
            Self::Wlr(offer) => offer.id(),
        }
    }

    /// Ask the source to write this flavor into `fd`.
    ///
    /// The request is only queued; the caller must flush the connection before
    /// dropping its copy of the write end, or the source never sees the fd.
    pub(crate) fn receive(&self, mime: &str, fd: BorrowedFd<'_>) {
        match self {
            Self::Ext(offer) => offer.receive(mime.to_owned(), fd),
            Self::Wlr(offer) => offer.receive(mime.to_owned(), fd),
        }
    }

    pub(crate) fn destroy(&self) {
        match self {
            Self::Ext(offer) => offer.destroy(),
            Self::Wlr(offer) => offer.destroy(),
        }
    }
}

// The registry is only used for the initial bind; dynamic global changes are of
// no interest to a clipboard manager.
impl
    Dispatch<
        wayland_client::protocol::wl_registry::WlRegistry,
        wayland_client::globals::GlobalListContents,
    > for WatchState
{
    fn event(
        _state: &mut Self,
        _proxy: &wayland_client::protocol::wl_registry::WlRegistry,
        _event: wayland_client::protocol::wl_registry::Event,
        _data: &wayland_client::globals::GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(WatchState: ignore WlSeat);
delegate_noop!(WatchState: ExtDataControlManagerV1);
delegate_noop!(WatchState: ZwlrDataControlManagerV1);

impl Dispatch<ExtDataControlDeviceV1, ()> for WatchState {
    fn event(
        state: &mut Self,
        _device: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            ext_data_control_device_v1::Event::DataOffer { id } => {
                state.offer_created(Offer::Ext(id));
            }
            ext_data_control_device_v1::Event::Selection { id } => {
                state.selection_changed(Kind::Clipboard, id.map(Offer::Ext));
            }
            ext_data_control_device_v1::Event::PrimarySelection { id } => {
                state.selection_changed(Kind::Primary, id.map(Offer::Ext));
            }
            ext_data_control_device_v1::Event::Finished => state.device_finished(),
            _ => {}
        }
    }

    event_created_child!(WatchState, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for WatchState {
    fn event(
        state: &mut Self,
        _device: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id } => {
                state.offer_created(Offer::Wlr(id));
            }
            zwlr_data_control_device_v1::Event::Selection { id } => {
                state.selection_changed(Kind::Clipboard, id.map(Offer::Wlr));
            }
            zwlr_data_control_device_v1::Event::PrimarySelection { id } => {
                state.selection_changed(Kind::Primary, id.map(Offer::Wlr));
            }
            zwlr_data_control_device_v1::Event::Finished => state.device_finished(),
            _ => {}
        }
    }

    event_created_child!(WatchState, ZwlrDataControlDeviceV1, [
        zwlr_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for WatchState {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offer_mime(&offer.id(), mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for WatchState {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            state.offer_mime(&offer.id(), mime_type);
        }
    }
}
