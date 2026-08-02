//! Rule 1 — the password-manager MIME marker.
//!
//! The highest-confidence signal clippo has, and the only one that is not a
//! guess: the application that owns the copy is *telling* us it is a
//! credential. KeePassXC and Bitwarden both attach the marker; so does anything
//! else following the KDE convention.
//!
//! Only the capture path can see this. By the time an entry is in the database
//! the marker flavor may not even have been stored — it carries no payload
//! worth keeping — which is why detection happens once, at capture, and the
//! answer is written to `entries.sensitive`.
//!
//! **The marker masks, it does not skip.** DESIGN.md is explicit: a skipped
//! password is a silently missing clipboard entry, which is worse UX than a
//! masked one that still pastes correctly.

/// The marker flavor KeePassXC and Bitwarden attach to a copied credential.
///
/// `clippo-wayland` re-exports this rather than defining a second copy: the
/// watcher needs it to know the flavor is worth receiving, and this module
/// needs it to know what the flavor means, and those must be the same string.
pub const PASSWORD_MANAGER_HINT_MIME: &str = "x-kde-passwordManagerHint";

/// Whether one MIME type is the password-manager marker.
///
/// Case-insensitive, and tolerant of the whitespace some toolkits leave around
/// MIME parameters, for the same reason the rest of clippo's MIME handling is:
/// the several spellings a real application might send all mean one thing.
pub fn is_password_manager_hint(mime: &str) -> bool {
    PASSWORD_MANAGER_HINT_MIME.eq_ignore_ascii_case(normalize(mime).as_str())
}

/// Whether this rule fires for a selection advertising these MIME types.
///
/// Pass every MIME type the selection mentioned — both what it advertised and
/// what was actually received. The marker's *presence* is the signal, so it
/// still counts when the marker flavor itself was advertised but never read.
pub fn fires<'a>(mimes: impl IntoIterator<Item = &'a str>) -> bool {
    mimes.into_iter().any(is_password_manager_hint)
}

/// Strip the whitespace around a MIME type and its parameters.
fn normalize(mime: &str) -> String {
    mime.split(';')
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(";")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_is_recognised_however_it_is_spelled() {
        assert!(is_password_manager_hint(PASSWORD_MANAGER_HINT_MIME));
        assert!(is_password_manager_hint("x-kde-passwordmanagerhint"));
        assert!(is_password_manager_hint("X-KDE-PASSWORDMANAGERHINT"));
        assert!(is_password_manager_hint("  x-kde-passwordManagerHint  "));
    }

    #[test]
    fn an_ordinary_flavor_is_not_the_marker() {
        assert!(!is_password_manager_hint("text/plain"));
        assert!(!is_password_manager_hint(""));
        assert!(!is_password_manager_hint("x-kde-passwordManagerHint-ish"));
    }

    #[test]
    fn the_rule_fires_on_any_of_the_selections_mime_types() {
        assert!(fires(["text/plain", PASSWORD_MANAGER_HINT_MIME]));
        assert!(!fires(["text/plain", "text/html"]));
        assert!(!fires(std::iter::empty()));
    }
}
