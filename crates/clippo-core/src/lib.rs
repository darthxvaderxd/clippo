//! Shared clippo types: clipboard entries and their flavors, configuration, and
//! the secret detection + masking rules.
//!
//! Placeholder — filled in at M4 (secrets); see `docs/ROADMAP.md`.

/// Crate name, exposed so the placeholder has a testable surface.
pub const NAME: &str = "clippo-core";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_set() {
        assert_eq!(NAME, "clippo-core");
    }
}
