//! The `com.nilfactor.Clippo` D-Bus surface: zbus proxy and interface definitions
//! shared by the daemon, the CLI and the applet.
//!
//! Placeholder — filled in at M3 (daemon + CLI); see `docs/ROADMAP.md`.

/// Well-known bus name of the clippo daemon.
pub const BUS_NAME: &str = "com.nilfactor.Clippo";

/// Object path the daemon exports its interface on.
pub const OBJECT_PATH: &str = "/com/nilfactor/Clippo";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_path_matches_bus_name() {
        assert_eq!(OBJECT_PATH, format!("/{}", BUS_NAME.replace('.', "/")));
    }
}
