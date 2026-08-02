//! The self-echo guard: how clippo tells its own copy-back apart from a copy.
//!
//! DESIGN.md's risk table:
//!
//! > **Self-echo loop** — a wrong hash guard re-enters every copy-back into
//! > history → *Integration test at M3.*
//!
//! Taking the clipboard makes the compositor announce the new selection to
//! every data-control client, clippo included. So `Copy(2)` is always followed
//! by a capture of entry 2's own flavors, and a daemon that stored it would
//! bump `last_used_at` a second time for a use that never happened, on top of
//! the bump `Copy` already made.
//!
//! # What it is keyed on
//!
//! The **entry hash** — BLAKE3 of the canonical flavor, `clippo-store`'s
//! `dedup::hash`. That is not merely *a* hash of the content; it is the exact
//! identity a capture is deduplicated by, so "this capture would land on the
//! entry we just offered" and "this capture matches the guard" are the same
//! question asked once. Guarding on anything else — the flavor list, the byte
//! count — could disagree with dedup and let an echo through, or block a copy
//! that was not one.
//!
//! It also means the guard survives the round trip losing flavors. Clippo
//! offers every stored flavor except the derived thumbnail, and captures back
//! only the *interesting* ones; the canonical flavor is in both sets by
//! construction, so the hash is the same at both ends.
//!
//! This is why the guard lives here rather than in `clippo-wayland`, where
//! DESIGN.md's component list puts it: the hash it matches on is the store's,
//! and the Wayland crate would have to depend on the whole encrypted store to
//! compute it.
//!
//! # Both ways of getting it wrong
//!
//! - **Too weak** and every copy-back re-enters the history — the named risk.
//! - **Too strong** and a permanent "ignore this hash" makes a *deliberate*
//!   re-copy of the same text vanish silently, which is worse, because nothing
//!   in the UI says why.
//!
//! So the guard is armed for exactly one capture. It is spent whether it
//! matched or not ([`EchoGuard::is_echo`]), and it is cleared outright when
//! another application takes the clipboard, because after that the echo it was
//! armed for is never coming.
//!
//! One slot, deliberately, which leaves a corner: two `Copy` calls in quick
//! succession arm twice, and the *first* echo spends the guard the second one
//! wanted. What gets through is a capture of the entry `Copy` has just put on
//! the clipboard, so it deduplicates onto that same entry and bumps a
//! `last_used_at` the same call had already set moments earlier. No duplicate
//! row, no loop — which is why a second slot is not worth the state.

use tracing::debug;

/// One armed guard against clippo's own copy-back, or nothing.
#[derive(Debug, Default)]
pub struct EchoGuard {
    /// The entry hash the next capture is allowed to be, if any.
    armed: Option<String>,
}

impl EchoGuard {
    /// Expect one capture of the entry with this hash, and ignore it.
    ///
    /// Must be called **before** the flavors go to the compositor: the echo can
    /// arrive on the capture channel while the `Copy` call that caused it is
    /// still running.
    pub fn arm(&mut self, hash: &str) {
        self.armed = Some(hash.to_owned());
    }

    /// Whether this capture is the copy-back we armed for.
    ///
    /// Spends the guard either way. A capture that is *not* ours means some
    /// other application got in first, and the echo we were waiting for will
    /// never arrive — leaving the guard armed for it is what would turn a
    /// one-shot into the permanent denylist described above.
    pub fn is_echo(&mut self, hash: &str) -> bool {
        match self.armed.take() {
            Some(armed) => armed == hash,
            None => false,
        }
    }

    /// Forget an armed guard, reporting whether there was one.
    ///
    /// Called when clippo loses the selection: the compositor has given the
    /// clipboard to somebody else, so nothing is coming back.
    pub fn clear(&mut self) -> bool {
        self.armed.take().is_some()
    }

    /// Whether a copy-back is currently expected back.
    #[cfg(test)]
    pub fn is_armed(&self) -> bool {
        self.armed.is_some()
    }
}

impl Drop for EchoGuard {
    fn drop(&mut self) {
        if self.armed.is_some() {
            // Only reachable if the daemon shuts down between a `Copy` and its
            // echo, which is harmless — but a guard that was still waiting is
            // worth a line when someone is reading the journal after one.
            debug!("clippo shut down still expecting its own copy-back back");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HUNTER2: &str = "aaaa1111";
    const SOMETHING_ELSE: &str = "bbbb2222";

    #[test]
    fn an_unarmed_guard_ignores_nothing() {
        let mut guard = EchoGuard::default();
        assert!(!guard.is_armed());
        assert!(!guard.is_echo(HUNTER2));
    }

    /// The named risk: a copy-back must not come back round as a capture.
    #[test]
    fn an_armed_guard_swallows_the_matching_capture() {
        let mut guard = EchoGuard::default();
        guard.arm(HUNTER2);
        assert!(guard.is_armed());
        assert!(guard.is_echo(HUNTER2));
    }

    /// The other direction, and the one that is easy to get wrong: copying the
    /// same thing again by hand is a real copy and must register.
    #[test]
    fn the_guard_is_spent_after_one_capture_rather_than_being_a_denylist() {
        let mut guard = EchoGuard::default();
        guard.arm(HUNTER2);

        assert!(guard.is_echo(HUNTER2), "the copy-back");
        assert!(!guard.is_armed(), "one echo is all it was armed for");
        assert!(!guard.is_echo(HUNTER2), "the same text, copied by hand");
        assert!(!guard.is_echo(HUNTER2), "and again");
    }

    /// A capture that is not ours means somebody else took the clipboard, so
    /// the echo is never coming and the guard must not sit armed waiting for it.
    #[test]
    fn a_capture_that_is_not_the_echo_still_spends_the_guard() {
        let mut guard = EchoGuard::default();
        guard.arm(HUNTER2);

        assert!(!guard.is_echo(SOMETHING_ELSE));
        assert!(!guard.is_armed());
        assert!(
            !guard.is_echo(HUNTER2),
            "a later copy of the armed content is a real copy, not a stale echo"
        );
    }

    #[test]
    fn losing_the_selection_clears_an_armed_guard_and_says_it_did() {
        let mut guard = EchoGuard::default();
        assert!(!guard.clear(), "there was nothing to clear");

        guard.arm(HUNTER2);
        assert!(guard.clear());
        assert!(!guard.is_armed());
        assert!(!guard.is_echo(HUNTER2));
    }

    /// A second `Copy` before the first echo arrived. The one slot holds the
    /// newer copy-back — the one actually on the clipboard — and the older
    /// echo spends it without matching. Pinned because it is the documented
    /// corner rather than an accident: what gets through afterwards is a
    /// capture of the entry `Copy` just touched anyway.
    #[test]
    fn arming_twice_keeps_only_the_more_recent_copy_back() {
        let mut guard = EchoGuard::default();
        guard.arm(HUNTER2);
        guard.arm(SOMETHING_ELSE);

        assert!(!guard.is_echo(HUNTER2), "the older echo no longer matches");
        assert!(!guard.is_armed(), "and it spent the guard on its way past");
    }
}
