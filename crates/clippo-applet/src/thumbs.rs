//! The thumbnail cache: which image rows still need asking for, and what to
//! draw for one.
//!
//! Its own module for the reason [`crate::model`] is: this is bookkeeping that
//! is easy to get subtly wrong and does not need a compositor to exercise. The
//! two ways it has been wrong are both pinned by tests below.
//!
//! # Keyed on the entry, not on the id
//!
//! See [`EntryKey`]. An id is a row slot SQLite reissues, so a cache keyed on
//! one hands a deleted screenshot back for whatever entry inherits its id — the
//! user's remedy for "get that picture out of my history" leaving the picture
//! on screen.
//!
//! # An entry is marked as asked only once the request has been taken
//!
//! The request channel is bounded, so a `try_send` can fail while the worker is
//! behind. If marking happened first, one full queue would be permanent: every
//! id past the limit would be marked, dropped, and never requested again for
//! the life of the applet — drawing the generic image icon, which is also what
//! a genuinely thumbnail-less image draws, so the failure is invisible.
//!
//! The two orderings are not symmetrical, which is why this holds without any
//! claim about when the worker runs: marking after a send that failed asks
//! again, and marking before one asks never.
//!
//! [`Thumbnails::asked`] is therefore a separate call the caller makes *after*
//! the send succeeds, and [`Thumbnails::wanted`] keeps returning anything that
//! never got that far.

use std::collections::{HashMap, HashSet};

use clippo_ipc::EntrySummary;
use cosmic::widget::image::Handle;

use crate::model::{EntryKey, IMAGE_KIND};

/// How many decoded thumbnails to keep before dropping the ones no longer on
/// screen.
///
/// The cache is kept across popup opens on purpose — that is what stops
/// reopening the picker re-fetching every image — so pruning to the visible
/// list on every refresh would undo it, a refresh happening on every keystroke
/// and a query showing a handful of rows. Bounded instead: nothing is dropped
/// until there is more here than a picker could plausibly be showing.
const CAPACITY: usize = 256;

/// Decoded thumbnails, and what has been asked for.
#[derive(Debug, Default)]
pub struct Thumbnails {
    decoded: HashMap<EntryKey, Handle>,
    /// Entries a request has actually been handed to the bus worker for,
    /// whatever came back. Without it an image stored without a thumbnail would
    /// be re-requested on every refresh — a call per row per keystroke.
    asked: HashSet<EntryKey>,
}

impl Thumbnails {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// The image rows that still need a `Thumbnail` call, in list order.
    ///
    /// Only image rows: nothing else has a thumbnail to fetch, and asking would
    /// spend a round trip to be told so.
    pub fn wanted(&self, entries: &[EntrySummary]) -> Vec<EntryKey> {
        entries
            .iter()
            .filter(|entry| entry.kind == IMAGE_KIND)
            .map(EntryKey::of)
            .filter(|key| !self.asked.contains(key))
            .collect()
    }

    /// Note that the worker has taken a request for this entry.
    ///
    /// Called by the sender on success only. See the module docs for why that
    /// ordering is the whole point.
    pub fn asked(&mut self, key: EntryKey) {
        self.asked.insert(key);
    }

    /// File an answer. `None` is an entry that has no thumbnail to give.
    pub fn store(&mut self, key: EntryKey, bytes: Option<Vec<u8>>) {
        if let Some(bytes) = bytes {
            self.decoded.insert(key, Handle::from_bytes(bytes));
        }
    }

    /// What to draw on one row, if anything.
    ///
    /// Answers `None` for a row that is not an image however full the cache is.
    /// A picture is only ever the right thing on an image row, so asking the
    /// entry rather than the cache means no cache mistake can put one on a row
    /// of text.
    pub fn get(&self, entry: &EntrySummary) -> Option<&Handle> {
        if entry.kind != IMAGE_KIND {
            return None;
        }
        self.decoded.get(&EntryKey::of(entry))
    }

    /// Drop anything not on screen, once there is more here than a picker could
    /// be showing.
    ///
    /// Purely about memory. Nothing here is load bearing for correctness —
    /// [`EntryKey`] is what stops a cached image being handed back for the
    /// wrong entry, and it does that whether or not this ever runs.
    pub fn prune(&mut self, entries: &[EntrySummary]) {
        if self.decoded.len() <= CAPACITY && self.asked.len() <= CAPACITY {
            return;
        }
        let visible: HashSet<EntryKey> = entries.iter().map(EntryKey::of).collect();
        self.decoded.retain(|key, _| visible.contains(key));
        self.asked.retain(|key| visible.contains(key));
    }

    /// Forget everything.
    ///
    /// For a daemon that went away: a restarted `clippod` has a new database
    /// handle and may have swept on startup, so nothing cached about the old
    /// one is worth keeping.
    pub fn clear(&mut self) {
        self.decoded.clear();
        self.asked.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, kind: &str) -> EntrySummary {
        EntrySummary {
            id,
            created_at: id * 1_000,
            last_used_at: id * 1_000,
            kind: kind.to_owned(),
            preview: format!("entry {id}"),
            pinned: false,
            sensitive: false,
        }
    }

    fn png() -> Vec<u8> {
        b"\x89PNG\r\n\x1a\n not really".to_vec()
    }

    #[test]
    fn only_image_rows_are_worth_asking_about() {
        let cache = Thumbnails::new();
        let rows = [entry(1, "text"), entry(2, "image"), entry(3, "uris")];

        assert_eq!(cache.wanted(&rows), vec![EntryKey::of(&rows[1])]);
    }

    #[test]
    fn an_entry_already_asked_about_is_not_asked_about_again() {
        let mut cache = Thumbnails::new();
        let rows = [entry(2, "image")];

        cache.asked(EntryKey::of(&rows[0]));

        assert!(cache.wanted(&rows).is_empty());
    }

    /// The blocking half of the ordering rule. A request the worker never took
    /// must come back round — otherwise a history with more image rows than the
    /// queue holds loses the rest of its thumbnails for the session, and the
    /// generic icon it draws is indistinguishable from "stored without one".
    #[test]
    fn a_request_that_was_never_taken_is_still_wanted() {
        let mut cache = Thumbnails::new();
        let rows: Vec<EntrySummary> = (1..=3).map(|id| entry(id, "image")).collect();

        // Two got through; the third's `try_send` failed, so nothing was
        // recorded for it.
        cache.asked(EntryKey::of(&rows[0]));
        cache.asked(EntryKey::of(&rows[1]));

        assert_eq!(cache.wanted(&rows), vec![EntryKey::of(&rows[2])]);
    }

    /// An entry the daemon has no thumbnail for is not re-asked either — the
    /// answer was "there isn't one", which does not change on a keystroke.
    #[test]
    fn an_entry_with_no_thumbnail_is_not_asked_about_forever() {
        let mut cache = Thumbnails::new();
        let rows = [entry(2, "image")];
        let key = EntryKey::of(&rows[0]);

        cache.asked(key);
        cache.store(key, None);

        assert!(cache.wanted(&rows).is_empty());
        assert!(cache.get(&rows[0]).is_none(), "so the row draws the icon");
    }

    #[test]
    fn a_stored_thumbnail_is_what_its_row_draws() {
        let mut cache = Thumbnails::new();
        let row = entry(2, "image");

        cache.store(EntryKey::of(&row), Some(png()));

        assert!(cache.get(&row).is_some());
    }

    /// The other blocking half, in the form only the key can catch. SQLite
    /// reissues a deleted id to the next insert; when that next entry is *also*
    /// an image, the kind check below cannot help and a cache keyed on the id
    /// alone draws the deleted screenshot for it.
    #[test]
    fn a_reissued_id_does_not_inherit_the_deleted_entrys_picture() {
        let mut cache = Thumbnails::new();

        let mut deleted = entry(42, "image");
        deleted.created_at = 1_000;
        cache.store(EntryKey::of(&deleted), Some(png()));
        cache.asked(EntryKey::of(&deleted));
        assert!(cache.get(&deleted).is_some());

        // Deleted, then a copy that SQLite hands the same id to.
        let mut reissued = entry(42, "image");
        reissued.created_at = 2_000;

        assert!(
            cache.get(&reissued).is_none(),
            "a different entry must not draw the deleted one's picture"
        );
        assert_eq!(
            cache.wanted(std::slice::from_ref(&reissued)),
            vec![EntryKey::of(&reissued)],
            "and its own thumbnail is fetched rather than assumed cached"
        );
    }

    /// The cheaper half of the same protection: whatever the cache holds, a row
    /// that is not an image draws an icon. This is what covers a reissued id
    /// going to a *text* entry, which is the likeliest version of it — `Delete`
    /// on the default selection, then an ordinary copy.
    #[test]
    fn a_row_that_is_not_an_image_never_draws_a_picture() {
        let mut cache = Thumbnails::new();
        let image = entry(7, "image");
        cache.store(EntryKey::of(&image), Some(png()));

        let text = entry(7, "text");
        assert_eq!(EntryKey::of(&image), EntryKey::of(&text), "same key");
        assert!(cache.get(&text).is_none());
    }

    #[test]
    fn a_small_cache_is_left_alone_so_reopening_the_picker_refetches_nothing() {
        let mut cache = Thumbnails::new();
        let row = entry(2, "image");
        cache.store(EntryKey::of(&row), Some(png()));
        cache.asked(EntryKey::of(&row));

        // A query that matches nothing — the common case, on every keystroke.
        cache.prune(&[]);

        assert!(cache.get(&row).is_some());
        assert!(cache.wanted(&[row]).is_empty());
    }

    #[test]
    fn an_oversized_cache_is_cut_back_to_what_is_on_screen() {
        let mut cache = Thumbnails::new();
        let rows: Vec<EntrySummary> = (1..=(CAPACITY as i64 + 2))
            .map(|id| entry(id, "image"))
            .collect();
        for row in &rows {
            cache.store(EntryKey::of(row), Some(png()));
            cache.asked(EntryKey::of(row));
        }

        let visible = rows[..2].to_vec();
        cache.prune(&visible);

        assert!(cache.get(&visible[0]).is_some());
        assert!(cache.get(&rows[100]).is_none());
        assert_eq!(cache.decoded.len(), 2);
        assert_eq!(cache.asked.len(), 2);
    }

    #[test]
    fn a_restarted_daemon_invalidates_everything() {
        let mut cache = Thumbnails::new();
        let row = entry(2, "image");
        cache.store(EntryKey::of(&row), Some(png()));
        cache.asked(EntryKey::of(&row));

        cache.clear();

        assert!(cache.get(&row).is_none());
        assert_eq!(cache.wanted(&[row]).len(), 1, "and is asked for afresh");
    }
}
