//! Size-capped flavor buffers and the bookkeeping that turns several
//! independent pipe reads back into one atomic selection.
//!
//! Nothing here touches Wayland or file descriptors, which is what makes the
//! interesting half of the capture path testable without a compositor.

use crate::{DropReason, DroppedFlavor, Flavor, Selection, SelectionKind};

/// What the caller should do after handing bytes to a flavor buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Push {
    /// Bytes accepted — keep reading this flavor.
    Accepted,
    /// The flavor blew its size cap and has been dropped — stop reading it.
    OverCap,
    /// The slot was already resolved; the bytes went nowhere.
    Closed,
}

/// One flavor being read, with a hard ceiling on how much it may accumulate.
///
/// Once the cap is exceeded the buffer releases what it has collected. Holding
/// on to a partial blob would be pointless — a truncated PNG or a half a URI
/// list is not something clippo can paste back — and a misbehaving source is
/// exactly the case where we least want to keep the memory.
#[derive(Debug)]
pub(crate) struct FlavorBuffer {
    mime: String,
    cap: usize,
    data: Vec<u8>,
    over_cap: bool,
}

impl FlavorBuffer {
    pub(crate) fn new(mime: String, cap: usize) -> Self {
        Self {
            mime,
            cap,
            data: Vec::new(),
            over_cap: false,
        }
    }

    pub(crate) fn mime(&self) -> &str {
        &self.mime
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.data.len()
    }

    #[cfg(test)]
    pub(crate) fn is_over_cap(&self) -> bool {
        self.over_cap
    }

    /// Append a chunk, refusing to grow past the cap.
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Push {
        if self.over_cap {
            return Push::OverCap;
        }
        if self.data.len().saturating_add(chunk.len()) > self.cap {
            self.over_cap = true;
            self.data = Vec::new();
            return Push::OverCap;
        }
        self.data.extend_from_slice(chunk);
        Push::Accepted
    }

    /// Consume the buffer, yielding a flavor unless it blew its cap.
    fn finish(self) -> Result<Flavor, (String, DropReason)> {
        if self.over_cap {
            Err((self.mime, DropReason::OverCap { cap: self.cap }))
        } else {
            Ok(Flavor {
                mime: self.mime,
                data: self.data,
            })
        }
    }
}

#[derive(Debug)]
enum Slot {
    Reading(FlavorBuffer),
    Done(Flavor),
    Dropped { mime: String, reason: DropReason },
}

/// All flavors of a single copy, collected together.
///
/// A selection is emitted as one unit or not at all: every flavor gets its own
/// pipe and its own reader, and only once the last of them has resolved — by
/// EOF, by error, by cap, or by timing out — does the selection leave this
/// struct. That is what keeps "half a selection" off the channel.
#[derive(Debug)]
pub(crate) struct PendingSelection {
    generation: u64,
    kind: SelectionKind,
    /// Every MIME type the offer advertised, kept so the finished selection can
    /// report what was on offer and not just what came back.
    advertised: Vec<String>,
    slots: Vec<Slot>,
    open: usize,
}

impl PendingSelection {
    pub(crate) fn new(generation: u64, kind: SelectionKind, advertised: Vec<String>) -> Self {
        Self {
            generation,
            kind,
            advertised,
            slots: Vec::new(),
            open: 0,
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    /// Reserve a slot for a flavor whose read is about to start.
    pub(crate) fn expect_flavor(&mut self, mime: String, cap: usize) -> usize {
        self.slots.push(Slot::Reading(FlavorBuffer::new(mime, cap)));
        self.open += 1;
        self.slots.len() - 1
    }

    /// Feed bytes read from a slot's pipe.
    ///
    /// Blowing the cap resolves the slot immediately: there is nothing to be
    /// gained by reading the rest of a blob we have already decided to discard.
    pub(crate) fn push(&mut self, slot: usize, chunk: &[u8]) -> Push {
        let Some(Slot::Reading(buffer)) = self.slots.get_mut(slot) else {
            return Push::Closed;
        };
        match buffer.push(chunk) {
            Push::Accepted => Push::Accepted,
            Push::OverCap | Push::Closed => {
                self.resolve(slot);
                Push::OverCap
            }
        }
    }

    /// The slot's pipe reached EOF — the flavor is complete.
    pub(crate) fn finish(&mut self, slot: usize) {
        self.resolve(slot);
    }

    /// The slot's read failed or was abandoned.
    pub(crate) fn drop_flavor(&mut self, slot: usize, reason: DropReason) {
        let Some(entry) = self.slots.get_mut(slot) else {
            return;
        };
        let Slot::Reading(buffer) = entry else {
            return;
        };
        let mime = buffer.mime().to_owned();
        *entry = Slot::Dropped { mime, reason };
        self.open -= 1;
    }

    /// Give up on every slot still being read, and report how many there were.
    ///
    /// This is the escape hatch for a source that accepts the pipe and then
    /// never writes to it or closes it.
    pub(crate) fn abandon_open(&mut self, reason: DropReason) -> usize {
        let open = self.open;
        for slot in 0..self.slots.len() {
            self.drop_flavor(slot, reason.clone());
        }
        open
    }

    /// Whether every flavor has resolved one way or another.
    pub(crate) fn is_complete(&self) -> bool {
        self.open == 0
    }

    /// Consume the bookkeeping into the selection it collected.
    ///
    /// A selection with nothing left is still produced. It carries what was
    /// advertised and why each flavor went missing, which is what makes a
    /// failed capture diagnosable instead of invisible; callers that only want
    /// storable content check [`Selection::is_empty`].
    pub(crate) fn into_selection(self) -> Selection {
        debug_assert!(self.is_complete(), "selection emitted while still reading");
        let mut flavors = Vec::new();
        let mut dropped = Vec::new();
        for slot in self.slots {
            match slot {
                Slot::Done(flavor) => flavors.push(flavor),
                Slot::Dropped { mime, reason } => dropped.push(DroppedFlavor { mime, reason }),
                // Unreachable once complete; discarding beats panicking here.
                Slot::Reading(_) => {}
            }
        }
        Selection {
            kind: self.kind,
            advertised: self.advertised,
            flavors,
            dropped,
        }
    }

    /// Turn a reading slot into its finished form, decrementing the open count.
    fn resolve(&mut self, slot: usize) {
        let Some(entry) = self.slots.get_mut(slot) else {
            return;
        };
        if !matches!(entry, Slot::Reading(_)) {
            return;
        }
        let placeholder = Slot::Dropped {
            mime: String::new(),
            reason: DropReason::Stalled,
        };
        let Slot::Reading(buffer) = std::mem::replace(entry, placeholder) else {
            unreachable!("matched Slot::Reading above")
        };
        *entry = match buffer.finish() {
            Ok(flavor) => Slot::Done(flavor),
            Err((mime, reason)) => Slot::Dropped { mime, reason },
        };
        self.open -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: usize = 16;

    fn pending() -> PendingSelection {
        PendingSelection::new(1, SelectionKind::Clipboard, Vec::new())
    }

    /// The drop list of a finished selection, as `(mime, reason)` pairs.
    fn drops(selection: &Selection) -> Vec<(&str, &DropReason)> {
        selection
            .dropped
            .iter()
            .map(|dropped| (dropped.mime.as_str(), &dropped.reason))
            .collect()
    }

    #[test]
    fn buffer_accumulates_chunks_up_to_the_cap() {
        let mut buffer = FlavorBuffer::new("text/plain".into(), CAP);
        assert_eq!(buffer.push(b"hello "), Push::Accepted);
        assert_eq!(buffer.push(b"world"), Push::Accepted);
        assert_eq!(buffer.len(), 11);
        assert!(!buffer.is_over_cap());
        assert_eq!(buffer.finish().unwrap().data, b"hello world");
    }

    #[test]
    fn buffer_accepts_exactly_the_cap() {
        let mut buffer = FlavorBuffer::new("text/plain".into(), CAP);
        assert_eq!(buffer.push(&[b'x'; CAP]), Push::Accepted);
        assert!(!buffer.is_over_cap());
        assert_eq!(buffer.finish().unwrap().data.len(), CAP);
    }

    #[test]
    fn buffer_drops_rather_than_truncates_when_the_cap_is_passed() {
        let mut buffer = FlavorBuffer::new("image/png".into(), CAP);
        assert_eq!(buffer.push(&[b'x'; CAP]), Push::Accepted);
        assert_eq!(buffer.push(b"!"), Push::OverCap);
        assert!(buffer.is_over_cap());
        // Not truncated to the cap — released entirely.
        assert_eq!(buffer.len(), 0);
        let (mime, reason) = buffer.finish().unwrap_err();
        assert_eq!(mime, "image/png");
        assert_eq!(reason, DropReason::OverCap { cap: CAP });
    }

    #[test]
    fn buffer_rejects_an_oversized_first_chunk_without_growing() {
        let mut buffer = FlavorBuffer::new("image/png".into(), CAP);
        assert_eq!(buffer.push(&[b'x'; CAP * 4]), Push::OverCap);
        assert_eq!(buffer.len(), 0);
        assert!(buffer.finish().is_err());
    }

    #[test]
    fn a_selection_is_incomplete_until_every_flavor_resolves() {
        let mut pending = pending();
        let text = pending.expect_flavor("text/plain".into(), CAP);
        let html = pending.expect_flavor("text/html".into(), CAP);
        assert!(!pending.is_complete());

        pending.push(text, b"hi");
        assert!(!pending.is_complete(), "bytes alone do not finish a flavor");
        pending.finish(text);
        assert!(!pending.is_complete(), "the other flavor is still open");

        pending.push(html, b"<p>hi</p>");
        pending.finish(html);
        assert!(pending.is_complete());
    }

    #[test]
    fn all_flavors_arrive_in_one_selection_in_offer_order() {
        let mut pending = PendingSelection::new(
            1,
            SelectionKind::Clipboard,
            ["text/plain;charset=utf-8", "text/plain", "TIMESTAMP"]
                .map(String::from)
                .to_vec(),
        );
        let utf8 = pending.expect_flavor("text/plain;charset=utf-8".into(), CAP);
        let plain = pending.expect_flavor("text/plain".into(), CAP);
        // Interleaved, as independent readers would deliver them.
        pending.push(utf8, b"hi");
        pending.push(plain, b"hi");
        pending.finish(plain);
        pending.finish(utf8);

        let selection = pending.into_selection();
        assert_eq!(selection.kind, SelectionKind::Clipboard);
        let mimes: Vec<&str> = selection.flavors.iter().map(|f| f.mime.as_str()).collect();
        assert_eq!(mimes, ["text/plain;charset=utf-8", "text/plain"]);
        assert_eq!(selection.flavor("text/plain").unwrap().data, b"hi");
        // What the source offered survives the capture, fetched or not.
        assert_eq!(selection.skipped(), ["TIMESTAMP"]);
    }

    #[test]
    fn an_over_cap_flavor_is_dropped_but_its_siblings_survive() {
        let mut pending = pending();
        let text = pending.expect_flavor("text/plain".into(), CAP);
        let image = pending.expect_flavor("image/png".into(), CAP);

        pending.push(text, b"caption");
        pending.finish(text);
        assert_eq!(pending.push(image, &[0u8; CAP + 1]), Push::OverCap);

        // Blowing the cap resolves the flavor on the spot.
        assert!(pending.is_complete());

        let selection = pending.into_selection();
        assert_eq!(selection.flavors.len(), 1);
        assert_eq!(selection.flavors[0].mime, "text/plain");
        assert_eq!(
            drops(&selection),
            vec![("image/png", &DropReason::OverCap { cap: CAP })]
        );
    }

    /// Nothing survived, but *why* nothing survived is the whole point of the
    /// message — `clippo-watch` has no other way to report it.
    #[test]
    fn a_selection_with_nothing_left_still_reports_what_it_lost() {
        let mut pending =
            PendingSelection::new(1, SelectionKind::Clipboard, vec!["image/png".to_owned()]);
        let image = pending.expect_flavor("image/png".into(), CAP);
        pending.drop_flavor(image, DropReason::Io("broken pipe".into()));
        assert!(pending.is_complete());

        let selection = pending.into_selection();
        assert!(selection.is_empty());
        assert_eq!(selection.advertised, ["image/png"]);
        assert_eq!(
            drops(&selection),
            vec![("image/png", &DropReason::Io("broken pipe".into()))]
        );
    }

    #[test]
    fn abandoning_open_reads_leaves_finished_flavors_alone() {
        let mut pending = pending();
        let text = pending.expect_flavor("text/plain".into(), CAP);
        let _stalled = pending.expect_flavor("text/uri-list".into(), CAP);
        pending.push(text, b"done");
        pending.finish(text);

        assert_eq!(pending.abandon_open(DropReason::Stalled), 1);
        assert!(pending.is_complete());

        let selection = pending.into_selection();
        assert_eq!(selection.flavors.len(), 1);
        assert_eq!(selection.flavors[0].data, b"done");
        assert_eq!(
            drops(&selection),
            vec![("text/uri-list", &DropReason::Stalled)]
        );
    }

    #[test]
    fn resolving_a_slot_twice_does_not_disturb_the_open_count() {
        let mut pending = pending();
        let text = pending.expect_flavor("text/plain".into(), CAP);
        let html = pending.expect_flavor("text/html".into(), CAP);
        pending.finish(text);
        // Late EOF, a duplicate error, and stray bytes must all be no-ops.
        pending.finish(text);
        pending.drop_flavor(text, DropReason::Stalled);
        assert_eq!(pending.push(text, b"late"), Push::Closed);
        assert!(!pending.is_complete(), "text/html is still open");

        pending.finish(html);
        assert!(pending.is_complete());
        assert_eq!(pending.into_selection().flavors.len(), 2);
    }

    #[test]
    fn bytes_for_an_unknown_slot_are_ignored() {
        let mut pending = pending();
        assert_eq!(pending.push(7, b"nowhere"), Push::Closed);
        pending.finish(7);
        pending.drop_flavor(7, DropReason::Stalled);
        assert!(pending.is_complete());
    }
}
