//! The picker's state, with no libcosmic in it.
//!
//! Everything here is an ordinary struct and an ordinary method, which is the
//! point: the parts of an applet that are easy to get wrong — where the
//! selection lands after the history changes underneath it, when a revealed
//! secret stops being visible — are exactly the parts a compositor is not
//! needed to test. [`crate::app`] is then thin enough to read.
//!
//! # Selection is an id, not an index
//!
//! The obvious representation is "row 3 is highlighted", and it is wrong here
//! for a reason that only shows up live: this list changes while the user is
//! looking at it. A copy made in another window inserts at the top, and every
//! index below it shifts by one — so an index-based selection silently moves to
//! a *different entry* between the user reading the row and pressing Enter, and
//! they paste something they did not choose. Keeping the id means the highlight
//! follows the entry, and a deletion is the one case that has to pick somewhere
//! new to be.
//!
//! # The revealed value
//!
//! M5's rule is that a revealed secret "is dropped when the row loses focus or
//! the popup closes", and [`Model::revealed`] enforces the first half
//! structurally rather than by remembering to call something: it answers `None`
//! unless the revealed id is *also* the selected one. Moving the selection
//! therefore stops a revealed value being rendered without anything having to
//! notice that the selection moved. [`Model::forget_revealed`] covers the
//! second half, and the value itself is [`Zeroizing`] so that dropping it wipes
//! the copy rather than leaving it in freed memory for a core dump to pick up.

use clippo_ipc::EntrySummary;
use zeroize::Zeroizing;

/// The `clippo_core::EntryKind` word for an image row, as it arrives over the
/// bus.
///
/// A `String` on [`EntrySummary`] rather than the enum, because the wire
/// signature is a string. Named once here so that the two places that ask "is
/// this an image" — whether to fetch a thumbnail, and whether to draw one —
/// cannot answer differently.
pub const IMAGE_KIND: &str = "image";

/// Which entry a cached thing belongs to.
///
/// `(id, created_at)` rather than the id alone, and that is not belt and
/// braces: `entries.id` is an `INTEGER PRIMARY KEY` *without* `AUTOINCREMENT`,
/// so SQLite hands out `max(rowid) + 1` and an id is a row slot rather than a
/// name. Deleting the newest entry frees its id for the next copy, and
/// `clippo clear` restarts at 1 — so anything the applet holds across a refresh
/// and looks up by id alone will sooner or later be handed back for a different
/// entry. For a thumbnail that means a deleted screenshot drawn beside the
/// unrelated text the user copied afterwards, which is the user's remedy for
/// "get that picture out of my history" leaving the picture on screen.
///
/// `created_at` is a capture timestamp the daemon already sends on every row,
/// and the store never rewrites it — a repeat copy bumps `last_used_at` and
/// leaves `created_at` alone — so the pair is stable for as long as the entry
/// exists and cannot be reissued to another one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryKey {
    /// The entry's id, as passed to every D-Bus member.
    pub id: i64,
    /// When it was captured, in Unix milliseconds.
    pub created_at: i64,
}

impl EntryKey {
    /// The key for one row.
    pub fn of(entry: &EntrySummary) -> Self {
        Self {
            id: entry.id,
            created_at: entry.created_at,
        }
    }
}

/// Whether the daemon is answering.
///
/// The distinction this exists for is "no daemon" versus "no history": both
/// draw an empty list, and only one of them is a problem the user can fix. An
/// applet that showed a bare empty list for a dead `clippod` would be telling
/// them their clipboard history had been lost.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Status {
    /// Talking to `clippod`.
    ///
    /// The default, optimistically: the applet starts before it has called
    /// anything, and claiming the daemon is missing before having asked would
    /// show the error state on every panel start.
    #[default]
    Connected,
    /// Nothing owns the daemon's name, or a call to it failed.
    DaemonUnavailable,
}

impl Status {
    /// Whether the daemon is answering.
    pub fn is_connected(&self) -> bool {
        matches!(self, Status::Connected)
    }
}

/// A value the user asked to see, and the entry it belongs to.
///
/// Held apart from the [`EntrySummary`] rather than written into its `preview`
/// so that there is exactly one place a full value lives and one place to drop
/// it. Merging it into the entry would mean a revealed secret survived every
/// clone of the list.
struct Revealed {
    id: i64,
    value: Zeroizing<String>,
}

/// The picker's state.
#[derive(Default)]
pub struct Model {
    /// What the user has typed. Sent to the daemon's `Search`; never used to
    /// filter [`entries`](Self::entries) here — see [`Model::set_entries`].
    query: String,
    /// The rows, exactly as the daemon ranked them.
    entries: Vec<EntrySummary>,
    /// The highlighted entry's id, or `None` when the list is empty.
    selected: Option<i64>,
    revealed: Option<Revealed>,
    status: Status,
    /// Whether the next list should land the highlight on its top row.
    ///
    /// Set by [`set_query`](Model::set_query) and [`restart`](Model::restart),
    /// taken by [`set_entries`](Model::set_entries) — they are a round trip
    /// apart, because the applet asks the daemon and finds out what the ranking
    /// is some milliseconds later. Without it `set_entries` cannot tell a fresh
    /// ranking from the same one with a row added or removed, and the two want
    /// opposite answers.
    land_at_top: bool,
}

impl Model {
    /// An empty picker.
    pub fn new() -> Self {
        Self::default()
    }

    /// What the user has typed.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// The rows, in the order they are drawn.
    pub fn entries(&self) -> &[EntrySummary] {
        &self.entries
    }

    /// Whether the daemon is answering.
    pub fn status(&self) -> &Status {
        &self.status
    }

    /// Record whether the daemon is answering.
    ///
    /// Losing the daemon empties the list: the entries on screen describe a
    /// history this applet can no longer act on, and offering rows whose Enter
    /// would fail is worse than showing why.
    pub fn set_status(&mut self, status: Status) {
        if !status.is_connected() {
            self.entries.clear();
            self.selected = None;
            self.forget_revealed();
        }
        self.status = status;
    }

    /// Replace the query. Does not filter anything — the caller sends it to
    /// `Search` and feeds the answer back through [`set_entries`](Self::set_entries).
    ///
    /// Records that the query changed, so the list that comes back is treated
    /// as a new ranking rather than as the old one edited. Only a real change
    /// counts: re-setting the same string happens on any redraw path and must
    /// not move the highlight the user just arrowed onto.
    pub fn set_query(&mut self, query: String) {
        if query != self.query {
            self.land_at_top = true;
        }
        self.query = query;
    }

    /// Say that the next list is a fresh look at the history rather than the
    /// current one changing.
    ///
    /// What opening the picker does. The rows from last time are still here —
    /// they are not thrown away on close, so that reopening draws something
    /// immediately — but the highlight should not still be nine rows down where
    /// the user left it ten minutes ago. The entry they almost certainly want is
    /// the one they just copied, which is the top row.
    ///
    /// Separate from [`set_query`](Self::set_query) because clearing an already
    /// empty query is not a change and would otherwise leave the highlight
    /// exactly where reopening should have moved it.
    pub fn restart(&mut self) {
        self.land_at_top = true;
    }

    /// Replace the rows with what the daemon just returned.
    ///
    /// The daemon has already ranked these; re-sorting or filtering here is
    /// what M5 rules out, because the applet and `clippo search` would then
    /// disagree about the same query.
    ///
    /// Where the highlight lands depends on why the list changed, and the two
    /// reasons want opposite answers:
    ///
    /// - **The same query, re-answered** — a copy arrived, or a row was
    ///   deleted here or by `clippo rm` in a terminal. The selection survives if
    ///   its entry is still there; when it is not, the highlight lands on
    ///   whatever is now at the same position rather than jumping to the top,
    ///   so deleting several rows in a row does not walk the user back to the
    ///   start of the history each time.
    /// - **A fresh ranking** — the user typed, or the picker was just opened.
    ///   The highlight belongs on the best match. Keeping the row number here is
    ///   how `Enter` ends up copying something the ranking did not put first:
    ///   arrow down to row 3, type one character, and the highlight stays on
    ///   row 3 of a list it has never seen.
    pub fn set_entries(&mut self, entries: Vec<EntrySummary>) {
        let previous = self.selected_index();
        let land_at_top = std::mem::take(&mut self.land_at_top);
        self.entries = entries;

        let still_there = !land_at_top
            && self
                .selected
                .is_some_and(|id| self.entries.iter().any(|entry| entry.id == id));
        if !still_there {
            let landing = if land_at_top {
                0
            } else {
                previous
                    .unwrap_or(0)
                    .min(self.entries.len().saturating_sub(1))
            };
            self.selected = self.entries.get(landing).map(|entry| entry.id);
        }

        // Not conditional on the selection having moved: a revealed value must
        // not outlive the list it was read from, and the entry could have been
        // deleted and its id reused by nothing at all.
        self.forget_revealed();
    }

    /// The highlighted entry, if there is one.
    pub fn selected_entry(&self) -> Option<&EntrySummary> {
        let id = self.selected?;
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// The highlighted entry's id.
    pub fn selected_id(&self) -> Option<i64> {
        self.selected
    }

    /// Where the highlight is, as a row number.
    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected?;
        self.entries.iter().position(|entry| entry.id == id)
    }

    /// Highlight one entry by id, ignoring an id that is not in the list.
    pub fn select(&mut self, id: i64) {
        if self.entries.iter().any(|entry| entry.id == id) {
            self.selected = Some(id);
        }
    }

    /// Move the highlight one row towards the top.
    ///
    /// Stops at the ends rather than wrapping: in a list that is scrolled and
    /// keyboard-driven, wrapping from the first row to the last looks like the
    /// list jumped somewhere on its own.
    pub fn select_previous(&mut self) {
        self.step(-1);
    }

    /// Move the highlight one row towards the bottom.
    pub fn select_next(&mut self) {
        self.step(1);
    }

    fn step(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.selected = None;
            return;
        }
        let current = self.selected_index().unwrap_or(0) as isize;
        let last = self.entries.len() as isize - 1;
        let next = (current + delta).clamp(0, last) as usize;
        self.selected = self.entries.get(next).map(|entry| entry.id);
    }

    /// Hold a value `Reveal` just returned, for as long as its row stays
    /// selected.
    ///
    /// Takes anything that becomes a [`Zeroizing<String>`] so the caller can
    /// hand over one it was already holding, rather than this wrapping a plain
    /// `String` and leaving the caller's unwiped copy behind.
    pub fn set_revealed(&mut self, id: i64, value: impl Into<Zeroizing<String>>) {
        self.revealed = Some(Revealed {
            id,
            value: value.into(),
        });
    }

    /// The revealed value to draw, if any.
    ///
    /// `None` unless the revealed entry is the selected one — which is what
    /// makes "dropped when the row loses focus" true by construction rather
    /// than by every caller that moves the selection remembering to clear it.
    pub fn revealed(&self) -> Option<&str> {
        let revealed = self.revealed.as_ref()?;
        (Some(revealed.id) == self.selected).then(|| revealed.value.as_str())
    }

    /// Drop any revealed value, wiping it.
    ///
    /// Called when the popup closes. The [`Zeroizing`] wrapper is what makes
    /// this a wipe rather than a `None` assignment over memory that still holds
    /// the secret.
    pub fn forget_revealed(&mut self) {
        self.revealed = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64) -> EntrySummary {
        EntrySummary {
            id,
            created_at: id,
            last_used_at: id,
            kind: "text".to_owned(),
            preview: format!("entry {id}"),
            pinned: false,
            sensitive: false,
        }
    }

    fn model_with(ids: &[i64]) -> Model {
        let mut model = Model::new();
        model.set_entries(ids.iter().copied().map(entry).collect());
        model
    }

    #[test]
    fn the_first_row_is_selected_once_there_is_one() {
        assert_eq!(model_with(&[]).selected_id(), None);
        assert_eq!(model_with(&[7, 8]).selected_id(), Some(7));
    }

    #[test]
    fn the_arrows_stop_at_the_ends_rather_than_wrapping() {
        let mut model = model_with(&[1, 2, 3]);

        model.select_previous();
        assert_eq!(model.selected_id(), Some(1), "already at the top");

        model.select_next();
        model.select_next();
        assert_eq!(model.selected_id(), Some(3));
        model.select_next();
        assert_eq!(model.selected_id(), Some(3), "already at the bottom");
    }

    /// The reason the selection is an id. A copy made while the popup is open
    /// arrives at the top, and the highlight must stay on the row the user was
    /// looking at rather than following the row *number*.
    #[test]
    fn a_new_entry_arriving_does_not_move_the_highlight_to_another_row() {
        let mut model = model_with(&[5, 6, 7]);
        model.select_next();
        assert_eq!(model.selected_id(), Some(6));

        model.set_entries(vec![entry(9), entry(5), entry(6), entry(7)]);

        assert_eq!(model.selected_id(), Some(6), "same entry, different row");
        assert_eq!(model.selected_index(), Some(2));
    }

    /// `clippo rm` in a terminal, on the row that happens to be highlighted.
    #[test]
    fn deleting_the_selected_row_lands_the_highlight_where_it_was() {
        let mut model = model_with(&[1, 2, 3, 4]);
        model.select_next();
        model.select_next();
        assert_eq!(model.selected_index(), Some(2));

        model.set_entries(vec![entry(1), entry(2), entry(4)]);

        assert_eq!(
            model.selected_id(),
            Some(4),
            "the row that moved up into the gap"
        );
        assert_eq!(model.selected_index(), Some(2));
    }

    #[test]
    fn deleting_the_last_row_falls_back_onto_the_new_last_row() {
        let mut model = model_with(&[1, 2, 3]);
        model.select_next();
        model.select_next();

        model.set_entries(vec![entry(1), entry(2)]);

        assert_eq!(model.selected_id(), Some(2));
    }

    #[test]
    fn emptying_the_history_leaves_nothing_selected() {
        let mut model = model_with(&[1, 2]);
        model.set_entries(vec![]);
        assert_eq!(model.selected_id(), None);
        assert_eq!(model.selected_entry(), None);
    }

    /// The M5 rule, as a test: a revealed value stops being rendered the moment
    /// its row is no longer the focused one.
    #[test]
    fn a_revealed_value_is_not_shown_once_its_row_loses_focus() {
        let mut model = model_with(&[1, 2]);
        model.set_revealed(1, "hunter2".to_owned());
        assert_eq!(model.revealed(), Some("hunter2"));

        model.select_next();
        assert_eq!(
            model.revealed(),
            None,
            "the selection moved, so it must not be drawn"
        );

        model.select_previous();
        assert_eq!(
            model.revealed(),
            Some("hunter2"),
            "coming back is allowed; it never left the focused row"
        );
    }

    #[test]
    fn forgetting_a_revealed_value_is_what_closing_the_popup_does() {
        let mut model = model_with(&[1]);
        model.set_revealed(1, "hunter2".to_owned());

        model.forget_revealed();

        assert_eq!(model.revealed(), None);
    }

    /// A refresh is a new list, and a value read from the old one does not
    /// carry over into it.
    #[test]
    fn a_refresh_drops_a_revealed_value() {
        let mut model = model_with(&[1, 2]);
        model.set_revealed(1, "hunter2".to_owned());

        model.set_entries(vec![entry(1), entry(2)]);

        assert_eq!(model.revealed(), None);
    }

    /// Losing the daemon must not leave rows on screen that Enter would fail
    /// on, and must not leave a secret behind either.
    #[test]
    fn losing_the_daemon_clears_the_rows_and_any_revealed_value() {
        let mut model = model_with(&[1, 2]);
        model.set_revealed(1, "hunter2".to_owned());

        model.set_status(Status::DaemonUnavailable);

        assert!(model.entries().is_empty());
        assert_eq!(model.selected_id(), None);
        assert_eq!(model.revealed(), None);
        assert!(!model.status().is_connected());
    }

    /// The applet ranks nothing itself — this asserts the model hands back
    /// exactly the order it was given, however unlike the query it looks.
    #[test]
    fn the_daemons_ranking_is_preserved_verbatim() {
        let mut model = Model::new();
        model.set_query("zz".to_owned());
        model.set_entries(vec![entry(3), entry(1), entry(2)]);

        assert_eq!(
            model.entries().iter().map(|e| e.id).collect::<Vec<_>>(),
            [3, 1, 2]
        );
    }

    #[test]
    fn selecting_an_id_that_is_not_in_the_list_changes_nothing() {
        let mut model = model_with(&[1, 2]);
        model.select(99);
        assert_eq!(model.selected_id(), Some(1));
    }

    /// Typing is a new ranking, so `Enter` must copy the best match rather than
    /// whatever inherited the row number the highlight happened to be on.
    #[test]
    fn a_new_query_puts_the_highlight_on_the_best_match() {
        let mut model = model_with(&[1, 2, 3, 4]);
        model.select_next();
        model.select_next();
        assert_eq!(model.selected_index(), Some(2));

        model.set_query("ab".to_owned());
        model.set_entries(vec![entry(9), entry(3), entry(1)]);

        assert_eq!(
            model.selected_id(),
            Some(9),
            "the top-ranked match, not row 2 of a list it has never seen"
        );
    }

    /// The other half of the same rule: an entry still in the results after a
    /// query change does not keep the highlight either — the ranking decides.
    #[test]
    fn a_new_query_does_not_keep_the_highlight_on_a_surviving_entry() {
        let mut model = model_with(&[1, 2, 3]);
        model.select_next();
        assert_eq!(model.selected_id(), Some(2));

        model.set_query("b".to_owned());
        model.set_entries(vec![entry(3), entry(2)]);

        assert_eq!(model.selected_id(), Some(3));
    }

    /// And the landing rule for a deletion still applies once the query has
    /// settled — one keystroke must not disable it for the rest of the session.
    #[test]
    fn a_deletion_after_a_query_change_still_lands_where_it_was() {
        let mut model = Model::new();
        model.set_query("a".to_owned());
        model.set_entries(vec![entry(1), entry(2), entry(3), entry(4)]);
        model.select_next();
        model.select_next();
        assert_eq!(model.selected_index(), Some(2));

        // Same query, one row gone: the deletion rule, not the requery rule.
        model.set_entries(vec![entry(1), entry(2), entry(4)]);

        assert_eq!(model.selected_id(), Some(4));
        assert_eq!(model.selected_index(), Some(2));
    }

    /// Re-setting the identical query happens on ordinary redraw paths and is
    /// not a fresh ranking.
    #[test]
    fn setting_the_same_query_again_does_not_move_the_highlight() {
        let mut model = model_with(&[1, 2, 3]);
        model.select_next();

        model.set_query(String::new());
        model.set_entries(vec![entry(1), entry(2), entry(3)]);

        assert_eq!(model.selected_id(), Some(2));
    }

    /// Which is why opening the picker says so itself. Clearing an already
    /// empty query is not a change, so without this the highlight would still
    /// be where the user left it last time — and `Enter` would copy that
    /// instead of the entry they just copied elsewhere.
    #[test]
    fn reopening_the_picker_puts_the_highlight_back_on_the_newest_entry() {
        let mut model = model_with(&[1, 2, 3]);
        model.select_next();
        model.select_next();
        assert_eq!(model.selected_id(), Some(3));

        // What `present` does: same (empty) query, fresh look.
        model.set_query(String::new());
        model.restart();
        model.set_entries(vec![entry(9), entry(1), entry(2), entry(3)]);

        assert_eq!(model.selected_id(), Some(9));
    }

    #[test]
    fn an_entry_key_is_the_id_and_the_capture_time() {
        let key = EntryKey::of(&entry(7));
        assert_eq!(
            key,
            EntryKey {
                id: 7,
                created_at: 7
            }
        );
    }

    /// The reason the key is a pair. SQLite reissues a deleted id to the next
    /// insert, so the same id can name two different entries over a session —
    /// and anything cached against it must not follow.
    #[test]
    fn a_reissued_id_is_a_different_key() {
        let mut screenshot = entry(42);
        screenshot.created_at = 1_000;
        let mut later_text = entry(42);
        later_text.created_at = 2_000;

        assert_ne!(EntryKey::of(&screenshot), EntryKey::of(&later_text));
    }
}
