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
//! not go through the check that its row is still focused.
//!
//! # Images
//!
//! An image row draws the stored `image/png;clippo-thumb` bytes, which capture
//! derived once. Nothing here ever asks for the full-size blob: the thumbnails
//! map is filled by `Thumbnail` calls and a row with no entry in it draws the
//! generic image icon rather than falling back to the real image.

use std::collections::HashMap;

use clippo_ipc::EntrySummary;
use cosmic::iced::{Alignment, Length};
use cosmic::widget::{self, image};
use cosmic::{theme, Element};

use crate::app::{Action, Message};
use crate::model::{Model, Status};

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

/// Pixel size of a thumbnail in the list.
const THUMB: f32 = 40.0;

/// The whole picker.
pub fn picker<'a>(
    model: &'a Model,
    thumbnails: &'a HashMap<i64, image::Handle>,
) -> Element<'a, Message> {
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
fn list<'a>(model: &'a Model, thumbnails: &'a HashMap<i64, image::Handle>) -> Element<'a, Message> {
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
                thumbnails.get(&entry.id),
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
    thumbnail: Option<&'a image::Handle>,
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

    let text = revealed.unwrap_or(&entry.preview);
    let mut line = widget::Row::new()
        .push(leading)
        .push(widget::text::body(shorten(text, PREVIEW_CHARS)).width(Length::Fill))
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
/// the safe direction: a new [`clippo_core::EntryKind`] should draw a plain row
/// rather than an empty one.
fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "image" => "image-x-generic-symbolic",
        "html" => "text-html-symbolic",
        "uris" => "web-browser-symbolic",
        _ => "text-x-generic-symbolic",
    }
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

    #[test]
    fn every_entry_kind_the_daemon_sends_has_an_icon() {
        for kind in ["text", "html", "uris", "image"] {
            assert!(kind_icon(kind).ends_with("-symbolic"), "{kind}");
        }
        assert_eq!(kind_icon("something-new"), "text-x-generic-symbolic");
    }
}
