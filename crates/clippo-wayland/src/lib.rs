//! Hand-rolled `ext_data_control_v1` / `zwlr_data_control_v1` client: watches
//! selections and re-offers stored entries.
//!
//! Placeholder — filled in at M1 (capture); see `docs/ROADMAP.md`.

/// Crate name, exposed so the placeholder has a testable surface.
pub const NAME: &str = "clippo-wayland";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_set() {
        assert_eq!(NAME, "clippo-wayland");
    }
}
