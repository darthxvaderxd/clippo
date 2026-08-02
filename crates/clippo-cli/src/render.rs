//! Turning `EntrySummary` values into something safe to put in a terminal.
//!
//! # Previews are hostile input
//!
//! A preview is whatever the user last copied, and a user copies things from
//! web pages and from other people's terminals. A copied `ESC [ 2 J` clears the
//! screen when it is printed; a copied `U+202E` reverses the display order of
//! everything after it, so a row can claim to be one entry while another id
//! sits under the cursor. The daemon already flattens whitespace when it builds
//! a preview, but this module does not rely on that: everything printed goes
//! through [`one_line`], which is written to be safe on arbitrary input so that
//! it stays safe if the daemon's preview rules change.
//!
//! [`crate::cli::Command::Reveal`] is the deliberate exception, and says so in
//! its `--help`. It exists to be redirected, and a value that came back
//! altered would be the wrong value.

use clippo_core::Timestamp;
use clippo_ipc::EntrySummary;

use crate::cli::PREVIEW_COLUMN_CHARS;
use crate::error::CliError;

/// The character marking a preview cut short, as the daemon uses.
const ELLIPSIS: char = '\u{2026}';

/// The history as a table: id, age, kind, flags, preview.
///
/// Returns the empty string for no entries; the caller says "nothing here" on
/// stderr rather than printing a header with no rows under it.
pub fn table(entries: &[EntrySummary], now: Timestamp) -> String {
    if entries.is_empty() {
        return String::new();
    }

    let rows: Vec<[String; 5]> = entries
        .iter()
        .map(|entry| {
            [
                entry.id.to_string(),
                age(now, entry.last_used_at),
                one_line(&entry.kind, KIND_CHARS),
                flags(entry),
                one_line(&entry.preview, PREVIEW_COLUMN_CHARS),
            ]
        })
        .collect();

    const HEADERS: [&str; 5] = ["ID", "AGE", "KIND", "FL", "PREVIEW"];
    // Right-align the two numeric-ish columns so ids and ages line up on their
    // last digit; everything else reads left to right.
    const RIGHT: [bool; 5] = [true, true, false, false, false];

    let widths: Vec<usize> = (0..HEADERS.len())
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .chain(std::iter::once(HEADERS[column].chars().count()))
                .max()
                .unwrap_or(0)
        })
        .collect();

    let mut out = String::new();
    write_row(&mut out, &HEADERS.map(str::to_owned), &widths, &RIGHT);
    for row in &rows {
        write_row(&mut out, row, &widths, &RIGHT);
    }
    out
}

/// How much of a kind word a column shows. `image` is the longest the daemon
/// sends; the cap is only here so a daemon that grew a new one cannot widen the
/// table without bound.
const KIND_CHARS: usize = 12;

/// Two columns between fields — one space reads as a typo next to a
/// right-aligned number.
const GUTTER: &str = "  ";

fn write_row(out: &mut String, cells: &[String; 5], widths: &[usize], right: &[bool; 5]) {
    for (column, cell) in cells.iter().enumerate() {
        if column > 0 {
            out.push_str(GUTTER);
        }
        let padding = widths[column].saturating_sub(cell.chars().count());
        // The last column is never padded: trailing spaces on every row are
        // invisible until somebody copies one back out.
        let last = column + 1 == cells.len();
        if right[column] {
            for _ in 0..padding {
                out.push(' ');
            }
            out.push_str(cell);
        } else {
            out.push_str(cell);
            if !last {
                for _ in 0..padding {
                    out.push(' ');
                }
            }
        }
    }
    out.push('\n');
}

/// The `FL` column: `p` for pinned, `s` for suspected secret, `.` for neither.
///
/// Two fixed characters rather than words, so the column never moves when a
/// row below it happens to be pinned. `clippo list --help` says what they are.
fn flags(entry: &EntrySummary) -> String {
    let mut flags = String::with_capacity(2);
    flags.push(if entry.pinned { 'p' } else { '.' });
    flags.push(if entry.sensitive { 's' } else { '.' });
    flags
}

/// The same entries as JSON, for scripts.
///
/// Every field of the D-Bus type, under the same names, so a script reads the
/// daemon's answer rather than this table's rendering of it: timestamps stay
/// Unix milliseconds rather than becoming "3m", and the preview is the whole
/// one rather than the column's share of it.
///
/// JSON escaping is what makes this safe to emit unsanitised — a control
/// character comes out as ``, which no terminal acts on. A script that
/// decodes it and prints the result is back to handling hostile bytes, exactly
/// as it would be with any other clipboard tool.
pub fn json(entries: &[EntrySummary]) -> Result<String, CliError> {
    let mut json = serde_json::to_string_pretty(entries).map_err(CliError::Json)?;
    json.push('\n');
    Ok(json)
}

/// How long ago, in the widest unit that gives a whole number.
///
/// One or two characters plus a unit, because the column exists to answer "is
/// this the thing I copied a moment ago?" and nothing finer. A timestamp in
/// the future — a clock that moved backwards — reads as `0s` rather than as a
/// negative age, which is what [`Timestamp::since`] saturates to.
pub fn age(now: Timestamp, last_used_at: i64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;

    let seconds = now
        .since(Timestamp::from_unix_millis(last_used_at))
        .as_secs();
    if seconds < MINUTE {
        format!("{seconds}s")
    } else if seconds < HOUR {
        format!("{}m", seconds / MINUTE)
    } else if seconds < DAY {
        format!("{}h", seconds / HOUR)
    } else {
        format!("{}d", seconds / DAY)
    }
}

/// One line of terminal-safe text, at most `max_chars` characters long.
///
/// Whitespace — newlines included — collapses to single spaces, so a copied
/// paragraph is one row. Anything else the terminal would *act* on rather than
/// show becomes a visible `\u{1b}` escape: control characters, and the
/// invisible and reordering formatting characters that `char::is_control`
/// does not cover.
///
/// Counting is on characters written, not characters read, so an entry made
/// entirely of escape sequences cannot push the row past the column width.
pub fn one_line(text: &str, max_chars: usize) -> String {
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

/// Characters `char::is_control` misses that a terminal still acts on.
///
/// The same set `clippo-watch` escapes, and for the same reason: `is_control`
/// is category Cc only, while the bidi overrides and isolates are Cf and
/// visually reorder everything after them — which in a table means the id
/// column and the row's own preview can be made to swap places. The zero-width
/// characters are here because "invisible in a preview" defeats the preview.
fn is_invisible_or_reordering(character: char) -> bool {
    matches!(character,
        '\u{00ad}'                // soft hyphen
        | '\u{061c}'              // arabic letter mark
        | '\u{180e}'              // mongolian vowel separator
        | '\u{200b}'..='\u{200f}' // zero-width space … RTL mark
        | '\u{202a}'..='\u{202e}' // bidi embeddings and overrides
        | '\u{2060}'..='\u{206f}' // word joiner, isolates, deprecated formatting
        | '\u{feff}'              // zero-width no-break space / BOM
        | '\u{fff9}'..='\u{fffb}' // interlinear annotation
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;

    fn now() -> Timestamp {
        Timestamp::from_unix_millis(NOW)
    }

    fn entry(id: i64, preview: &str) -> EntrySummary {
        EntrySummary {
            id,
            created_at: NOW,
            last_used_at: NOW,
            kind: "text".to_owned(),
            preview: preview.to_owned(),
            pinned: false,
            sensitive: false,
        }
    }

    fn seconds_ago(entry: EntrySummary, seconds: i64) -> EntrySummary {
        EntrySummary {
            last_used_at: NOW - seconds * 1_000,
            ..entry
        }
    }

    #[test]
    fn a_table_has_a_header_and_one_row_per_entry() {
        let entries = vec![
            seconds_ago(entry(2, "hello"), 90),
            EntrySummary {
                pinned: true,
                sensitive: true,
                kind: "image".to_owned(),
                ..seconds_ago(entry(140, "image/png, 2.0 KB"), 3 * 3600)
            },
        ];
        // `concat!` rather than one literal: a `\`-continued string literal
        // eats the leading spaces that the right-aligned ID column is made of.
        assert_eq!(
            table(&entries, now()),
            concat!(
                " ID  AGE  KIND   FL  PREVIEW\n",
                "  2   1m  text   ..  hello\n",
                "140   3h  image  ps  image/png, 2.0 KB\n",
            )
        );
    }

    #[test]
    fn an_empty_history_renders_as_nothing_at_all() {
        assert_eq!(table(&[], now()), "");
    }

    /// A trailing space on every row is invisible in the terminal and turns up
    /// again when somebody copies the output back out.
    #[test]
    fn no_row_ends_in_whitespace() {
        let rendered = table(
            &[entry(1, "short"), entry(2, "a much longer preview")],
            now(),
        );
        for line in rendered.lines() {
            assert_eq!(line, line.trim_end(), "{line:?}");
        }
    }

    #[test]
    fn ages_use_the_widest_unit_that_is_a_whole_number() {
        assert_eq!(age(now(), NOW), "0s");
        assert_eq!(age(now(), NOW - 59_000), "59s");
        assert_eq!(age(now(), NOW - 60_000), "1m");
        assert_eq!(age(now(), NOW - 3_599_000), "59m");
        assert_eq!(age(now(), NOW - 3_600_000), "1h");
        assert_eq!(age(now(), NOW - 86_400_000), "1d");
        assert_eq!(age(now(), NOW - 30 * 86_400_000), "30d");
    }

    /// A clock that went backwards while clippod was running leaves entries
    /// stamped in the future. `0s` is wrong but harmless; `-4h` in a right
    /// aligned column is a bug report.
    #[test]
    fn an_entry_from_the_future_reads_as_no_age_at_all() {
        assert_eq!(age(now(), NOW + 86_400_000), "0s");
    }

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
    /// in a table reorders the row it sits in — including the id somebody is
    /// about to type at `clippo rm`.
    #[test]
    fn reordering_and_invisible_characters_are_escaped_too() {
        assert_eq!(one_line("a\u{202e}b", 40), "a\\u{202e}b");
        assert_eq!(one_line("a\u{200b}b", 40), "a\\u{200b}b");
        assert_eq!(one_line("a\u{feff}b", 40), "a\\u{feff}b");
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
        let rendered = one_line("0123456789", 10);
        assert_eq!(rendered, "0123456789");
    }

    #[test]
    fn json_carries_every_field_the_daemon_sent_under_its_own_name() {
        let entries = vec![EntrySummary {
            pinned: true,
            sensitive: true,
            ..seconds_ago(entry(7, "line one\nline two"), 5)
        }];
        let rendered = json(&entries).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();

        let entry = &parsed[0];
        assert_eq!(entry["id"], 7);
        assert_eq!(entry["created_at"], NOW);
        assert_eq!(entry["last_used_at"], NOW - 5_000);
        assert_eq!(entry["kind"], "text");
        assert_eq!(entry["pinned"], true);
        assert_eq!(entry["sensitive"], true);
        // Whole and unflattened: a script wants the daemon's value, not the
        // table's rendering of it.
        assert_eq!(entry["preview"], "line one\nline two");
        assert!(rendered.ends_with('\n'), "{rendered:?}");
    }

    /// JSON's own escaping is what makes emitting an unsanitised preview safe.
    #[test]
    fn json_escapes_control_characters_rather_than_emitting_them() {
        let rendered = json(&[entry(1, "\u{1b}[2J")]).unwrap();
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(rendered.contains("\\u001b"), "{rendered:?}");
    }

    #[test]
    fn an_empty_history_is_an_empty_json_array_not_an_error() {
        assert_eq!(json(&[]).unwrap(), "[]\n");
    }
}
