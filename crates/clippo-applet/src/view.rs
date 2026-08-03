//! Drawing the picker: a search field, a scrollable list, and a hint line.
//!
//! Pure rendering — every function here takes state and returns an `Element`,
//! and none of them decide anything. What a key does is [`crate::app`]'s
//! business; what a row looks like is this file's.
//!
//! # What a row shows
//!
//! [`EntrySummary::preview`] and nothing else, because that is the only content
//! `List` and `Search` return and M4 made it a mask for a suspected secret. A
//! sensitive row gets the mask the daemon sent plus a lock badge, and the badge
//! is the part that makes the masking legible: `ab••••••••yz` on its own could
//! be a value that genuinely looks like that.
//!
//! The one exception is a row the user has pressed `Ctrl+R` on, which draws the
//! revealed value in place of the mask. [`Model::revealed`] decides when that
//! is — this file just asks, so there is no route to drawing a secret that does
//! not go through the check that its row is still focused. What it draws is
//! bounded by [`readable`], because `Reveal` answers with the whole stored
//! value and the list is the wrong place to render megabytes.
//!
//! # Images
//!
//! An image row draws the stored `image/png;clippo-thumb` bytes, which capture
//! derived once. Nothing here ever asks for the full-size blob: the cache is
//! filled by `Thumbnail` calls and a row [`Thumbnails::get`] has nothing for
//! draws the generic image icon rather than falling back to the real image.
//!
//! A *sensitive* image row draws its thumbnail like any other, next to the lock
//! badge. That is a decision rather than an oversight, and it is M4's: an
//! image's preview is deliberately left unmasked because it is a type and a
//! size (`image/png, 2.0 KB`) and hiding it would protect nothing while
//! removing the only useful thing on the row. A thumbnail is the same
//! judgement one step further — the marker fires on the flavors, not on what
//! the picture shows, and a screenshot the user copied is a screenshot they
//! chose. `Reveal` remains the only route to the full-size bytes.

use clippo_ipc::EntrySummary;
use cosmic::iced::{Alignment, Length};
use cosmic::widget;
use cosmic::widget::image::Handle;
use cosmic::{theme, Element};

use crate::app::{Action, Message};
use crate::model::{Model, Status};
use crate::thumbs::Thumbnails;

/// The id of the search field, so that opening the picker can focus it.
pub fn search_id() -> widget::Id {
    widget::Id::new("clippo-search")
}

/// The longest preview drawn on one row.
///
/// The daemon's previews are already single-line, but not bounded to anything
/// this narrow, and a very long one would otherwise set the width of every row
/// in the list.
const PREVIEW_CHARS: usize = 96;

/// The longest revealed value drawn on one row.
///
/// Generous where [`PREVIEW_CHARS`] is tight, because a revealed value is the
/// one thing on a row the user asked to *read* rather than to browse. M4's
/// entropy rule will not call anything longer than 128 characters a secret, so
/// a password or a token is far inside this. Its *shape* rules are the
/// exception and are what set the figure: they have no length gate at all, and
/// `shape:private-key-block` matches a whole PEM block, which for an RSA-2048
/// key is around 1 750 characters. That fits. A 4096-bit key does not, and gets
/// the ellipsis and `clippo reveal <id>` like any other oversized value.
///
/// It is a bound rather than no limit because `Reveal` returns the whole
/// flavor — `clippod` sets `max_flavor_bytes` to at least 8 MiB — and the row
/// sits inside a vertical [`widget::scrollable()`], which offers its content
/// infinite height. So nothing downstream truncates: the text widget shapes
/// every line it is given and takes the height that needs. Shaped at
/// [`Picker`][crate::surface::Picker]'s width, a 10 KB value comes out around
/// 4 000 pixels tall — against a 560-pixel popup — and a 1 MB one around
/// 400 000, on a frame that spends a quarter of a second doing it. `Ctrl+R` is
/// not restricted to sensitive rows either, so that is an ordinary large paste
/// rather than a contrived one.
const REVEAL_CHARS: usize = 2_000;

/// The most lines a revealed value is drawn over.
///
/// [`REVEAL_CHARS`] on its own does not bound the height: the same characters
/// as one wrapped paragraph are a few dozen layout runs, and as
/// [`REVEAL_CHARS`] newlines they are [`REVEAL_CHARS`] of them. Previews cannot
/// reach this because the daemon flattens those, but a revealed value is the
/// stored bytes verbatim.
///
/// The figure is set by the same key [`REVEAL_CHARS`] is. A PEM block is one
/// line per 64 base64 characters, so an RSA-2048 private key — the longest
/// thing M4's shape rules call a secret, and the most likely long one in the
/// picker — is 28 lines, and a cap that cut it would cut it in half. This still
/// bounds the height hard: at most 32 explicit runs plus the few dozen that
/// [`REVEAL_CHARS`] wraps into, around 1 400 pixels against a 560-pixel popup,
/// where an unbounded row reached 400 000.
const REVEAL_LINES: usize = 32;

/// Pixel size of a thumbnail in the list.
const THUMB: f32 = 40.0;

/// Characters of the refusal reason the picker prints.
///
/// Generous, because the reason is the actionable part — it carries the pid and
/// the executable path — but bounded, because that path comes from `/proc` and
/// therefore from the refused process itself. A peer that named itself two
/// thousand characters would otherwise choose the height of this popup.
///
/// Length is all this bounds. The *characters* in that path — the reordering
/// ones a GUI acts on as readily as a terminal does — are escaped where the
/// refusal is built, in `clippo_ipc::peer`, because the same string is also
/// printed by `clippo` and written to the journal.
const REASON_CHARS: usize = 240;

/// The whole picker.
pub fn picker<'a>(model: &'a Model, thumbnails: &'a Thumbnails) -> Element<'a, Message> {
    let search = widget::search_input("Search the clipboard", model.query())
        .id(search_id())
        .on_input(Message::QueryChanged)
        // Enter in the field is the same as Enter anywhere else in the picker.
        // Without this the field would swallow it and the primary action would
        // be the one key that needed the mouse.
        .on_submit(|_| Message::Key(Action::Activate))
        .width(Length::Fill);

    let body: Element<'a, Message> = match model.status() {
        Status::DaemonUnavailable => daemon_missing(),
        Status::DaemonUntrusted(why) => daemon_untrusted(why),
        Status::Connected if model.entries().is_empty() => nothing_here(model.query()),
        Status::Connected => list(model, thumbnails),
    };

    widget::Column::new()
        .push(search)
        .push(widget::divider::horizontal::default())
        .push(body)
        .push(widget::divider::horizontal::default())
        .push(hints(model))
        .spacing(8)
        .padding(8)
        .into()
}

/// The rows.
fn list<'a>(model: &'a Model, thumbnails: &'a Thumbnails) -> Element<'a, Message> {
    let selected = model.selected_id();
    let revealed = model.revealed();

    let rows = model
        .entries()
        .iter()
        .fold(widget::Column::new().spacing(2), |column, entry| {
            let is_selected = selected == Some(entry.id);
            column.push(row(
                entry,
                is_selected,
                // Only ever handed to the row it belongs to. `Model::revealed`
                // has already established that this is the focused row, so the
                // check here is about not leaking it sideways into the others.
                is_selected.then_some(revealed).flatten(),
                // Answers `None` for anything that is not an image row, so a
                // cache mistake cannot put a picture on a row of text.
                thumbnails.get(entry),
            ))
        });

    widget::scrollable(rows)
        .height(Length::Fill)
        .width(Length::Fill)
        .into()
}

/// One entry.
fn row<'a>(
    entry: &'a EntrySummary,
    selected: bool,
    revealed: Option<&'a str>,
    thumbnail: Option<&'a Handle>,
) -> Element<'a, Message> {
    let leading: Element<'a, Message> = match thumbnail {
        Some(handle) => widget::image(handle.clone())
            .width(THUMB)
            .height(THUMB)
            .into(),
        None => widget::icon::from_name(kind_icon(&entry.kind))
            .size(16)
            .into(),
    };

    // A revealed value is wrapped rather than cut at the width of the list: a
    // secret the user has to go elsewhere to finish reading defeats the reason
    // they pressed Ctrl+R. It is still bounded — see `readable` — because the
    // value is the whole flavor and the row has no height to be cut to. A
    // preview stays on the tight cap: those are browsed rather than read, and
    // one long one would otherwise set the width of every row.
    let body = match revealed {
        Some(value) => widget::text::body(readable(value)).width(Length::Fill),
        None => widget::text::body(shorten(&entry.preview, PREVIEW_CHARS)).width(Length::Fill),
    };
    let mut line = widget::Row::new()
        .push(leading)
        .push(body)
        .spacing(8)
        .align_y(Alignment::Center);

    if entry.pinned {
        line = line.push(widget::icon::from_name("view-pin-symbolic").size(14));
    }
    if entry.sensitive {
        // The badge, not the mask, is what tells the user this row is being
        // treated as a secret — and it stays on while the value is revealed,
        // because a revealed row is still a sensitive one.
        line = line.push(widget::icon::from_name("changes-prevent-symbolic").size(14));
    }

    widget::button::custom(line)
        .class(if selected {
            theme::Button::Suggested
        } else {
            theme::Button::MenuItem
        })
        .width(Length::Fill)
        .padding([4, 8])
        // Clicking a row selects it and copies it, which is what a click on a
        // clipboard entry can only reasonably mean.
        .on_press(Message::Chose(entry.id))
        .into()
}

/// The explicit "no daemon" state.
///
/// M5 asks for this by name: an empty list for a dead `clippod` reads as an
/// empty history, which would have the user hunting for clipboard entries that
/// were never lost.
fn daemon_missing<'a>() -> Element<'a, Message> {
    widget::Column::new()
        .push(widget::icon::from_name("dialog-warning-symbolic").size(32))
        .push(widget::text::body("clippod is not running"))
        .push(widget::text::caption(
            "Your history is safe — nothing can be read or recorded until the daemon is back. \
             Start it with `systemctl --user start clippod`.",
        ))
        .spacing(8)
        .padding(16)
        .align_x(Alignment::Center)
        .width(Length::Fill)
        .into()
}

/// Something holds the daemon's name and it is not clippod.
///
/// The one screen in this applet that exists to be alarming. The chain F5
/// describes ends in "the applet reconnects and looks normal", and a check that
/// refused quietly would leave that ending intact — the user would see an empty
/// picker and assume their history had gone, which is a story with an innocent
/// explanation. So this says what is happening, in the place they are looking.
///
/// The reason is printed through [`shorten`] rather than raw: it is built from
/// a path out of `/proc`, which is a filename of somebody else's choosing, and
/// a panel popup is not the place to find out what a very long one does to the
/// layout.
fn daemon_untrusted(why: &str) -> Element<'_, Message> {
    widget::Column::new()
        .push(widget::icon::from_name("dialog-error-symbolic").size(32))
        .push(widget::text::body("Not talking to this clippod"))
        .push(widget::text::caption(untrusted_caption(why)))
        .push(widget::text::caption(
            "Your history is on disk and encrypted; a peer on the bus cannot read it. Check \
             `systemctl --user status clippod`, then look at the process above.",
        ))
        .spacing(8)
        .padding(16)
        .align_x(Alignment::Center)
        .width(Length::Fill)
        .into()
}

/// The sentence [`daemon_untrusted`] leads with.
///
/// A function of its own so the wording is testable without building a widget
/// tree: the two things that must be true of it are that it says nothing was
/// sent, and that it names the process — and neither is worth having to render
/// a popup to check.
fn untrusted_caption(why: &str) -> String {
    format!(
        "Something is holding clippo's D-Bus name and it is not a clippo program, so nothing has \
         been sent to it — not your searches, not anything. {}",
        shorten(why, REASON_CHARS)
    )
}

/// Connected, but with nothing to show.
fn nothing_here(query: &str) -> Element<'_, Message> {
    let message = if query.is_empty() {
        "Nothing copied yet".to_owned()
    } else {
        format!("Nothing matched \u{201c}{}\u{201d}", shorten(query, 40))
    };

    widget::Column::new()
        .push(widget::text::body(message))
        .padding(16)
        .align_x(Alignment::Center)
        .width(Length::Fill)
        .into()
}

/// The key hints.
///
/// Present because every one of M5's bindings is invisible otherwise — there is
/// no menu bar to discover them from, and a keyboard-first UI whose keys are
/// undocumented is a mouse-driven one in practice.
fn hints(model: &Model) -> Element<'_, Message> {
    let mut hints = vec![
        "\u{2191}\u{2193} move",
        "Enter copy",
        "Del remove",
        "Ctrl+P pin",
    ];
    if model.selected_entry().is_some_and(|entry| entry.sensitive) {
        hints.push("Ctrl+R reveal");
    }

    widget::text::caption(hints.join("   \u{b7}   "))
        .width(Length::Fill)
        .into()
}

/// The icon for an entry kind.
///
/// Falls through to the text icon for a kind this build does not know, which is
/// the safe direction: a new `clippo_core::EntryKind` should draw a plain row
/// rather than an empty one.
fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "image" => "image-x-generic-symbolic",
        "html" => "text-html-symbolic",
        "uris" => "web-browser-symbolic",
        _ => "text-x-generic-symbolic",
    }
}

/// Cut a revealed value down to something the list can draw.
///
/// Bounded in both directions, because the two are independent: the character
/// cap alone leaves a value of newlines one layout run per line, and a line cap
/// alone leaves one paragraph wrapping forever. Together they put a constant
/// ceiling on the row's height — at most [`REVEAL_LINES`] runs plus however few
/// [`REVEAL_CHARS`] wraps into — whatever the daemon returned.
///
/// A cut is marked with the same ellipsis a cut preview gets, so a value that
/// did not fit is visibly a value that did not fit rather than one that looks
/// complete. `clippo reveal <id>` in a terminal is the way to see the rest, and
/// the values the mask exists for are inside both caps — up to and including a
/// PEM private key, which is what sets them.
///
/// The scan is bounded by the caps rather than by the value, because this runs
/// on every redraw of a revealed row — including the ones mouse motion over the
/// popup causes — and `Ctrl+R` is not restricted to rows the daemon called
/// sensitive, so the value can be an ordinary multi-megabyte paste. It stops at
/// whichever cap comes first, which makes the cost a function of the caps.
fn readable(value: &str) -> String {
    // Byte index the character cap cuts at: the start of character number
    // `REVEAL_CHARS - 1`, so the ellipsis is the last of the `REVEAL_CHARS`
    // drawn. Reaching the read below means having passed that character, so the
    // initial value is never the one used.
    let mut ellipsis_at = 0;
    let mut newlines = 0;

    // `chars` is how many characters precede this one, so seeing `REVEAL_CHARS`
    // of them is exactly "the value is longer than the cap".
    for (chars, (index, character)) in value.char_indices().enumerate() {
        if chars == REVEAL_CHARS {
            return value[..ellipsis_at].to_owned() + "\u{2026}";
        }
        if chars + 1 == REVEAL_CHARS {
            ellipsis_at = index;
        }
        if character == '\n' {
            newlines += 1;
            if newlines == REVEAL_LINES {
                // Everything from this newline on is dropped. If that is only
                // the newline itself — a value that ends in one — then there is
                // no more to promise, and the ellipsis would be a lie.
                let more = if value[index + 1..].is_empty() {
                    ""
                } else {
                    "\u{2026}"
                };
                return value[..index].to_owned() + more;
            }
        }
    }
    value.to_owned()
}

/// Cut a preview to a width the list can draw, on a character boundary.
///
/// Characters rather than bytes so that a multi-byte preview cannot panic here,
/// and an ellipsis so a cut row is visibly cut.
fn shorten(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    text.chars()
        .take(limit.saturating_sub(1))
        .chain(std::iter::once('\u{2026}'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_preview_is_left_alone() {
        assert_eq!(shorten("hello", 10), "hello");
        assert_eq!(shorten("hello", 5), "hello");
    }

    #[test]
    fn a_long_preview_is_cut_with_an_ellipsis() {
        assert_eq!(shorten("abcdef", 4), "abc\u{2026}");
        assert_eq!(shorten("abcdef", 4).chars().count(), 4);
    }

    /// Cutting by characters, not bytes: the same cut done on `&str[..n]`
    /// panics on this input.
    #[test]
    fn a_multibyte_preview_is_cut_without_panicking() {
        let cut = shorten("\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}", 3);
        assert_eq!(cut, "\u{e9}\u{e9}\u{2026}");
    }

    #[test]
    fn shortening_to_nothing_does_not_panic() {
        assert_eq!(shorten("abc", 0), "\u{2026}");
    }

    /// Every value the mask exists for. M4's entropy rule refuses to call
    /// anything over 128 characters a secret, and its shape rules — which have
    /// no length gate — top out at a PEM private key, so the caps are not a cut
    /// in any case `Ctrl+R` was added for.
    ///
    /// The key is a real RSA-2048 block's shape rather than one sized to the
    /// caps: 26 body lines of 64 base64 characters between the header and the
    /// footer. Calibrating the fixture to the constant instead would make this
    /// pass for a cap that halves the most likely long secret in the picker.
    #[test]
    fn a_secret_is_revealed_whole() {
        let body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDGh3Zx8kKp3nQ7";
        let key = format!(
            "-----BEGIN RSA PRIVATE KEY-----\n{}\n-----END RSA PRIVATE KEY-----",
            vec![body; 26].join("\n")
        );
        assert_eq!(
            key.lines().count(),
            28,
            "the shape of an RSA-2048 PEM block"
        );
        assert!(key.chars().count() > 1_700, "{}", key.chars().count());

        assert_eq!(readable("hunter2"), "hunter2");
        assert_eq!(readable(&key), key);
    }

    /// The blocking half. `Reveal` returns the whole flavor — megabytes, if the
    /// user pressed `Ctrl+R` on a large paste — and the row is inside a
    /// scrollable, which imposes no height of its own. Without this the widget's
    /// height is a linear function of the daemon's answer.
    #[test]
    fn a_revealed_value_is_bounded_in_characters() {
        let huge = "a".repeat(1_000_000);

        let drawn = readable(&huge);

        assert_eq!(drawn.chars().count(), REVEAL_CHARS);
        assert!(drawn.ends_with('\u{2026}'), "and says that it was cut");
    }

    /// The other half of the bound: the character cap alone leaves a value of
    /// newlines one layout run per line, which is the taller of the two ways to
    /// get a 400 000-pixel row.
    #[test]
    fn a_revealed_value_is_bounded_in_lines() {
        let tall = "x\n".repeat(100_000);

        let drawn = readable(&tall);

        assert_eq!(drawn.lines().count(), REVEAL_LINES);
        assert!(drawn.ends_with('\u{2026}'), "and says that it was cut");
    }

    /// Both at once, on the value that is long *and* tall: neither cap is
    /// allowed to hide that the other one fired.
    #[test]
    fn a_value_cut_both_ways_is_marked_once() {
        let both = "b".repeat(REVEAL_CHARS * 2) + "\n" + &"c\n".repeat(50);

        let drawn = readable(&both);

        assert_eq!(drawn.chars().count(), REVEAL_CHARS);
        assert_eq!(drawn.matches('\u{2026}').count(), 1);
    }

    /// A value that fills the line cap and ends in a newline has dropped
    /// nothing but that empty final line, so it must not claim there is more.
    #[test]
    fn a_trailing_newline_is_not_more_to_see() {
        let exactly = "x\n".repeat(REVEAL_LINES);

        let drawn = readable(&exactly);

        assert_eq!(drawn, exactly.trim_end_matches('\n'));
        assert!(!drawn.contains('\u{2026}'));
    }

    /// F5's step 2: the refusal is visible where the user actually looks. The
    /// journal is not evidence a panel user will ever see; this caption is.
    #[test]
    fn the_picker_says_it_is_refusing_and_names_the_process() {
        let drawn = untrusted_caption(":1.42 is pid 4321, which is running /usr/bin/python3");

        assert!(drawn.contains("nothing has been sent"), "{drawn}");
        assert!(drawn.contains("4321"), "{drawn}");
        assert!(drawn.contains("/usr/bin/python3"), "{drawn}");
    }

    /// The reason comes out of `/proc`, so its length is chosen by the process
    /// being refused. A popup whose height an impostor picks is not one.
    #[test]
    fn a_very_long_refusal_reason_is_cut_before_it_reaches_the_popup() {
        let drawn = untrusted_caption(&"x".repeat(10_000));

        assert!(
            drawn.chars().count() < 500,
            "{} chars",
            drawn.chars().count()
        );
        assert!(drawn.contains('\u{2026}'), "{drawn}");
    }

    #[test]
    fn every_entry_kind_the_daemon_sends_has_an_icon() {
        for kind in ["text", "html", "uris", "image"] {
            assert!(kind_icon(kind).ends_with("-symbolic"), "{kind}");
        }
        assert_eq!(kind_icon("something-new"), "text-x-generic-symbolic");
    }
}
