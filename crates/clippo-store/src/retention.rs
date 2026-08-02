//! How long history is kept, and what is exempt from being dropped.
//!
//! DESIGN.md, `clippo-store` → "Retention":
//!
//! > Max entries (default 500) and max age (default 30 days). **Pinned entries
//! > are exempt from both**, and from `Clear()` unless explicitly included.
//!
//! # Pin exemption, stated precisely
//!
//! A pinned entry is not merely spared — it does not occupy the budget either.
//! ROADMAP.md's verification 5 is the shape to hold in mind:
//!
//! > Pin an entry, set `max_entries = 5`, copy ten things. The pinned entry
//! > survives, and five unpinned remain.
//!
//! Six rows, not five. Both retention statements below therefore filter on
//! `pinned = 0` on *both* sides: the `DELETE` skips pinned rows, and the
//! `SELECT` that decides which rows are the newest `max_entries` only ever
//! looks at unpinned ones. Counting pinned rows toward the cap would quietly
//! shrink a user's live history every time they pinned something, which is the
//! opposite of what pinning is for.
//!
//! # When the sweep runs
//!
//! **After every insert, in the insert's own transaction** — see
//! [`Store::insert`](crate::Store::insert). Not on a timer, for three reasons:
//!
//! - It is two indexed `DELETE`s that match nothing in the ordinary case, run
//!   at most as often as the user copies something. A timer would cost the same
//!   work while adding a task to own it and a window in which the limits are
//!   untrue.
//! - Sharing the insert's transaction means the history is never observably
//!   over its limits, not even between two statements.
//! - It needs no clock of its own: the copy being inserted carries the capture
//!   time, which is the most recent reading anything has.
//!
//! The one gap that leaves is a daemon nobody copies anything into: an entry
//! passes its 30 days without a sweep noticing. [`Store::enforce_retention`] is
//! the same sweep exposed for a caller with its own clock — `clippod` runs it
//! once at startup — so the gap closes at the next start rather than requiring
//! a timer to exist.
//!
//! # Age is measured from last use
//!
//! `last_used_at`, not `created_at`. An entry copied a year ago and pasted this
//! morning is in active use, and deleting it on its birthday would be a
//! surprise the user cannot prevent except by pinning. Re-copying or pasting an
//! entry therefore renews it, which is the same rule that already governs its
//! position in the list.

use std::time::Duration;

use clippo_core::{Config, Timestamp};
use rusqlite::Connection;

use crate::StoreError;

/// The two limits, resolved from [`Config`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// How many **unpinned** entries to keep. Pinned entries are extra.
    pub max_entries: usize,

    /// How old an unpinned entry may get, or `None` for no age limit.
    ///
    /// Already resolved: [`Config::max_age`] is where `max_age_days = 0` is
    /// given its meaning, so the zero case cannot be forgotten here.
    pub max_age: Option<Duration>,
}

impl Retention {
    /// The limits a [`Config`] asks for.
    pub fn from_config(config: &Config) -> Self {
        Self {
            max_entries: config.max_entries,
            max_age: config.max_age(),
        }
    }

    /// Keep everything: no count limit and no age limit.
    ///
    /// Not reachable from a config file — `max_entries = 0` is refused and
    /// there is no "unlimited" spelling — but useful to a caller that wants to
    /// sweep on its own terms, and to tests that are about something else.
    pub const fn unlimited() -> Self {
        Self {
            max_entries: usize::MAX,
            max_age: None,
        }
    }
}

impl Default for Retention {
    /// DESIGN.md's defaults: 500 entries, 30 days.
    fn default() -> Self {
        Self::from_config(&Config::default())
    }
}

/// What one retention pass deleted.
///
/// The two counts are kept apart because they mean different things to whoever
/// reads the log: entries going out by age is a history working as designed,
/// while entries going out by count on a quiet machine says `max_entries` is
/// set lower than the user actually wants.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sweep {
    /// Unpinned entries deleted for being older than the age limit.
    pub expired: usize,
    /// Unpinned entries deleted for being outside the newest `max_entries`.
    pub over_capacity: usize,
}

impl Sweep {
    /// How many entries the pass deleted in total.
    pub const fn total(self) -> usize {
        self.expired + self.over_capacity
    }

    /// Whether the pass deleted anything, i.e. whether there is now freed space
    /// to reclaim.
    pub const fn is_empty(self) -> bool {
        self.total() == 0
    }
}

/// Apply both limits, deleting the unpinned entries that fall outside them.
///
/// `now` is the caller's notion of the current time — for the after-insert pass
/// that is the capture time of the copy going in. Flavor rows and their blobs
/// go with the entry through `ON DELETE CASCADE`; nothing here deletes from
/// `flavors` by hand, so there is no second statement to forget.
pub(crate) fn sweep(
    conn: &Connection,
    policy: &Retention,
    now: Timestamp,
) -> Result<Sweep, StoreError> {
    let expired = match policy.max_age {
        None => 0,
        Some(max_age) => conn.execute(
            "DELETE FROM entries WHERE pinned = 0 AND last_used_at < ?1",
            [now.saturating_sub(max_age).as_unix_millis()],
        )?,
    };

    // The subquery is the newest `max_entries` *unpinned* entries, in exactly
    // the order `Store::list` shows them, so what survives is what the user can
    // see at the top of their history. `NOT IN` over an id list rather than a
    // `LIMIT ... OFFSET` delete because SQLite only supports the latter with a
    // compile-time option that the bundled SQLCipher does not carry.
    let over_capacity = conn.execute(
        "DELETE FROM entries
         WHERE pinned = 0
           AND id NOT IN (
               SELECT id FROM entries
               WHERE pinned = 0
               ORDER BY last_used_at DESC, id DESC
               LIMIT ?1
           )",
        [i64::try_from(policy.max_entries).unwrap_or(i64::MAX)],
    )?;

    if expired + over_capacity > 0 {
        tracing::debug!(
            expired,
            over_capacity,
            "clippo dropped history entries that were outside its retention limits"
        );
    }

    Ok(Sweep {
        expired,
        over_capacity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Temp;
    use crate::{dedup, Store};
    use clippo_core::{EntryId, EntryKind, Flavor, NewEntry};

    /// A day, in milliseconds — the unit every timestamp in here is written in.
    const DAY: i64 = 24 * 60 * 60 * 1_000;

    /// A text selection captured at `at`.
    fn text(at: i64, body: &str) -> NewEntry {
        let flavors = vec![Flavor::new("text/plain;charset=utf-8", body)];
        NewEntry {
            created_at: Timestamp::from_unix_millis(at),
            kind: EntryKind::Text,
            preview: body.to_owned(),
            hash: dedup::hash(EntryKind::Text, &flavors).expect("text has a canonical flavor"),
            sensitive: false,
            flavors,
        }
    }

    /// A store whose only configured limit is `max_entries`.
    fn keeping(temp: &Temp, max_entries: usize) -> Store {
        temp.open().with_retention(Retention {
            max_entries,
            max_age: None,
        })
    }

    /// A store whose only configured limit is an age.
    fn keeping_for(temp: &Temp, days: u32) -> Store {
        temp.open().with_retention(Retention {
            max_entries: usize::MAX,
            max_age: Some(Duration::from_secs(u64::from(days) * 24 * 60 * 60)),
        })
    }

    fn previews(store: &Store) -> Vec<String> {
        store
            .list(100, 0)
            .unwrap()
            .into_iter()
            .map(|entry| entry.preview)
            .collect()
    }

    fn flavor_rows(store: &Store) -> i64 {
        store
            .connection()
            .query_row("SELECT count(*) FROM flavors", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn the_defaults_are_the_ones_design_md_documents() {
        let policy = Retention::default();
        assert_eq!(policy.max_entries, 500);
        assert_eq!(policy.max_age, Some(Duration::from_secs(30 * 24 * 60 * 60)));
        assert_eq!(policy, Retention::from_config(&Config::default()));

        let config = Config {
            max_age_days: 0,
            ..Config::default()
        };
        assert_eq!(Retention::from_config(&config).max_age, None);
    }

    #[test]
    fn the_oldest_entries_go_when_there_are_too_many() {
        let temp = Temp::new();
        let mut store = keeping(&temp, 3);
        for n in 0..6 {
            store
                .insert(&text(1_000 + n, &format!("copy {n}")))
                .unwrap();
        }

        assert_eq!(store.count().unwrap(), 3);
        assert_eq!(previews(&store), vec!["copy 5", "copy 4", "copy 3"]);
    }

    #[test]
    fn entries_older_than_the_age_limit_go() {
        let temp = Temp::new();
        let mut store = keeping_for(&temp, 30);

        // Day 0 and day 5 of some month, then a copy 40 days later. The first
        // two are past their thirty days by then; the third is not.
        store.insert(&text(0, "ancient")).unwrap();
        store.insert(&text(5 * DAY, "old")).unwrap();
        assert_eq!(store.count().unwrap(), 2);

        store.insert(&text(40 * DAY, "fresh")).unwrap();
        assert_eq!(previews(&store), vec!["fresh"]);
    }

    #[test]
    fn an_entry_pasted_recently_is_not_old_however_long_ago_it_was_copied() {
        // Age is measured from last use, so the daily-used snippet copied a
        // year ago survives while its neighbour does not.
        let temp = Temp::new();
        let mut store = keeping_for(&temp, 30);
        let id = store.insert(&text(0, "in daily use")).unwrap().id();
        store.insert(&text(0, "forgotten")).unwrap();

        assert!(store
            .touch(id, Timestamp::from_unix_millis(39 * DAY))
            .unwrap());
        store.insert(&text(40 * DAY, "today")).unwrap();

        assert_eq!(previews(&store), vec!["today", "in daily use"]);
    }

    #[test]
    fn a_pinned_entry_survives_the_count_limit_and_does_not_use_up_the_budget() {
        // ROADMAP.md verification 5, automated: pin an entry, set
        // `max_entries = 5`, copy ten things — the pinned entry survives, and
        // five unpinned remain. Six rows, not five.
        let temp = Temp::new();
        let mut store = keeping(&temp, 5);

        let pinned = store.insert(&text(1_000, "pinned")).unwrap().id();
        assert!(store.set_pinned(pinned, true).unwrap());

        for n in 0..10 {
            store
                .insert(&text(2_000 + n, &format!("copy {n}")))
                .unwrap();
        }

        let entries = store.list(100, 0).unwrap();
        assert_eq!(entries.len(), 6, "five unpinned plus the pinned one");
        assert_eq!(
            entries.iter().filter(|entry| !entry.pinned).count(),
            5,
            "the pinned entry must not be counted toward the unpinned budget"
        );
        assert!(store.get(pinned).unwrap().is_some(), "the pin survived");
        assert_eq!(
            previews(&store),
            vec!["copy 9", "copy 8", "copy 7", "copy 6", "copy 5", "pinned"]
        );
    }

    #[test]
    fn a_pinned_entry_never_expires_however_old_it_gets() {
        let temp = Temp::new();
        let mut store = keeping_for(&temp, 30);

        let pinned = store.insert(&text(0, "pinned")).unwrap().id();
        store.set_pinned(pinned, true).unwrap();
        store.insert(&text(0, "unpinned")).unwrap();

        // A decade later, not merely a day past the limit.
        store.insert(&text(3_650 * DAY, "today")).unwrap();
        assert_eq!(previews(&store), vec!["today", "pinned"]);
    }

    #[test]
    fn unpinning_an_old_entry_lets_the_next_sweep_take_it() {
        // The exemption is a property of the row, not something granted once at
        // insert: an entry that has been unpinned is ordinary again.
        let temp = Temp::new();
        let mut store = keeping_for(&temp, 30);
        let id = store.insert(&text(0, "was pinned")).unwrap().id();
        store.set_pinned(id, true).unwrap();
        store.insert(&text(40 * DAY, "today")).unwrap();
        assert_eq!(store.count().unwrap(), 2);

        store.set_pinned(id, false).unwrap();
        store
            .enforce_retention(Timestamp::from_unix_millis(40 * DAY))
            .unwrap();
        assert_eq!(previews(&store), vec!["today"]);
    }

    #[test]
    fn retention_takes_the_flavor_rows_and_their_blobs_with_it() {
        // An orphaned `flavors` row is a clipboard blob that outlived the entry
        // the user can see — the thing `ON DELETE CASCADE` and
        // `PRAGMA foreign_keys` exist for, checked after a sweep rather than
        // after an explicit delete.
        let temp = Temp::new();
        let mut store = keeping(&temp, 2);

        for n in 0..5 {
            let body = format!("copy {n}");
            let flavors = vec![
                Flavor::new("text/html", format!("<b>{body}</b>")),
                Flavor::new("text/plain;charset=utf-8", body.clone()),
            ];
            store
                .insert(&NewEntry {
                    created_at: Timestamp::from_unix_millis(1_000 + n),
                    kind: EntryKind::Html,
                    preview: body,
                    hash: dedup::hash(EntryKind::Html, &flavors).unwrap(),
                    sensitive: false,
                    flavors,
                })
                .unwrap();
        }

        assert_eq!(store.count().unwrap(), 2);
        assert_eq!(
            flavor_rows(&store),
            4,
            "two surviving entries of two flavors each, and nothing left behind"
        );
    }

    #[test]
    fn a_sweep_reports_which_limit_did_the_deleting() {
        let temp = Temp::new();
        // Retention is off while these go in, so the sweep below is the first.
        let mut store = temp.open().with_retention(Retention::unlimited());
        for n in 0..3 {
            store.insert(&text(n, &format!("ancient {n}"))).unwrap();
        }
        for n in 0..4 {
            store
                .insert(&text(40 * DAY + n, &format!("recent {n}")))
                .unwrap();
        }
        assert_eq!(store.count().unwrap(), 7);

        store.set_retention(Retention {
            max_entries: 2,
            max_age: Some(Duration::from_secs(30 * 24 * 60 * 60)),
        });
        let swept = store
            .enforce_retention(Timestamp::from_unix_millis(40 * DAY))
            .unwrap();

        assert_eq!(swept.expired, 3, "the three from day zero");
        assert_eq!(swept.over_capacity, 2, "two of the four that were left");
        assert_eq!(swept.total(), 5);
        assert!(!swept.is_empty());
        assert_eq!(store.count().unwrap(), 2);

        // A second pass has nothing left to do.
        let swept = store
            .enforce_retention(Timestamp::from_unix_millis(40 * DAY))
            .unwrap();
        assert_eq!(swept, Sweep::default());
        assert!(swept.is_empty());
    }

    #[test]
    fn an_unlimited_policy_deletes_nothing() {
        let temp = Temp::new();
        let mut store = temp.open().with_retention(Retention::unlimited());
        for n in 0..50 {
            store.insert(&text(n * DAY, &format!("copy {n}"))).unwrap();
        }
        assert_eq!(store.count().unwrap(), 50);
        assert!(store
            .enforce_retention(Timestamp::now())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn clear_spares_pinned_entries_unless_it_is_asked_not_to() {
        let temp = Temp::new();
        let mut store = temp.open();
        let pinned = store.insert(&text(1_000, "pinned")).unwrap().id();
        store.set_pinned(pinned, true).unwrap();
        store.insert(&text(2_000, "one")).unwrap();
        store.insert(&text(3_000, "two")).unwrap();

        assert_eq!(store.clear(false).unwrap(), 2);
        assert_eq!(previews(&store), vec!["pinned"]);
        assert_eq!(
            flavor_rows(&store),
            1,
            "the cleared entries took their blobs"
        );

        // And nothing left to clear the second time.
        assert_eq!(store.clear(false).unwrap(), 0);
        assert!(store.get(pinned).unwrap().is_some());
    }

    #[test]
    fn clear_takes_the_pinned_entries_too_when_it_is_told_to() {
        let temp = Temp::new();
        let mut store = temp.open();
        let pinned = store.insert(&text(1_000, "pinned")).unwrap().id();
        store.set_pinned(pinned, true).unwrap();
        store.insert(&text(2_000, "one")).unwrap();

        assert_eq!(store.clear(true).unwrap(), 2);
        assert_eq!(store.count().unwrap(), 0);
        assert_eq!(flavor_rows(&store), 0);
        assert!(store.get(pinned).unwrap().is_none());
        assert!(store.get(EntryId::new(404)).unwrap().is_none());
    }
}
