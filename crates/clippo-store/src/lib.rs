//! Encrypted clipboard history: SQLCipher-backed entries and flavor blobs, with
//! dedup and retention.
//!
//! Placeholder — filled in at M2 (storage); see `docs/ROADMAP.md`.

/// Crate name, exposed so the placeholder has a testable surface.
pub const NAME: &str = "clippo-store";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_set() {
        assert_eq!(NAME, "clippo-store");
    }
}
