//! Rendering a preview somewhere a human will look at it.
//!
//! # Previews are hostile input
//!
//! A preview is whatever the user last copied, and a user copies things from
//! web pages and from other people's terminals. A copied `ESC [ 2 J` clears a
//! terminal when it is printed; a copied `U+202E` reverses the display order of
//! everything after it, so a row can claim to be one entry while another id
//! sits under the cursor. Neither hazard is the terminal's alone — the bidi
//! overrides reorder a line of shaped text in a GUI list exactly as they do a
//! table column, which is why this lives in the crate every frontend already
//! depends on rather than in one of them.
//!
//! Three frontends escape the same set for the same reason: `clippo list`'s
//! table and `--json`, `clippo-watch`'s flavor dump, and the applet's rows.
//! [`one_line`] is the whole rendering for the first and the last; the middle
//! one quotes differently and takes [`is_invisible_or_reordering`] on its own.
//!
//! `clippo reveal` is the one deliberate exception, and says so in its
//! `--help`. It exists to be redirected, and a value that came back altered
//! would be the wrong value.

/// The character marking a preview cut short, as the daemon uses.
pub const ELLIPSIS: char = '\u{2026}';

/// One line of display-safe text, at most `max_chars` characters long.
///
/// Whitespace — newlines included — collapses to single spaces, so a copied
/// paragraph is one row. Anything else that would be *acted* on rather than
/// shown becomes a visible `\u{1b}` escape: control characters, and the
/// invisible and reordering formatting characters that `char::is_control` does
/// not cover.
///
/// Counting is on characters written, not characters read, so an entry made
/// entirely of escape sequences cannot push the row past the width it was
/// given.
#[must_use]
pub fn one_line(text: &str, max_chars: usize) -> String {
    // No column at all is no output. Worth stating rather than falling out of
    // the loop below, where the trimming that follows a cut would spin: `pop`
    // on an empty string reports nothing to do without making progress.
    if max_chars == 0 {
        return String::new();
    }

    let mut out = String::with_capacity(text.len().min(max_chars));
    let mut written = 0_usize;
    let mut pending_space = false;
    let mut cut = false;

    for character in text.chars() {
        if character.is_whitespace() {
            // Leading whitespace is dropped rather than turned into a space:
            // the column starts where the content does.
            pending_space = written > 0;
            continue;
        }

        let mut piece = [0_u8; 4];
        let escaped;
        let piece: &str = if character.is_control() || is_invisible_or_reordering(character) {
            escaped = format!("\\u{{{:x}}}", character as u32);
            &escaped
        } else {
            character.encode_utf8(&mut piece)
        };

        let space = usize::from(pending_space);
        if written + space + piece.chars().count() > max_chars {
            cut = true;
            break;
        }
        if pending_space {
            out.push(' ');
            written += 1;
            pending_space = false;
        }
        out.push_str(piece);
        written += piece.chars().count();
    }

    if cut {
        // Trailing whitespace before the cut is already gone: `pending_space`
        // was never written.
        while out.chars().count() >= max_chars {
            out.pop();
        }
        out.push(ELLIPSIS);
    }
    out
}

/// Characters `char::is_control` misses that a reader is still misled by.
///
/// `is_control` is category Cc only, while the bidi overrides and isolates are
/// Cf and visually reorder everything after them — which in a table means the
/// id column and the row's own preview can be made to swap places, and in the
/// applet means the row a user is about to press Enter on can be made to read
/// as a different one. The zero-width characters are here because "invisible in
/// a preview" defeats the preview, and the tag block for the same reason at its
/// most deliberate: a copy can carry a whole ASCII message in characters
/// nothing renders.
///
/// Hand-listed rather than "every Cf", which would want a Unicode table
/// dependency. This is the subset a reader is actually misled by.
#[must_use]
pub fn is_invisible_or_reordering(character: char) -> bool {
    matches!(character,
        '\u{00ad}'                // soft hyphen
        | '\u{061c}'              // arabic letter mark
        | '\u{180e}'              // mongolian vowel separator
        | '\u{200b}'..='\u{200f}' // zero-width space … RTL mark
        | '\u{202a}'..='\u{202e}' // bidi embeddings and overrides
        | '\u{2060}'..='\u{206f}' // word joiner, isolates, deprecated formatting
        | '\u{feff}'              // zero-width no-break space / BOM
        | '\u{fff9}'..='\u{fffb}' // interlinear annotation
        | '\u{e0000}'..='\u{e007f}' // language tags: invisible, and ASCII-shaped
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_multi_line_preview_becomes_one_line() {
        assert_eq!(one_line("  one\ntwo\r\n\tthree  ", 40), "one two three");
    }

    /// The whole point of the module: a copied escape sequence is shown, not
    /// executed.
    #[test]
    fn control_characters_are_escaped_rather_than_printed() {
        let rendered = one_line("clear:\u{1b}[2J\u{7}", 40);
        assert_eq!(rendered, "clear:\\u{1b}[2J\\u{7}");
        assert!(!rendered.contains('\u{1b}'));
    }

    /// `char::is_control` does not cover these, and a right-to-left override
    /// reorders the row it sits in — including the id somebody is about to type
    /// at `clippo rm`, or the preview they are about to press Enter on.
    #[test]
    fn reordering_and_invisible_characters_are_escaped_too() {
        assert_eq!(one_line("a\u{202e}b", 40), "a\\u{202e}b");
        assert_eq!(one_line("a\u{200b}b", 40), "a\\u{200b}b");
        assert_eq!(one_line("a\u{feff}b", 40), "a\\u{feff}b");
        // The tag block: a message in characters that render as nothing at all.
        assert_eq!(one_line("a\u{e0041}b", 40), "a\\u{e0041}b");
    }

    /// Not reachable from the CLI, whose callers all pass a constant, but the
    /// trimming after a cut is a `pop` loop and a zero-width column is the one
    /// input that makes it go round without shortening anything.
    #[test]
    fn a_column_no_characters_wide_is_no_output_rather_than_a_hang() {
        assert_eq!(one_line("anything at all", 0), "");
    }

    #[test]
    fn ordinary_text_is_left_alone() {
        assert_eq!(one_line("naïve café — 100%", 40), "naïve café — 100%");
    }

    #[test]
    fn a_long_preview_is_cut_and_says_so() {
        let rendered = one_line(&"x".repeat(100), 10);
        assert_eq!(rendered, "xxxxxxxxx\u{2026}");
        assert_eq!(rendered.chars().count(), 10);
    }

    /// Counting output characters is what stops an entry made of escape
    /// sequences from being 60 characters of source and 400 of column.
    #[test]
    fn escaping_cannot_push_a_row_past_the_column_width() {
        let rendered = one_line(&"\u{1b}".repeat(20), 16);
        assert!(rendered.chars().count() <= 16, "{rendered}");
        assert!(rendered.ends_with(ELLIPSIS), "{rendered}");
    }

    #[test]
    fn a_preview_of_exactly_the_width_is_not_marked_as_cut() {
        assert_eq!(one_line("0123456789", 10), "0123456789");
    }

    /// A mask is neither a control character nor an invisible one, so it
    /// reaches every frontend as the bullets the daemon stored.
    #[test]
    fn a_mask_passes_through_untouched() {
        let masked = "su\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}ue";
        assert_eq!(one_line(masked, 40), masked);
    }
}
