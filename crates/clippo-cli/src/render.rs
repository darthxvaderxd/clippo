//! Turning `EntrySummary` values into something safe to put in a terminal.
//!
//! # Previews are hostile input
//!
//! A preview is whatever the user last copied, and a user copies things from
//! web pages and from other people's terminals. Why that matters, and which
//! characters it comes down to, is [`clippo_core::display`]'s subject — the
//! applet has the same problem and so does `clippo-watch`, so the rule lives
//! once, there. The daemon already flattens whitespace when it builds a
//! preview, but this module does not rely on that: everything printed goes
//! through [`one_line`], which is safe on arbitrary input so that it stays safe
//! if the daemon's preview rules change.
//!
//! [`json`] is the same rule by a different route. A script wants the daemon's
//! value, not a column's rendering of it, so previews stay whole and
//! unflattened — but the dangerous characters are `\u`-escaped by JSON's own
//! syntax, which decodes back to exactly what was stored. `serde_json` does that
//! for the control characters and not for the reordering ones, so
//! [`escape_invisible`] finishes the job.
//!
//! [`crate::cli::Command::Reveal`] is the one deliberate exception, and says so
//! in its `--help`. It exists to be redirected, and a value that came back
//! altered would be the wrong value.

use clippo_core::display::{is_invisible_or_reordering, one_line};
use clippo_core::Timestamp;
use clippo_ipc::EntrySummary;

use crate::cli::PREVIEW_COLUMN_CHARS;
use crate::error::CliError;

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
/// The preview is not flattened, but it is still made safe to print: JSON
/// escaping does the job here that [`one_line`] does for the table.
/// `serde_json` writes a control character as its `\u001b` form, and
/// [`escape_invisible`] does the same for the reordering and zero-width
/// characters it passes through — so this is safe in the terminal of somebody
/// running `clippo list --json` to see the shape of the output, while decoding
/// back to exactly the string the daemon sent. A script that decodes it and
/// prints the result is back to handling hostile bytes, exactly as it would be
/// with any other clipboard tool.
pub fn json(entries: &[EntrySummary]) -> Result<String, CliError> {
    let serialised = serde_json::to_string_pretty(entries).map_err(CliError::Json)?;
    let mut json = escape_invisible(&serialised);
    json.push('\n');
    Ok(json)
}

/// `\u`-escape the characters `serde_json` emits as themselves that a
/// terminal still acts on.
///
/// `serde_json` escapes the control characters, `"` and `\`, and passes every
/// other code point through as UTF-8 — which sends the Cf reordering and
/// zero-width characters that [`is_invisible_or_reordering`] exists for straight
/// at the terminal. `\uXXXX` is how JSON spells those anyway, so escaping
/// them changes the document's bytes without changing the string it decodes to:
/// a script reads what was stored, and only the display of it changes.
///
/// Rewriting the serialised text rather than the values is sound because JSON's
/// own syntax is ASCII. Every character this touches is therefore inside a
/// string literal, which is the one place `\uXXXX` means anything.
fn escape_invisible(json: &str) -> String {
    if !json.chars().any(is_invisible_or_reordering) {
        return json.to_owned();
    }

    let mut out = String::with_capacity(json.len());
    for character in json.chars() {
        if is_invisible_or_reordering(character) {
            // UTF-16 rather than the code point: a `\u` escape is four hex
            // digits, so anything above the BMP — the tag block — is written as
            // the surrogate pair JSON would have used for it.
            let mut units = [0_u16; 2];
            for unit in character.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        } else {
            out.push(character);
        }
    }
    out
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

    /// `one_line` itself is `clippo_core::display`'s, and tested there. What
    /// belongs here is that the table actually routes a preview through it: a
    /// column that stopped escaping would pass every test in that module.
    #[test]
    fn a_previews_escape_sequences_do_not_reach_the_table() {
        let rendered = table(&[entry(1, "clear:\u{1b}[2J and a \u{202e}flip")], now());
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(!rendered.contains('\u{202e}'), "{rendered:?}");
        assert!(rendered.contains("\\u{1b}"), "{rendered:?}");
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

    /// What a masked entry looks like on the way out of `clippo list`. The
    /// daemon has already replaced the value with `ab••••••••yz`, and the CLI's
    /// job is to print that unchanged: a bullet is neither a control character
    /// nor an invisible one, so nothing here escapes or collapses it.
    #[test]
    fn a_masked_preview_reaches_the_terminal_as_bullets() {
        let masked = "su\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}ue";
        let entries = vec![EntrySummary {
            sensitive: true,
            ..entry(3, masked)
        }];

        let table = table(&entries, now());
        assert!(table.contains(masked), "{table}");
        // …under the `s` flag, which is what says the row is a mask and not a
        // value that happens to contain bullets.
        assert!(table.contains(".s  su"), "{table}");

        let rendered = json(&entries).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed[0]["preview"], masked);
        assert_eq!(parsed[0]["sensitive"], true);
    }

    /// JSON's own escaping is what makes emitting an unflattened preview safe.
    #[test]
    fn json_escapes_control_characters_rather_than_emitting_them() {
        let rendered = json(&[entry(1, "\u{1b}[2J")]).unwrap();
        assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
        assert!(rendered.contains("\\u001b"), "{rendered:?}");
    }

    /// The half `serde_json` does not do. These are Cf, not Cc, so they go
    /// through its escaping untouched — and somebody running `clippo list
    /// --json` to see the shape of the output is looking at a terminal.
    #[test]
    fn json_escapes_the_reordering_and_invisible_characters_too() {
        for (raw, escaped) in [
            ("a\u{202e}b", "a\\u202eb"),
            ("a\u{200b}b", "a\\u200bb"),
            ("a\u{feff}b", "a\\ufeffb"),
            // Above the BMP, so it is a surrogate pair rather than one escape.
            ("a\u{e0041}b", "a\\udb40\\udc41b"),
        ] {
            let rendered = json(&[entry(1, raw)]).unwrap();
            assert!(rendered.contains(escaped), "{rendered:?}");
            for character in raw.chars().filter(|c| is_invisible_or_reordering(*c)) {
                assert!(!rendered.contains(character), "{rendered:?}");
            }
        }
    }

    /// The whole justification for escaping in the serialised text: a script
    /// decodes the document and gets the daemon's string back, byte for byte.
    /// If this ever fails, `--json` has stopped being machine output.
    #[test]
    fn an_escaped_preview_still_decodes_to_exactly_what_was_stored() {
        let preview = "a\u{202e}b\u{200b}c\u{e0041}d\u{1b}e\nf";
        let rendered = json(&[entry(1, preview)]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed[0]["preview"], preview);
    }

    /// An escape is only ever inside a string literal, so nothing that has no
    /// dangerous character in it is touched at all.
    #[test]
    fn ordinary_json_comes_back_unchanged() {
        let ordinary = "{\n  \"preview\": \"naïve café\"\n}";
        assert_eq!(escape_invisible(ordinary), ordinary);
    }

    #[test]
    fn an_empty_history_is_an_empty_json_array_not_an_error() {
        assert_eq!(json(&[]).unwrap(), "[]\n");
    }
}
