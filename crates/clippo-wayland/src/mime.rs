//! The MIME types clippo bothers to capture.
//!
//! This is the single definition of "interesting" for the whole crate — call
//! sites ask [`is_interesting`] rather than matching MIME strings inline.

/// The marker flavor KeePassXC and Bitwarden attach to a copied credential.
///
/// It carries no useful payload of its own; `clippo-core` uses its *presence*
/// as one of the signals that an entry is sensitive (see DESIGN.md, "secret
/// detection"). clippo masks such entries rather than skipping them, so the
/// marker has to be captured alongside the real flavors.
pub const PASSWORD_MANAGER_HINT_MIME: &str = "x-kde-passwordManagerHint";

/// Every flavor clippo receives from a selection, and nothing else.
///
/// Verbatim from DESIGN.md, `clippo-wayland` → "Watch". Anything a source
/// advertises that is not in this list — `TIMESTAMP`, `SAVE_TARGETS`,
/// `text/rtf`, application-private types — is ignored: reading it would cost a
/// pipe and a copy for data nothing downstream can use.
pub const INTERESTING_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "text/html",
    "text/uri-list",
    "image/png",
    "image/jpeg",
    PASSWORD_MANAGER_HINT_MIME,
];

/// Whether a MIME type advertised by an offer is one clippo wants to receive.
///
/// Matching is case-insensitive and ignores whitespace, so the several spellings
/// of the UTF-8 text flavor that real applications advertise
/// (`text/plain;charset=utf-8`, `text/plain;charset=UTF-8`,
/// `text/plain; charset=utf-8`) all resolve to the same entry.
pub fn is_interesting(mime: &str) -> bool {
    let normalized = normalize(mime);
    INTERESTING_MIMES
        .iter()
        .any(|known| known.eq_ignore_ascii_case(&normalized))
}

/// Whether a MIME type is the password-manager marker.
pub fn is_password_manager_hint(mime: &str) -> bool {
    PASSWORD_MANAGER_HINT_MIME.eq_ignore_ascii_case(&normalize(mime))
}

/// Strip the whitespace that some toolkits put around MIME parameters.
fn normalize(mime: &str) -> String {
    mime.chars().filter(|c| !c.is_ascii_whitespace()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn design_list_is_captured_verbatim() {
        let from_design: &[&str] = &[
            "text/plain;charset=utf-8",
            "text/plain",
            "text/html",
            "text/uri-list",
            "image/png",
            "image/jpeg",
            "x-kde-passwordManagerHint",
        ];
        assert_eq!(INTERESTING_MIMES, from_design);
    }

    #[test]
    fn every_listed_mime_is_interesting() {
        for mime in INTERESTING_MIMES {
            assert!(is_interesting(mime), "{mime} should be interesting");
        }
    }

    #[test]
    fn uninteresting_flavors_are_rejected() {
        for mime in [
            "TIMESTAMP",
            "SAVE_TARGETS",
            "MULTIPLE",
            "text/rtf",
            "text/plain;charset=iso-8859-1",
            "image/gif",
            "image/png;clippo-thumb",
            "application/pdf",
            "application/x-kde4-slotlist",
            "",
        ] {
            assert!(!is_interesting(mime), "{mime} should not be interesting");
        }
    }

    #[test]
    fn matching_ignores_case_and_whitespace() {
        assert!(is_interesting("TEXT/PLAIN;CHARSET=UTF-8"));
        assert!(is_interesting("text/plain;charset=UTF-8"));
        assert!(is_interesting("text/plain; charset=utf-8"));
        assert!(is_interesting(" text/html "));
        assert!(is_interesting("Image/PNG"));
    }

    #[test]
    fn password_hint_is_recognised_and_interesting() {
        assert!(is_password_manager_hint(PASSWORD_MANAGER_HINT_MIME));
        assert!(is_password_manager_hint("x-kde-passwordmanagerhint"));
        assert!(is_interesting(PASSWORD_MANAGER_HINT_MIME));
        assert!(!is_password_manager_hint("text/plain"));
    }
}
