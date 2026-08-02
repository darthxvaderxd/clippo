//! The previews the daemon keeps in memory, and the fuzzy search over them.
//!
//! DESIGN.md, `clippo-store` → Search:
//!
//! > No FTS5. The daemon keeps previews in memory and matches with
//! > `nucleo-matcher` (the fuzzy matcher COSMIC's own launcher uses). Better UX
//! > than `LIKE`, and it sidesteps building FTS5 against SQLCipher.
//!
//! So the cache is not an optimisation, it is where search happens. What that
//! buys, beyond avoiding an FTS5-on-SQLCipher build: the previews are already
//! decrypted, so typing in the applet does not decrypt the whole history on
//! every keystroke.
//!
//! # Staying honest
//!
//! A search index that disagrees with the database shows a user rows that are
//! not there, and the row they click is gone by the time they click it. This
//! cache therefore holds *every* entry, not a page of them, and is
//! [`replace`](PreviewCache::replace)d wholesale from the store after every
//! mutation rather than patched.
//!
//! Patching would be faster and would be wrong. `Store::insert` runs retention
//! inside its own transaction and reports how many rows it wrote, not which
//! rows it evicted, so an incremental update has no way to learn that a copy
//! pushed the oldest entry out. The drift would be silent, and it would be in
//! the direction that shows deleted clipboard content. A `SELECT` of a few
//! hundred previews is microseconds; correctness here is worth more than they
//! cost.
//!
//! The size is bounded by retention — `max_entries`, 500 by default — and each
//! entry is capped at [`PREVIEW_MAX_CHARS`][crate::preview::PREVIEW_MAX_CHARS].

use clippo_core::Entry;
use clippo_ipc::EntrySummary;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config as MatcherConfig, Matcher, Utf32Str};

/// Every entry in the history as a summary, newest use first.
pub struct PreviewCache {
    /// In `Store::list` order — `last_used_at DESC, id DESC` — so a page of
    /// this is a page of the history.
    entries: Vec<EntrySummary>,
    /// Reused across searches. `nucleo-matcher`'s own docs ask for this: the
    /// matcher owns the scratch matrices the algorithm needs, and building one
    /// per keystroke would allocate them again every time.
    matcher: Matcher,
    /// Scratch for [`Utf32Str::new`], reused for the same reason.
    haystack: Vec<char>,
}

impl PreviewCache {
    /// An empty cache. [`PreviewCache::replace`] fills it from the store.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            matcher: Matcher::new(MatcherConfig::DEFAULT),
            haystack: Vec::new(),
        }
    }

    /// Take the store's view of the history as the truth, wholesale.
    pub fn replace(&mut self, entries: Vec<Entry>) {
        self.entries = entries.into_iter().map(summarize).collect();
    }

    /// How many entries are cached, which is how many the store holds.
    pub fn entry_count(&self) -> usize {
        self.entries.len()
    }

    /// One page of the history. A `limit` of 0 means "all of it".
    ///
    /// An `offset` past the end is an empty page, not an error: a frontend
    /// paging through a history that shrank under it should see the end of the
    /// list, not a fault.
    pub fn page(&self, limit: usize, offset: usize) -> Vec<EntrySummary> {
        let from = offset.min(self.entries.len());
        let rest = &self.entries[from..];
        let take = if limit == 0 { rest.len() } else { limit };
        rest.iter().take(take).cloned().collect()
    }

    /// The entries whose preview fuzzy-matches `query`, best match first.
    ///
    /// An empty query matches everything, so `Search("")` is the history in
    /// recency order — which is what an applet wants for the first frame of a
    /// popup whose search field has not been typed into yet.
    ///
    /// Ties keep recency order. The sort is stable and the input is already in
    /// `last_used_at` order, so two equally good matches come back with the
    /// more recently used one first, which is nearly always the one meant.
    pub fn search(&mut self, query: &str, limit: usize) -> Vec<EntrySummary> {
        // `Smart` case matching is case-insensitive until the query contains an
        // uppercase character, at which point it becomes sensitive — the
        // behaviour every editor's quick-open has trained people to expect.
        let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);

        let mut scored: Vec<(u32, &EntrySummary)> = self
            .entries
            .iter()
            .filter_map(|entry| {
                let haystack = Utf32Str::new(&entry.preview, &mut self.haystack);
                pattern
                    .score(haystack, &mut self.matcher)
                    .map(|score| (score, entry))
            })
            .collect();
        scored.sort_by(|(left, _), (right, _)| right.cmp(left));

        let take = if limit == 0 { scored.len() } else { limit };
        scored
            .into_iter()
            .take(take)
            .map(|(_, entry)| entry.clone())
            .collect()
    }
}

impl Default for PreviewCache {
    fn default() -> Self {
        Self::new()
    }
}

/// An [`Entry`] as a frontend sees it.
///
/// Everything but `hash`, which is an unsalted hash of the copied bytes and
/// stays inside the encrypted database. See [`EntrySummary`].
fn summarize(entry: Entry) -> EntrySummary {
    EntrySummary {
        id: entry.id.get(),
        created_at: entry.created_at.as_unix_millis(),
        last_used_at: entry.last_used_at.as_unix_millis(),
        kind: entry.kind.as_str().to_owned(),
        preview: entry.preview,
        pinned: entry.pinned,
        sensitive: entry.sensitive,
    }
}

#[cfg(test)]
mod tests {
    use clippo_core::{EntryId, EntryKind, Timestamp};

    use super::*;

    /// Entries as the store hands them over: newest use first.
    fn history(previews: &[&str]) -> Vec<Entry> {
        previews
            .iter()
            .enumerate()
            .map(|(index, preview)| Entry {
                id: EntryId::new(index as i64 + 1),
                created_at: Timestamp::from_unix_millis(1_000 - index as i64),
                last_used_at: Timestamp::from_unix_millis(1_000 - index as i64),
                kind: EntryKind::Text,
                preview: (*preview).to_owned(),
                hash: format!("{index:064x}"),
                pinned: false,
                sensitive: false,
            })
            .collect()
    }

    fn cache(previews: &[&str]) -> PreviewCache {
        let mut cache = PreviewCache::new();
        cache.replace(history(previews));
        cache
    }

    fn previews(entries: &[EntrySummary]) -> Vec<&str> {
        entries.iter().map(|entry| entry.preview.as_str()).collect()
    }

    #[test]
    fn a_page_keeps_the_stores_order() {
        let cache = cache(&["newest", "middle", "oldest"]);
        assert_eq!(previews(&cache.page(2, 0)), ["newest", "middle"]);
        assert_eq!(previews(&cache.page(2, 1)), ["middle", "oldest"]);
        assert_eq!(previews(&cache.page(0, 0)), ["newest", "middle", "oldest"]);
    }

    #[test]
    fn a_page_past_the_end_is_empty_rather_than_a_failure() {
        let cache = cache(&["one"]);
        assert!(cache.page(10, 5).is_empty());
    }

    #[test]
    fn search_is_fuzzy_rather_than_a_substring_match() {
        let mut cache = cache(&["cargo build --workspace", "git commit --amend"]);
        assert_eq!(
            previews(&cache.search("crgbld", 10)),
            ["cargo build --workspace"],
            "a subsequence with gaps should match, which LIKE would not"
        );
    }

    #[test]
    fn search_filters_out_what_does_not_match_at_all() {
        let mut cache = cache(&["hello world", "goodbye"]);
        assert_eq!(previews(&cache.search("hello", 10)), ["hello world"]);
        assert!(cache.search("zzzz", 10).is_empty());
    }

    /// Ranking is by how good the match is, not by where the entry sits in the
    /// history: a run of the query's characters together beats the same
    /// characters scattered across a line, even when the scattered one is more
    /// recent.
    ///
    /// Note what this does *not* claim. `nucleo` scores the match, not the
    /// haystack, so two entries that both contain `clippo` at a word boundary
    /// score the same however long they are, and recency breaks that tie.
    #[test]
    fn search_ranks_a_close_match_above_a_scattered_one() {
        let mut cache = cache(&["cargo lint --package portal", "clippo"]);
        assert_eq!(
            previews(&cache.search("clippo", 10)),
            ["clippo", "cargo lint --package portal"]
        );
    }

    /// The input is in recency order and the sort is stable, so equally good
    /// matches stay in recency order.
    #[test]
    fn equally_good_matches_keep_recency_order() {
        let mut cache = cache(&["match", "match", "match"]);
        let found = cache.search("match", 10);
        assert_eq!(
            found.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }

    #[test]
    fn an_empty_query_is_the_whole_history_in_order() {
        let mut cache = cache(&["newest", "middle", "oldest"]);
        assert_eq!(
            previews(&cache.search("", 0)),
            ["newest", "middle", "oldest"]
        );
    }

    #[test]
    fn search_honours_its_limit() {
        let mut cache = cache(&["one match", "two match", "three match"]);
        assert_eq!(cache.search("match", 2).len(), 2);
        assert_eq!(cache.search("match", 0).len(), 3);
    }

    #[test]
    fn search_is_case_insensitive_until_the_query_is_not() {
        let mut cache = cache(&["Cargo.toml", "cargo.lock"]);
        assert_eq!(cache.search("cargo", 10).len(), 2);
        assert_eq!(previews(&cache.search("Cargo", 10)), ["Cargo.toml"]);
    }

    #[test]
    fn replacing_the_cache_forgets_what_the_store_no_longer_has() {
        let mut cache = cache(&["gone", "kept"]);
        cache.replace(history(&["kept"]));
        assert_eq!(cache.entry_count(), 1);
        assert!(cache.search("gone", 10).is_empty());
    }
}
