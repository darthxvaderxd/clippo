//! What clippo advertises when it puts an entry back on the clipboard, and the
//! non-blocking write that answers each paste.
//!
//! Nothing here touches Wayland, exactly as [`crate::flavor`] does not, which
//! is what makes the interesting half of the copy-back path testable against a
//! real pipe rather than against a compositor.
//!
//! # Why the write must not block
//!
//! A `send` event hands us a file descriptor and the receiving application
//! decides when — and whether — it ever reads from it. A blocking `write` of a
//! screenshot into a 64 KiB pipe would therefore park the *whole* event loop on
//! an application that is busy, minimised, or wedged: no captures, no further
//! pastes, no shutdown. So the fd is made non-blocking, [`BlobWriter`] pushes
//! only as much as the pipe will take, and the loop comes back to it when there
//! is room. A receiver that never reads costs one registered descriptor and no
//! loop time at all, because a full pipe is simply never write-ready.

use std::os::fd::BorrowedFd;
use std::sync::Arc;

use rustix::io::Errno;

use crate::{mime, Flavor};

/// One flavor clippo advertises to the compositor.
///
/// The bytes are behind an [`Arc`] because one offer answers many pastes: every
/// application that asks for a flavor gets its own pipe and its own
/// [`BlobWriter`], and copying a multi-megabyte screenshot per paste would make
/// the cost of pasting proportional to how many things are listening.
#[derive(Clone)]
pub(crate) struct OfferedFlavor {
    pub(crate) mime: String,
    pub(crate) data: Arc<Vec<u8>>,
}

impl std::fmt::Debug for OfferedFlavor {
    /// Length, not content — see [`Flavor`]'s own impl for why.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OfferedFlavor")
            .field("mime", &self.mime)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// Turn the flavors of a stored entry into the list clippo will advertise.
///
/// Two flavors are removed:
///
/// - anything with an **empty MIME type**, which no application can ask for and
///   which the protocol has no way to express;
/// - a **repeat of a MIME type already in the list**, because `offer` is a set
///   and the second one would only ever shadow the first.
///
/// Note what is *not* filtered here: clippo's derived thumbnail. That exclusion
/// belongs to the caller — `clippo-store`'s `NEVER_OFFERED` names it next to the
/// MIME it mirrors — because this crate has no reason to know that one
/// particular `image/png` variant is clippo's own invention.
pub(crate) fn offered_flavors(flavors: Vec<Flavor>) -> Vec<OfferedFlavor> {
    let mut offered: Vec<OfferedFlavor> = Vec::with_capacity(flavors.len());
    for flavor in flavors {
        if flavor.mime.trim().is_empty() {
            tracing::warn!("not offering a stored flavor that has no MIME type");
            continue;
        }
        if offered
            .iter()
            .any(|already| mime::same(&already.mime, &flavor.mime))
        {
            tracing::debug!(mime = %flavor.mime, "not offering a flavor twice");
            continue;
        }
        offered.push(OfferedFlavor {
            mime: flavor.mime,
            data: Arc::new(flavor.data),
        });
    }
    offered
}

/// The bytes to answer a `send` for this MIME type with, if we advertised it.
///
/// Matching is [`mime::same`] rather than string equality: an application is
/// entitled to ask using any spelling of the type it was offered.
pub(crate) fn blob_for(offered: &[OfferedFlavor], wanted: &str) -> Option<Arc<Vec<u8>>> {
    offered
        .iter()
        .find(|flavor| mime::same(&flavor.mime, wanted))
        .map(|flavor| Arc::clone(&flavor.data))
}

/// How far a paste got the last time the pipe was written to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WriteProgress {
    /// Every byte is through. Close the pipe — that is the receiver's EOF.
    Done,
    /// The pipe is full. Wait for the receiver to drain it.
    Blocked,
    /// The receiver went away, or the write failed for good.
    Failed(String),
}

/// One paste in flight: a blob, a cursor into it, and nothing else.
#[derive(Debug)]
pub(crate) struct BlobWriter {
    data: Arc<Vec<u8>>,
    written: usize,
}

impl BlobWriter {
    pub(crate) fn new(data: Arc<Vec<u8>>) -> Self {
        Self { data, written: 0 }
    }

    /// Bytes still owed to the receiver.
    pub(crate) fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.written)
    }

    /// Push as much as the pipe will take right now.
    ///
    /// `fd` must already be non-blocking; that is the caller's job, and it is
    /// what turns a full pipe into [`WriteProgress::Blocked`] instead of a
    /// parked event loop.
    ///
    /// An empty blob is [`WriteProgress::Done`] on the first call without any
    /// write at all, which is correct: a flavor with no bytes is an immediate
    /// EOF, not a failure.
    pub(crate) fn pump(&mut self, fd: BorrowedFd<'_>) -> WriteProgress {
        while self.written < self.data.len() {
            match rustix::io::write(fd, &self.data[self.written..]) {
                // Not an error, and not progress either. Treating it as success
                // would spin this loop forever on a pipe that accepts nothing.
                Ok(0) => return WriteProgress::Failed("the receiver accepted no bytes".to_owned()),
                Ok(bytes) => self.written += bytes,
                Err(Errno::INTR) => {}
                // `Errno::WOULDBLOCK` is the same value as `AGAIN` on Linux.
                Err(Errno::AGAIN) => return WriteProgress::Blocked,
                // EPIPE lands here: the application asked for a flavor and then
                // closed its end, which is ordinary — a paste the user cancelled
                // looks exactly like this.
                Err(error) => return WriteProgress::Failed(error.to_string()),
            }
        }
        WriteProgress::Done
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::{AsFd, OwnedFd};

    use super::*;

    /// A pipe whose write end is non-blocking, as the `send` handler makes it.
    ///
    /// The read end is non-blocking too, which the real receiver's is not: it
    /// belongs to the pasting application, and here it belongs to the same
    /// thread as the writer. A blocking [`drain`] on an empty pipe would be a
    /// test that hangs rather than one that fails.
    fn nonblocking_pipe() -> (OwnedFd, OwnedFd) {
        let (read, write) = rustix::pipe::pipe_with(
            rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
        )
        .expect("a pipe");
        (read, write)
    }

    fn drain(fd: &OwnedFd) -> Vec<u8> {
        let mut all = Vec::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            match rustix::io::read(fd.as_fd(), &mut chunk) {
                Ok(0) => return all,
                Ok(bytes) => all.extend_from_slice(&chunk[..bytes]),
                Err(Errno::INTR) => {}
                Err(Errno::AGAIN) => return all,
                Err(error) => panic!("the test pipe would not read: {error}"),
            }
        }
    }

    fn flavors(pairs: &[(&str, &str)]) -> Vec<Flavor> {
        pairs
            .iter()
            .map(|(mime, data)| Flavor::new(*mime, *data))
            .collect()
    }

    #[test]
    fn a_blob_that_fits_goes_out_in_one_pump() {
        let (read, write) = nonblocking_pipe();
        let mut writer = BlobWriter::new(Arc::new(b"hunter2".to_vec()));
        assert_eq!(writer.pump(write.as_fd()), WriteProgress::Done);
        assert_eq!(writer.remaining(), 0);
        drop(write);
        assert_eq!(drain(&read), b"hunter2");
    }

    #[test]
    fn an_empty_flavor_is_an_immediate_eof_rather_than_a_failure() {
        let (read, write) = nonblocking_pipe();
        let mut writer = BlobWriter::new(Arc::new(Vec::new()));
        assert_eq!(writer.pump(write.as_fd()), WriteProgress::Done);
        drop(write);
        assert!(drain(&read).is_empty());
    }

    /// The property the event loop depends on: a receiver that is not reading
    /// yields control back instead of parking the thread. The blob is several
    /// times a pipe's default 64 KiB buffer, so it cannot go out in one write.
    #[test]
    fn a_receiver_that_is_not_reading_blocks_rather_than_wedging_the_writer() {
        let (read, write) = nonblocking_pipe();
        let blob: Vec<u8> = (0..1_000_000u32).map(|byte| byte as u8).collect();
        let mut writer = BlobWriter::new(Arc::new(blob.clone()));

        assert_eq!(writer.pump(write.as_fd()), WriteProgress::Blocked);
        let stalled = writer.remaining();
        assert!(stalled > 0, "the whole blob cannot have fitted in a pipe");
        // Pumping again without the receiver having read changes nothing, and
        // still returns rather than blocking.
        assert_eq!(writer.pump(write.as_fd()), WriteProgress::Blocked);
        assert_eq!(writer.remaining(), stalled);

        // Now let the receiver drain, one pump per read, exactly as the loop
        // does when the fd becomes writable again.
        let mut received = Vec::new();
        loop {
            received.extend_from_slice(&drain(&read));
            match writer.pump(write.as_fd()) {
                WriteProgress::Done => break,
                WriteProgress::Blocked => {}
                WriteProgress::Failed(error) => panic!("the write failed: {error}"),
            }
        }
        drop(write);
        received.extend_from_slice(&drain(&read));
        assert_eq!(received, blob);
    }

    #[test]
    fn a_receiver_that_hung_up_fails_the_write_rather_than_retrying_it() {
        let (read, write) = nonblocking_pipe();
        drop(read);
        let mut writer = BlobWriter::new(Arc::new(b"nobody is listening".to_vec()));
        assert!(
            matches!(writer.pump(write.as_fd()), WriteProgress::Failed(_)),
            "a closed receiver must be a failure, not a retry"
        );
    }

    #[test]
    fn every_stored_flavor_is_offered_once_and_in_order() {
        let offered = offered_flavors(flavors(&[
            ("text/plain;charset=utf-8", "hi"),
            ("text/html", "<b>hi</b>"),
            ("text/plain", "hi"),
        ]));
        let mimes: Vec<&str> = offered.iter().map(|f| f.mime.as_str()).collect();
        assert_eq!(
            mimes,
            ["text/plain;charset=utf-8", "text/html", "text/plain"]
        );
    }

    #[test]
    fn a_repeated_or_nameless_flavor_is_not_offered() {
        let offered = offered_flavors(flavors(&[
            ("text/plain", "first"),
            ("text/plain; charset=", "shadow"),
            ("TEXT/PLAIN", "shadow"),
            ("", "nameless"),
            ("text/html", "<b>hi</b>"),
        ]));
        let mimes: Vec<&str> = offered.iter().map(|f| f.mime.as_str()).collect();
        assert_eq!(mimes, ["text/plain", "text/plain; charset=", "text/html"]);
        assert_eq!(
            *blob_for(&offered, "text/plain").unwrap(),
            b"first".to_vec()
        );
    }

    #[test]
    fn a_paste_finds_the_flavor_however_it_spells_the_type() {
        let offered = offered_flavors(flavors(&[
            ("text/plain;charset=utf-8", "hi"),
            ("image/png", "PNG-ish bytes"),
        ]));
        assert_eq!(
            *blob_for(&offered, "text/plain; charset=UTF-8").unwrap(),
            b"hi".to_vec()
        );
        assert_eq!(
            *blob_for(&offered, "IMAGE/PNG").unwrap(),
            b"PNG-ish bytes".to_vec()
        );
        assert!(blob_for(&offered, "text/html").is_none());
    }

    #[test]
    fn one_blob_serves_many_pastes_without_copying_it() {
        let offered = offered_flavors(flavors(&[("image/png", "pretend this is a screenshot")]));
        let first = blob_for(&offered, "image/png").unwrap();
        let second = blob_for(&offered, "image/png").unwrap();
        assert!(
            Arc::ptr_eq(&first, &second),
            "two pastes of one entry must share the bytes, not clone them"
        );
    }

    #[test]
    fn debug_does_not_print_what_is_on_the_clipboard() {
        let offered = offered_flavors(flavors(&[("text/plain", "hunter2")]));
        let rendered = format!("{offered:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("text/plain"), "{rendered}");
    }
}
