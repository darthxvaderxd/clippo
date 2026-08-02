//! Turning a captured selection into the one line a frontend shows.
//!
//! **This is the module M4 changes.** Masking a suspected secret is a change to
//! [`build`] and to nothing else, because [`build`] is the only place a preview
//! comes from and a preview is the only content [`crate::daemon`] puts in an
//! [`EntrySummary`][clippo_ipc::EntrySummary]. The full value leaves the daemon
//! through exactly one member, `Reveal`, which calls [`reveal`] below.
//!
//! Keeping those two functions side by side is the point of the module: a
//! reader can see at a glance that the masked path and the unmasked path are
//! different functions, and that only one of them is reachable from `List` and
//! `Search`.

use clippo_core::{EntryKind, Flavor};
use clippo_store::dedup;

/// How much of a copy a preview keeps, in characters.
///
/// Characters rather than bytes so that the count means the same thing for
/// `café` as for `cafe`, and so truncation can never split a code point. The
/// number is a display choice: long enough for a URL or a sentence, short
/// enough that a list of 500 previews stays a few hundred kilobytes and that a
/// stray whole-file copy does not become the row.
pub const PREVIEW_MAX_CHARS: usize = 120;

/// The character marking a preview that was cut short.
const ELLIPSIS: char = '\u{2026}';

/// The one-line rendering of a copy, as stored in `entries.preview`.
///
/// Built once at capture and stored, not recomputed per call: the list and the
/// search index both read it, and rebuilding it would mean loading every blob
/// in the history to answer a `List`.
///
/// Text is flattened — every run of whitespace becomes one space — so that a
/// multi-line copy is one row rather than a paragraph pushed into a panel, and
/// so that a fuzzy match against it is not defeated by a newline. An image has
/// no text to show, so it gets its type and size; the applet draws the stored
/// thumbnail instead.
pub fn build(kind: EntryKind, flavors: &[Flavor]) -> String {
    let Some(source) = preview_source(kind, flavors) else {
        // The caller has already refused a selection with no canonical flavor —
        // it has no hash and so no identity to store under. Reachable only if
        // that check is ever removed, and an empty preview is a better answer
        // there than a panic in the capture path.
        return String::new();
    };

    if kind == EntryKind::Image {
        return format!("{}, {}", source.mime, human_bytes(source.data.len() as u64));
    }

    truncate(&flatten(&String::from_utf8_lossy(&source.data)))
}

/// The flavor a preview is read from, which is not always the canonical one.
///
/// Dedup picks the *richest* flavor, because that is what identifies the copy:
/// a browser selection is one entry keyed on its `text/html`. A preview wants
/// the opposite — the most readable flavor — because `<span class="…">clippo`
/// is a worse row and a worse thing to fuzzy-match against than `clippo`. So
/// plain text wins here when the copy carries any, and the canonical flavor is
/// the fallback for a copy that does not.
///
/// Identity is unaffected: `entries.hash` is still BLAKE3 of the canonical
/// flavor. This only decides what is shown.
fn preview_source(kind: EntryKind, flavors: &[Flavor]) -> Option<&Flavor> {
    if kind != EntryKind::Image {
        if let Some(plain) = flavors
            .iter()
            .find(|flavor| essence(&flavor.mime) == "text/plain")
        {
            return Some(plain);
        }
    }
    dedup::canonical_flavor(kind, flavors)
}

/// A MIME type with its parameters stripped, trimmed and lowercased, so that
/// `text/plain; charset=UTF-8` is plain text. The same normalisation
/// `clippo-wayland` and `clippo-store` apply.
fn essence(mime: &str) -> String {
    let essence = match mime.split_once(';') {
        Some((essence, _parameters)) => essence,
        None => mime,
    };
    essence.trim().to_ascii_lowercase()
}

/// The whole stored value of a text entry, for `Reveal`.
///
/// `None` for an image: there is nothing to put in a D-Bus string, and the
/// blob is what the copy-back path offers rather than something a caller reads.
///
/// The only unmasked route out of the daemon, and deliberately a separate
/// function from [`build`] so it stays that way.
///
/// It reads the same flavor [`build`] previews, so revealing an entry shows the
/// whole of the thing the row showed part of. Reading the canonical flavor
/// instead would answer a masked `<b>clip…` with `clippo`, which is a different
/// string and looks like a bug.
pub fn reveal(kind: EntryKind, flavors: &[Flavor]) -> Option<String> {
    if kind == EntryKind::Image {
        return None;
    }
    preview_source(kind, flavors).map(|flavor| String::from_utf8_lossy(&flavor.data).into_owned())
}

/// Collapse every run of whitespace to a single space and trim the ends.
///
/// Control characters go with it: `\r`, `\t` and a stray `ESC` are all
/// whitespace or invisible, and a preview is written straight into a terminal
/// by the CLI. Anything else — including the bidi overrides `clippo-watch`
/// escapes — is left alone here; a preview is a value to render, not a value to
/// quote, and the frontend that renders it decides how.
fn flatten(text: &str) -> String {
    let mut flattened = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        if character.is_whitespace() || character.is_control() {
            pending_space = !flattened.is_empty();
            continue;
        }
        if pending_space {
            flattened.push(' ');
            pending_space = false;
        }
        flattened.push(character);
    }
    flattened
}

/// Cut to [`PREVIEW_MAX_CHARS`], marking that something was cut.
fn truncate(text: &str) -> String {
    let mut kept: String = text.chars().take(PREVIEW_MAX_CHARS).collect();
    if text.chars().nth(PREVIEW_MAX_CHARS).is_some() {
        kept.push(ELLIPSIS);
    }
    kept
}

/// A byte count for a human, at one decimal place.
///
/// Binary units under the units people expect to read: `KB` here is 1024
/// bytes, matching `max_image_bytes`, which is what the number is usually being
/// compared against.
fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    if bytes >= MIB {
        format!("{:.1} MB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(data: &str) -> Vec<Flavor> {
        vec![Flavor::new("text/plain;charset=utf-8", data)]
    }

    #[test]
    fn a_text_preview_is_the_copied_text() {
        assert_eq!(build(EntryKind::Text, &text("hello")), "hello");
    }

    #[test]
    fn a_multi_line_copy_previews_as_one_line() {
        assert_eq!(
            build(EntryKind::Text, &text("  one\n\ttwo\r\n\nthree  ")),
            "one two three"
        );
    }

    #[test]
    fn a_long_copy_is_cut_and_says_so() {
        let preview = build(EntryKind::Text, &text(&"x".repeat(500)));
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS + 1);
        assert!(preview.ends_with(ELLIPSIS), "{preview}");
    }

    /// Counting characters rather than bytes is what keeps this from splitting
    /// a code point, which `String::truncate` on a byte index would panic on.
    #[test]
    fn truncation_counts_characters_not_bytes() {
        let preview = build(EntryKind::Text, &text(&"é".repeat(500)));
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS + 1);
        assert!(preview.starts_with("éé"), "{preview}");
    }

    #[test]
    fn a_copy_of_exactly_the_limit_is_not_marked_as_cut() {
        let preview = build(EntryKind::Text, &text(&"x".repeat(PREVIEW_MAX_CHARS)));
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS);
        assert!(!preview.ends_with(ELLIPSIS), "{preview}");
    }

    /// The richest flavor decides *identity*; the most readable one decides the
    /// preview. A browser copy is an `html` entry that previews as its text.
    #[test]
    fn an_html_copy_previews_from_its_plain_text_flavor() {
        let flavors = vec![
            Flavor::new("text/html", "<b>clippo</b>"),
            Flavor::new("text/plain;charset=utf-8", "clippo"),
        ];
        assert_eq!(build(EntryKind::Html, &flavors), "clippo");
    }

    /// …and falls back to the canonical flavor when the copy carries no plain
    /// text at all.
    #[test]
    fn an_html_copy_with_no_text_flavor_previews_from_the_markup() {
        let flavors = vec![Flavor::new("text/html", "<b>clippo</b>")];
        assert_eq!(build(EntryKind::Html, &flavors), "<b>clippo</b>");
    }

    #[test]
    fn an_image_previews_as_its_type_and_size() {
        let flavors = vec![Flavor::new("image/png", vec![0_u8; 2048])];
        assert_eq!(build(EntryKind::Image, &flavors), "image/png, 2.0 KB");
    }

    #[test]
    fn a_selection_with_no_canonical_flavor_previews_as_nothing() {
        let flavors = vec![Flavor::new("x-kde-passwordManagerHint", "secret")];
        assert_eq!(build(EntryKind::Text, &flavors), String::new());
    }

    #[test]
    fn reveal_returns_the_whole_value_where_the_preview_was_cut() {
        let whole = "y".repeat(500);
        let flavors = text(&whole);
        assert_eq!(reveal(EntryKind::Text, &flavors).unwrap(), whole);
        assert!(build(EntryKind::Text, &flavors).len() < whole.len());
    }

    /// Reveal keeps the value exactly as copied — no flattening, no trimming.
    /// A revealed password with its newlines eaten would paste as the wrong
    /// thing when the user hand-copied it out of the CLI.
    #[test]
    fn reveal_does_not_flatten_whitespace() {
        assert_eq!(
            reveal(EntryKind::Text, &text("-----BEGIN\nkey\n-----END\n")).unwrap(),
            "-----BEGIN\nkey\n-----END\n"
        );
    }

    #[test]
    fn an_image_has_nothing_to_reveal() {
        let flavors = vec![Flavor::new("image/png", vec![0_u8; 16])];
        assert!(reveal(EntryKind::Image, &flavors).is_none());
    }

    #[test]
    fn byte_counts_read_the_way_max_image_bytes_is_written() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(8 * 1024 * 1024), "8.0 MB");
    }
}
