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
//! # What we detect over is not what we render
//!
//! Two different questions, deliberately answered by two different functions:
//!
//! - **What we render** is [`preview_source`] — one flavor, the most readable
//!   one, read through [`whole_value`]. `Reveal` reads the same function, so
//!   revealing shows the whole of the thing the row showed part of.
//! - **What we detect over** is [`detection_flavors`] — *every* text-bearing
//!   flavor the entry carries, because a copy is several flavors of one thing
//!   and a secret can be in any of them. A page that writes
//!   `text/plain: "Click to continue"` beside `text/html: "<code>sk-…</code>"`
//!   is stored as an `html` entry, previews as the innocuous line, and pastes
//!   the key — so reading only the previewed flavor loses the flag, which is
//!   the one signal a frontend gives the user that a row is dangerous.
//!
//! They were one call before, and the divergence is the point rather than an
//! oversight: the flag describes the entry, the preview describes the row.
//!
//! The cost of storing the answer is that a row keeps whatever this module
//! concluded on the day it was captured — before masking existed, or while
//! `entropy_rule` was off. There is no migration pass over the history, so the
//! correction happens on use: a repeat copy re-runs detection and
//! [`clippo_store::Store::insert`]'s bump takes the new preview whenever the
//! new capture is the sensitive one. The flag and the mask only ever move
//! together, and only ever towards safety.

use std::borrow::Cow;
use std::ptr;

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
///
/// # Detection reads every flavor; the mask covers the rendered one
///
/// The rules run over each of [`detection_flavors`], not over the previewed
/// flavor alone — see the module docs for why. The *mask* is still built from
/// the previewed value, because a mask is a rendering of the row: an entry
/// flagged by its `text/html` flavor still shows `Cl••••••••ue` over the plain
/// text it would otherwise have shown, which is the same shape of row as every
/// other masked entry.
pub fn describe(
    kind: EntryKind,
    flavors: &[Flavor],
    hinted: bool,
    config: &SecretsConfig,
) -> Description {
    let signal = detect_over_flavors(kind, flavors, hinted, config);
    let sensitive = signal.is_some();

    let preview = if sensitive && kind != EntryKind::Image {
        let rendered = whole_value(kind, flavors).unwrap_or_default();
        secrets::mask(&flatten(&rendered), config)
    } else {
        build(kind, flavors)
    };

    Description {
        preview,
        sensitive,
        signal,
    }
}

/// Run the detection rules over every text-bearing flavor and report one signal.
///
/// # Which signal, when more than one flavor fires
///
/// The rules are ordered before the flavors are, which is the same rule
/// [`secrets::detect`] already applies within a single value: the reported
/// signal is the **highest-confidence** one any flavor produced — the marker,
/// then a token shape, then entropy — and only ties are broken by flavor order,
/// **canonical flavor first, then the remaining flavors in captured order**.
///
/// Stating it matters because the alternative is deciding by whichever flavor a
/// compositor happened to advertise first. Confidence first means an entry
/// whose `text/html` matched an AWS key and whose `text/plain` merely looked
/// random is reported as `shape:aws-access-key-id` — the defensible answer —
/// and the tie-break prefers the flavor that is the entry's identity.
///
/// The marker is checked once, up front, because it is a property of the
/// *selection* rather than of any one flavor: it may be advertised and never
/// received, and an image carries no text for the other two rules to read.
fn detect_over_flavors(
    kind: EntryKind,
    flavors: &[Flavor],
    hinted: bool,
    config: &SecretsConfig,
) -> Option<Signal> {
    if hinted {
        return Some(Signal::PasswordManagerHint);
    }

    // Two lines implement the rule above. A shape is the best a flavor can
    // report once the marker is out of the way, so the first one found is the
    // answer and nothing after it can change it — the same reason
    // `secrets::detect` stops at its first match. Entropy is only ever a
    // fallback, so it is kept and beaten later by a shape from any flavor.
    let mut fallback: Option<Signal> = None;
    for flavor in detection_flavors(kind, flavors) {
        match secrets::detect(&String::from_utf8_lossy(&flavor.data), false, config) {
            Some(shape @ Signal::Shape(_)) => return Some(shape),
            Some(other) if fallback.is_none() => fallback = Some(other),
            _ => {}
        }
    }
    fallback
}

/// The flavors detection is run over, in the order that breaks a tie.
///
/// Canonical flavor first — it is the entry's identity, so a signal from it is
/// the one that describes the entry rather than one of its renderings — then
/// every other text-bearing flavor in the order it was captured in.
fn detection_flavors(kind: EntryKind, flavors: &[Flavor]) -> Vec<&Flavor> {
    let canonical = dedup::canonical_flavor(kind, flavors)
        .and_then(|canonical| flavors.iter().position(|flavor| ptr::eq(flavor, canonical)))
        .filter(|index| is_text_bearing(&flavors[*index].mime));

    let mut ordered: Vec<&Flavor> = canonical.map(|index| &flavors[index]).into_iter().collect();
    ordered.extend(
        flavors
            .iter()
            .enumerate()
            .filter(|(index, flavor)| Some(*index) != canonical && is_text_bearing(&flavor.mime))
            .map(|(_, flavor)| flavor),
    );
    ordered
}

/// Whether a flavor's bytes are text the detection rules should read.
///
/// Decided from the MIME essence and nothing else, so it is the same answer for
/// `text/plain`, `text/plain; charset=UTF-8` and a `text/*` type no one has
/// invented yet. Included: the whole `text/` tree — `text/plain`, `text/html`
/// and `text/uri-list` are what clippo stores, and a secret can be in any of
/// them.
///
/// Excluded, deliberately:
///
/// - **Image flavors**, including the derived thumbnail. Running the shape
///   regexes over PNG bytes costs a scan over megabytes on the capture path and
///   can only produce noise: the rules are about characters a human copied, and
///   a match inside compressed pixels would be a false positive by
///   construction.
/// - **The `x-kde-passwordManagerHint` marker**, whose presence is already
///   rule 1 and reaches [`describe`] as `hinted`. Its payload is not a value
///   the user copied.
///
/// A text flavor attached to an *image* entry still counts — some sources
/// advertise a filename or a URL beside a screenshot, and there is no reason a
/// token in one of those should be missed. Only the mask is kind-dependent.
fn is_text_bearing(mime: &str) -> bool {
    essence(mime).starts_with("text/")
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
/// [`describe`] builds a sensitive entry's mask from it and would otherwise
/// copy a multi-megabyte text copy to render twelve characters of it, on the
/// capture path, for every copy. `Cow` borrows for valid UTF-8, which is every
/// text flavor that is not corrupt.
///
/// The mask and `Reveal` reading the *same* function is the point, and not only
/// an optimisation: the value a row showed part of and the value revealing
/// returns must not be able to become two different strings.
///
/// What this is **not** is what detection reads. That is every text-bearing
/// flavor — see [`detect_over_flavors`] and the module docs — because a flavor this function
/// does not return is still a flavor a paste can deliver. Rendering one flavor
/// and judging all of them is the asymmetry the two doc comments exist to make
/// visible.
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

    /// A fixture-shaped OpenAI key, not a real one. Long enough to clear the
    /// shape rule's 16-character floor.
    const KEY: &str = "sk-Wq3nR8tVzLm2Ka7Jd0Xy5Pb1Ce6Gh4Fs9Ut8Nv2Iw7Qr3Zx";

    /// The audit's F3 scenario verbatim: a page writing an innocuous
    /// `text/plain` beside a `text/html` carrying the key. The row is the
    /// harmless line and the entry is an `html` one, but the flag — the only
    /// signal a frontend gives that this row is dangerous — must still be set.
    fn a_page_that_hides_a_key_in_its_html() -> Vec<Flavor> {
        vec![
            Flavor::new("text/plain;charset=utf-8", "Click to continue"),
            Flavor::new("text/html", format!("<code>{KEY}</code>")),
        ]
    }

    #[test]
    fn a_secret_in_a_flavor_the_preview_never_shows_is_still_flagged() {
        let flavors = a_page_that_hides_a_key_in_its_html();
        assert_eq!(EntryKind::for_flavors(&flavors), Some(EntryKind::Html));

        let described = describe(EntryKind::Html, &flavors, false, &secrets());
        assert!(described.sensitive);
        assert_eq!(
            described.signal,
            Some(Signal::Shape(secrets::Shape::OpenAiApiKey))
        );

        // The rendered preview is still the plain-text flavor — masked, in the
        // same shape every other masked row has, and with no part of the key
        // in it.
        assert_eq!(build(EntryKind::Html, &flavors), "Click to continue");
        assert_eq!(
            described.preview,
            "Cl\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}ue"
        );
        assert!(!described.preview.contains("sk-"), "{}", described.preview);
    }

    /// Detection reading more flavors must not move anything else: the kind is
    /// still `html`, the dedup hash is still BLAKE3 over the canonical flavor
    /// alone, and `Reveal` still answers with the whole of what the row showed.
    #[test]
    fn detecting_over_an_extra_flavor_changes_neither_identity_nor_reveal() {
        let flavors = a_page_that_hides_a_key_in_its_html();
        let markup_alone = vec![flavors[1].clone()];

        assert_eq!(EntryKind::for_flavors(&flavors), Some(EntryKind::Html));
        assert_eq!(
            dedup::hash(EntryKind::Html, &flavors),
            dedup::hash(EntryKind::Html, &markup_alone),
        );
        assert_eq!(
            reveal(EntryKind::Html, &flavors).unwrap(),
            "Click to continue"
        );
    }

    /// The other direction, and the reason this is a false-negative fix rather
    /// than a new source of false positives: an ordinary browser copy, whose
    /// markup is only markup, is described exactly as it was before.
    #[test]
    fn a_copy_whose_every_flavor_is_clean_is_still_not_flagged() {
        let flavors = vec![
            Flavor::new("text/plain;charset=utf-8", "Click to continue"),
            Flavor::new("text/html", "<b>Click to continue</b>"),
        ];
        let described = describe(EntryKind::Html, &flavors, false, &secrets());
        assert!(!described.sensitive);
        assert_eq!(described.signal, None);
        assert_eq!(described.preview, "Click to continue");
    }

    /// The stated rule for choosing between flavors that both fire: the
    /// highest-confidence signal wins, whichever flavor it came from, and only
    /// a tie is broken by flavor order. Asserted in the direction that tells
    /// the two rules apart — the **canonical** flavor merely looks random and
    /// the other one matches a shape, so deciding by flavor order would report
    /// `entropy` where the defensible answer is the shape.
    #[test]
    fn the_reported_signal_is_the_most_defensible_one_any_flavor_produced() {
        let canonical_is_weaker = vec![
            Flavor::new("text/html", "Xr4$Tp9!Lm2#Wq7&Zc5%"),
            Flavor::new("text/plain;charset=utf-8", KEY),
        ];
        assert_eq!(
            describe(EntryKind::Html, &canonical_is_weaker, false, &secrets()).signal,
            Some(Signal::Shape(secrets::Shape::OpenAiApiKey))
        );

        // The same both ways round, so the answer does not depend on the order
        // a compositor happened to advertise the flavors in.
        let canonical_is_stronger = vec![
            Flavor::new("text/plain;charset=utf-8", "Xr4$Tp9!Lm2#Wq7&Zc5%"),
            Flavor::new("text/html", format!("<code>{KEY}</code>")),
        ];
        assert_eq!(
            describe(EntryKind::Html, &canonical_is_stronger, false, &secrets()).signal,
            Some(Signal::Shape(secrets::Shape::OpenAiApiKey))
        );

        // …and the marker still outranks both, without either being read.
        assert_eq!(
            describe(EntryKind::Html, &canonical_is_stronger, true, &secrets()).signal,
            Some(Signal::PasswordManagerHint)
        );
    }

    /// Every text flavor, in the order the tie-break names: the canonical one
    /// first, then the rest as captured. The marker and the image are not text
    /// and are not in it.
    #[test]
    fn only_the_text_flavors_are_detected_over_and_the_canonical_one_leads() {
        let flavors = vec![
            Flavor::new("text/plain;charset=utf-8", "plain"),
            Flavor::new("image/png", vec![0_u8; 8]),
            Flavor::new("x-kde-passwordManagerHint", "secret"),
            Flavor::new("text/html", "markup"),
        ];
        let ordered: Vec<&str> = detection_flavors(EntryKind::Html, &flavors)
            .iter()
            .map(|flavor| flavor.mime.as_str())
            .collect();
        assert_eq!(ordered, ["text/html", "text/plain;charset=utf-8"]);

        assert!(is_text_bearing("text/plain; charset=UTF-8"));
        assert!(is_text_bearing("TEXT/uri-list"));
        assert!(!is_text_bearing("image/png"));
        assert!(!is_text_bearing("x-kde-passwordManagerHint"));
    }

    /// Image bytes never reach the regexes, so a token's characters appearing
    /// inside compressed pixels cannot flag a screenshot. Only the marker can.
    #[test]
    fn an_image_flavor_is_not_run_through_the_rules() {
        let flavors = vec![Flavor::new(
            "image/png",
            format!("AKIAIOSFODNN7EXAMPLE {KEY}"),
        )];
        let described = describe(EntryKind::Image, &flavors, false, &secrets());
        assert!(!described.sensitive);
        assert_eq!(described.signal, None);
    }

    /// A text flavor beside an image still counts — a source that advertises a
    /// URL or a filename next to a screenshot can advertise a token, and the
    /// flag is worth as much there. The preview stays the size line, which is
    /// the kind-dependent half.
    #[test]
    fn a_text_flavor_beside_an_image_is_still_detected_over() {
        let flavors = vec![
            Flavor::new("image/png", vec![0_u8; 2048]),
            Flavor::new("text/plain;charset=utf-8", KEY),
        ];
        let described = describe(EntryKind::Image, &flavors, false, &secrets());
        assert!(described.sensitive);
        assert_eq!(
            described.signal,
            Some(Signal::Shape(secrets::Shape::OpenAiApiKey))
        );
        assert_eq!(described.preview, "image/png, 2.0 KB");
    }

    #[test]
    fn byte_counts_read_the_way_max_image_bytes_is_written() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(8 * 1024 * 1024), "8.0 MB");
    }
}
