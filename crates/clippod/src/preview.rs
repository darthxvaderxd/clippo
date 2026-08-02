//! Turning a captured selection into the one line a frontend shows.
//!
//! **This is where masking happens** — [`describe`] and nowhere else, because a
//! preview is the only content [`crate::daemon`] puts in an
//! [`EntrySummary`][clippo_ipc::EntrySummary], and [`describe`] is the only
//! place a preview comes from. The full value leaves the daemon through exactly
//! one member, `Reveal`, which calls [`reveal`] below.
//!
//! Keeping those two functions side by side is the point of the module: a
//! reader can see at a glance that the masked path and the unmasked path are
//! different functions, and that only one of them is reachable from `List` and
//! `Search`.
//!
//! # A masked preview is masked in the database
//!
//! [`describe`] runs once, at capture, and its answer is what
//! `entries.preview` holds. A sensitive entry's preview column is `ab••••••••yz`
//! — the whole value is in the `flavors` table and nowhere else. That is
//! stronger than masking on the way out: there is no code path from `List`,
//! from `Search`, from the cache or from a future member that could render an
//! unmasked preview, because the daemon does not have one to render.
//!
//! It also means detection runs against the *whole* value rather than the
//! 120-character preview, which the entropy rule's length gate would otherwise
//! see the wrong end of.
//!
//! The cost of storing the answer is that a row keeps whatever this module
//! concluded on the day it was captured — before masking existed, or while
//! `entropy_rule` was off. There is no migration pass over the history, so the
//! correction happens on use: a repeat copy re-runs detection and
//! [`clippo_store::Store::insert`]'s bump takes the new preview whenever the
//! new capture is the sensitive one. The flag and the mask only ever move
//! together, and only ever towards safety.

use std::borrow::Cow;

use clippo_core::secrets::{self, Signal};
use clippo_core::{EntryKind, Flavor, SecretsConfig};
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

/// The one-line rendering of a copy, before masking.
///
/// **Private on purpose.** This is the unmasked renderer; [`describe`] is the
/// one that decides whether a copy may be shown at all, and it is the only
/// caller. A `pub` here would be a second way to build a preview, and the next
/// person to need one would find it.
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
fn build(kind: EntryKind, flavors: &[Flavor]) -> String {
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

/// What a captured copy looks like to a frontend: one line, and whether it is
/// a suspected secret.
///
/// The two travel together because they are decided together — the flag is what
/// says the line is a mask — and because a caller that could set one without
/// the other is a caller that could store a full preview with `sensitive =
/// true`, which every frontend would render as a lock badge next to a password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    /// The `entries.preview` value: masked if `sensitive`.
    pub preview: String,
    /// The `entries.sensitive` value.
    pub sensitive: bool,
    /// Which rule decided, for the log line. `None` when nothing fired.
    pub signal: Option<Signal>,
}

/// Describe a captured copy: detect, then render accordingly.
///
/// `hinted` is whether the selection carried the password-manager MIME marker;
/// the daemon reads it from the [`Selection`][clippo_wayland::Selection],
/// because the marker may be advertised and never received and is not stored.
///
/// A suspected secret is rendered by [`mask`][clippo_core::secrets::mask] over
/// the *flattened* value, not the raw one. Flattening first is what keeps a
/// newline or an `ESC` out of the two visible characters at each end: the mask
/// is written straight into a terminal by the CLI, and `ab` is only safe if it
/// is really `ab`. Everything else about the value — its length above all — is
/// already gone by then.
///
/// An image is never masked even when the marker fires. Its preview is
/// `image/png, 2.0 KB`, which is a type and a size and reveals nothing; masking
/// it would hide the one useful thing on the row while protecting nothing. The
/// `sensitive` flag is still set, so the applet still draws the badge, and the
/// full-size blob is still only reachable through `Copy`.
///
/// M5 added one thing this used to say was unreachable: the applet draws the
/// derived thumbnail on the row, so a hinted image shows a downscale of itself
/// beside the lock badge. That follows from the paragraph above rather than
/// contradicting it — the marker fires on the flavors, not on what the picture
/// shows, and an image whose *preview* is deliberately unmasked has nothing
/// left for a mask to protect. `Reveal` still refuses an image outright.
pub fn describe(
    kind: EntryKind,
    flavors: &[Flavor],
    hinted: bool,
    config: &SecretsConfig,
) -> Description {
    let value = whole_value(kind, flavors).unwrap_or_default();
    let signal = secrets::detect(&value, hinted, config);
    let sensitive = signal.is_some();

    let preview = if sensitive && kind != EntryKind::Image {
        secrets::mask(&flatten(&value), config)
    } else {
        build(kind, flavors)
    };

    Description {
        preview,
        sensitive,
        signal,
    }
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
    whole_value(kind, flavors).map(Cow::into_owned)
}

/// The same value [`reveal`] returns, borrowed.
///
/// [`describe`] needs the whole value to detect against and would otherwise
/// copy a multi-megabyte text copy to look at the first sixty kilobytes of it,
/// on the capture path, for every copy. `Cow` borrows for valid UTF-8, which is
/// every text flavor that is not corrupt.
///
/// Detection and `Reveal` reading the *same* function is the point, and not
/// only an optimisation: a value that detection judged and a value the user is
/// shown must not be able to become two different strings.
fn whole_value(kind: EntryKind, flavors: &[Flavor]) -> Option<Cow<'_, str>> {
    if kind == EntryKind::Image {
        return None;
    }
    preview_source(kind, flavors).map(|flavor| String::from_utf8_lossy(&flavor.data))
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

    fn secrets() -> SecretsConfig {
        SecretsConfig::default()
    }

    fn describe_text(value: &str) -> Description {
        describe(EntryKind::Text, &text(value), false, &secrets())
    }

    #[test]
    fn an_ordinary_copy_is_described_exactly_as_it_was_before() {
        let described = describe_text("  one\n\ttwo  ");
        assert_eq!(described.preview, "one two");
        assert!(!described.sensitive);
        assert_eq!(described.signal, None);
    }

    #[test]
    fn a_suspected_secret_is_described_as_a_mask() {
        let described = describe_text("Xr4$Tp9!Lm2#Wq7&Zc5%");
        assert_eq!(
            described.preview,
            "Xr\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}5%"
        );
        assert!(described.sensitive);
        assert_eq!(described.signal, Some(Signal::Entropy));
    }

    /// The marker masks and does not skip: there is still an entry, still with
    /// its flavors, and `reveal` still answers with the password.
    #[test]
    fn a_password_manager_copy_is_masked_rather_than_discarded() {
        let flavors = text("hunter2");
        let described = describe(EntryKind::Text, &flavors, true, &secrets());
        assert!(described.sensitive);
        assert_eq!(described.signal, Some(Signal::PasswordManagerHint));
        assert!(!described.preview.contains("hunter2"));
        assert_eq!(reveal(EntryKind::Text, &flavors).unwrap(), "hunter2");
    }

    /// Detection reads the whole value and masking reads the whole value, so a
    /// secret longer than a preview still shows its real last two characters
    /// rather than the last two of a truncation.
    #[test]
    fn the_mask_is_built_from_the_whole_value_not_from_the_truncated_preview() {
        let long = format!("sk-{}ZZ", "a1B2".repeat(50));
        assert!(long.chars().count() > PREVIEW_MAX_CHARS);

        let described = describe_text(&long);
        assert!(described.sensitive);
        assert!(described.preview.starts_with("sk"), "{}", described.preview);
        assert!(described.preview.ends_with("ZZ"), "{}", described.preview);
        assert_eq!(described.preview.chars().count(), 2 + 8 + 2);
    }

    /// The mask goes straight into a terminal, so the two visible characters at
    /// each end must be characters. Flattening before masking is what makes the
    /// ends of a value that begins with an escape sequence safe.
    #[test]
    fn a_mask_never_carries_a_control_character_out_of_the_value() {
        let described = describe_text("sk-ClippoFixtureNotARealKey0000000000\u{1b}[0m\n");
        assert!(described.sensitive);
        assert!(
            !described.preview.chars().any(char::is_control),
            "{:?}",
            described.preview
        );
        // The escape is gone, so the visible characters are the value's.
        assert!(described.preview.starts_with("sk"), "{}", described.preview);
    }

    /// The other half of that: a value whose only whitespace is the newline the
    /// copy came with is still one token, and still a secret.
    #[test]
    fn a_trailing_newline_does_not_hide_a_password() {
        let described = describe_text("Xr4$Tp9!Lm2#Wq7&Zc5%\n");
        assert!(described.sensitive);
        assert_eq!(described.signal, Some(Signal::Entropy));
        assert_eq!(
            described.preview,
            "Xr\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}5%"
        );
    }

    /// An image's preview is its type and its size, which gives nothing away.
    /// The flag is still set, so the applet still marks the row.
    #[test]
    fn an_image_from_a_password_manager_keeps_its_size_preview() {
        let flavors = vec![Flavor::new("image/png", vec![0_u8; 2048])];
        let described = describe(EntryKind::Image, &flavors, true, &secrets());
        assert_eq!(described.preview, "image/png, 2.0 KB");
        assert!(described.sensitive);
    }

    /// The config half, at the level the daemon uses: the same copy, described
    /// twice, once with the entropy rule and once without.
    #[test]
    fn the_entropy_knob_changes_what_this_module_masks() {
        let without = SecretsConfig {
            entropy_rule: false,
            ..secrets()
        };
        let generated = text("Xr4$Tp9!Lm2#Wq7&Zc5%");

        assert!(describe(EntryKind::Text, &generated, false, &secrets()).sensitive);
        let unmasked = describe(EntryKind::Text, &generated, false, &without);
        assert!(!unmasked.sensitive);
        assert_eq!(unmasked.preview, "Xr4$Tp9!Lm2#Wq7&Zc5%");

        // …and the two rules that do not guess are unaffected.
        let token = text("AKIAIOSFODNN7EXAMPLE");
        assert!(describe(EntryKind::Text, &token, false, &without).sensitive);
        assert!(describe(EntryKind::Text, &generated, true, &without).sensitive);
    }

    /// How much of the value is visible is the user's setting, and the bullet
    /// run is not.
    #[test]
    fn the_visible_ends_of_a_mask_follow_the_config() {
        let described = describe(
            EntryKind::Text,
            &text("Xr4$Tp9!Lm2#Wq7&Zc5%"),
            false,
            &SecretsConfig {
                mask_prefix: 4,
                mask_suffix: 0,
                ..secrets()
            },
        );
        assert_eq!(
            described.preview,
            "Xr4$\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}"
        );
    }

    #[test]
    fn byte_counts_read_the_way_max_image_bytes_is_written() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(8 * 1024 * 1024), "8.0 MB");
    }
}
