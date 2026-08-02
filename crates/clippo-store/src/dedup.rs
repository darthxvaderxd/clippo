//! What makes two copies the same copy.
//!
//! A real selection arrives as several flavors of one thing — a browser copy is
//! `text/html` *and* `text/plain`, a screenshot is `image/png` and often a
//! thumbnail as well. Hashing all of them would make dedup depend on which
//! flavors an application happened to advertise, so exactly one flavor per
//! entry is **canonical**, and BLAKE3 over that flavor is the entry's identity.
//!
//! # The canonical flavor, per kind
//!
//! Chosen from the flavors whose MIME type agrees with the entry's
//! [`EntryKind`], richest first. Where two candidates tie, the first in the
//! captured order wins, so the rule is a total one.
//!
//! | [`EntryKind`] | Canonical flavor, in order of preference |
//! |---|---|
//! | [`Text`][EntryKind::Text] | `text/plain;charset=utf-8`, then `text/plain`, then any other `text/*` |
//! | [`Html`][EntryKind::Html] | `text/html` |
//! | [`Uris`][EntryKind::Uris] | `text/uri-list` |
//! | [`Image`][EntryKind::Image] | `image/png`, then `image/jpeg`, then any other `image/*` |
//!
//! Two flavors are deliberately never canonical:
//!
//! - **The thumbnail**, [`THUMBNAIL_MIME`][crate::images::THUMBNAIL_MIME]. It is derived from the image beside
//!   it, so hashing it would key an entry on the output of clippo's own
//!   downscaler — change the thumbnail size and every entry in the history
//!   changes identity.
//! - **The `x-kde-passwordManagerHint` marker**, which carries no content of
//!   its own. [`EntryKind::from_mime`] already returns `None` for it, so it
//!   falls out of every row of the table above.
//!
//! # The hash
//!
//! ```text
//! BLAKE3( "clippo/entry/v1\0" || <canonical MIME essence> || "\0" || <bytes> )
//! ```
//!
//! The *essence* is the MIME type with its parameters stripped and lowercased,
//! so `text/plain`, `text/plain;charset=utf-8` and `text/plain; charset=UTF-8`
//! of the same bytes are one entry rather than three — the same normalisation
//! `clippo-wayland` applies when it decides a flavor is interesting. The MIME
//! is in the hash at all so that identical bytes under genuinely different
//! types stay distinct entries. The `clippo/entry/v1` prefix is domain
//! separation: it keeps these hashes from colliding with any other BLAKE3 use,
//! and gives the rule a version to change should the canonical choice ever
//! need to.
//!
//! Note what the hash is *not*: it is unsalted, and deliberately so — it has to
//! be stable across restarts to dedup at all. It is therefore a confirmation
//! oracle for a guessed clipboard value, which is why `clippo-core` truncates
//! it in `Debug` and why it lives inside the encrypted database rather than
//! anywhere a log line can reach.

use clippo_core::{EntryKind, Flavor};

use crate::images::is_thumbnail;

/// The domain-separation prefix mixed into every entry hash.
const HASH_DOMAIN: &[u8] = b"clippo/entry/v1\0";

/// The canonical flavor of a selection, or `None` if it has none.
///
/// `None` means the flavors do not contain anything of the kind claimed —
/// a selection of nothing but a thumbnail or a password-manager marker, say.
/// The caller cannot store such a selection, because it has no identity.
///
/// Takes anything that iterates flavors rather than a slice, so the store can
/// ask the same question of the borrowed, already-filtered list it is about to
/// write without cloning a multi-megabyte blob to do it.
pub fn canonical_flavor<'a, I>(kind: EntryKind, flavors: I) -> Option<&'a Flavor>
where
    I: IntoIterator<Item = &'a Flavor>,
{
    let mut best: Option<(u8, &Flavor)> = None;
    for flavor in flavors {
        let Some(rank) = rank(kind, &flavor.mime) else {
            continue;
        };
        // Strictly greater, so an earlier flavor wins a tie and the rule stays
        // independent of how a compositor happened to order the offer.
        let better = match best {
            None => true,
            Some((best_rank, _)) => rank > best_rank,
        };
        if better {
            best = Some((rank, flavor));
        }
    }
    best.map(|(_, flavor)| flavor)
}

/// The dedup hash of a selection as lowercase hex, or `None` if it has no
/// canonical flavor.
///
/// This is what goes in `entries.hash`, the column the `UNIQUE` constraint sits
/// on.
pub fn hash(kind: EntryKind, flavors: &[Flavor]) -> Option<String> {
    canonical_flavor(kind, flavors).map(hash_flavor)
}

/// The dedup hash of one flavor treated as canonical.
pub fn hash_flavor(flavor: &Flavor) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(HASH_DOMAIN);
    hasher.update(essence(&flavor.mime).as_bytes());
    hasher.update(b"\0");
    hasher.update(&flavor.data);
    hasher.finalize().to_hex().to_string()
}

/// A MIME type with its parameters stripped, whitespace trimmed and lowercased.
///
/// `text/plain; charset=UTF-8` → `text/plain`.
fn essence(mime: &str) -> String {
    let essence = match mime.split_once(';') {
        Some((essence, _parameters)) => essence,
        None => mime,
    };
    essence.trim().to_ascii_lowercase()
}

/// How good a candidate this MIME type is for an entry of this kind — higher is
/// better, `None` means "not a candidate at all".
///
/// This is the table in the module docs, written out.
fn rank(kind: EntryKind, mime: &str) -> Option<u8> {
    if is_thumbnail(mime) {
        return None;
    }
    // A flavor can only be canonical for the kind it implies, so this rejects
    // the `text/plain` alongside a `text/html` copy as well as the marker.
    if EntryKind::from_mime(mime)? != kind {
        return None;
    }

    let essence = essence(mime);
    let utf8_text = mime
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
        .contains("charset=utf-8");

    Some(match kind {
        EntryKind::Text if essence == "text/plain" && utf8_text => 3,
        EntryKind::Text if essence == "text/plain" => 2,
        EntryKind::Text => 1,
        EntryKind::Html | EntryKind::Uris => 1,
        EntryKind::Image if essence == "image/png" => 3,
        EntryKind::Image if essence == "image/jpeg" => 2,
        EntryKind::Image => 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flavors(pairs: &[(&str, &str)]) -> Vec<Flavor> {
        pairs
            .iter()
            .map(|(mime, data)| Flavor::new(*mime, *data))
            .collect()
    }

    fn canonical_mime(kind: EntryKind, flavors: &[Flavor]) -> Option<&str> {
        canonical_flavor(kind, flavors).map(|flavor| flavor.mime.as_str())
    }

    #[test]
    fn the_utf8_text_flavor_is_canonical_for_a_text_copy() {
        let captured = flavors(&[
            ("text/plain", "hi"),
            ("text/plain;charset=utf-8", "hi"),
            ("x-kde-passwordManagerHint", "secret"),
        ]);
        assert_eq!(
            canonical_mime(EntryKind::Text, &captured),
            Some("text/plain;charset=utf-8")
        );
    }

    #[test]
    fn plain_text_is_canonical_when_there_is_no_utf8_spelling() {
        let captured = flavors(&[("text/rtf", "hi"), ("text/plain", "hi")]);
        assert_eq!(
            canonical_mime(EntryKind::Text, &captured),
            Some("text/plain")
        );

        // ... and any other text flavor when there is no `text/plain` either.
        let captured = flavors(&[("text/rtf", "hi")]);
        assert_eq!(canonical_mime(EntryKind::Text, &captured), Some("text/rtf"));
    }

    #[test]
    fn the_richest_flavor_is_canonical_for_the_other_kinds() {
        let browser = flavors(&[("text/html", "<b>hi</b>"), ("text/plain", "hi")]);
        assert_eq!(canonical_mime(EntryKind::Html, &browser), Some("text/html"));

        let files = flavors(&[("text/uri-list", "file:///a"), ("text/plain", "/a")]);
        assert_eq!(
            canonical_mime(EntryKind::Uris, &files),
            Some("text/uri-list")
        );

        let screenshot = flavors(&[("image/jpeg", "jpg"), ("image/png", "png")]);
        assert_eq!(
            canonical_mime(EntryKind::Image, &screenshot),
            Some("image/png")
        );
    }

    #[test]
    fn the_derived_thumbnail_is_never_canonical() {
        // Even when it is the only image flavor small enough to have survived,
        // hashing it would key the entry on clippo's own downscaler.
        let captured = flavors(&[
            (crate::images::THUMBNAIL_MIME, "thumb"),
            ("image/jpeg", "jpg"),
        ]);
        assert_eq!(
            canonical_mime(EntryKind::Image, &captured),
            Some("image/jpeg")
        );

        let only_a_thumbnail = flavors(&[("image/png; clippo-thumb", "thumb")]);
        assert_eq!(canonical_mime(EntryKind::Image, &only_a_thumbnail), None);
    }

    #[test]
    fn a_selection_with_nothing_of_its_kind_has_no_canonical_flavor() {
        let marker_only = flavors(&[("x-kde-passwordManagerHint", "secret")]);
        assert_eq!(canonical_flavor(EntryKind::Text, &marker_only), None);
        assert_eq!(hash(EntryKind::Text, &marker_only), None);
        assert_eq!(canonical_flavor(EntryKind::Image, &[]), None);
    }

    #[test]
    fn ties_go_to_the_flavor_that_was_captured_first() {
        let captured = flavors(&[("image/webp", "first"), ("image/avif", "second")]);
        assert_eq!(
            canonical_mime(EntryKind::Image, &captured),
            Some("image/webp")
        );
    }

    #[test]
    fn the_same_text_spelled_two_ways_hashes_the_same() {
        // The point of hashing the essence rather than the MIME as advertised:
        // two applications that spell the charset parameter differently must
        // not produce two history entries for one string.
        let first = hash(
            EntryKind::Text,
            &flavors(&[("text/plain;charset=utf-8", "hi")]),
        );
        let second = hash(
            EntryKind::Text,
            &flavors(&[("text/plain; charset=UTF-8", "hi")]),
        );
        assert_eq!(first, second);
        assert!(first.is_some());
    }

    #[test]
    fn different_bytes_and_different_types_hash_differently() {
        let text = hash(EntryKind::Text, &flavors(&[("text/plain", "hi")])).unwrap();
        let other = hash(EntryKind::Text, &flavors(&[("text/plain", "ho")])).unwrap();
        let html = hash(EntryKind::Html, &flavors(&[("text/html", "hi")])).unwrap();
        assert_ne!(text, other);
        assert_ne!(text, html, "identical bytes under a different type");
    }

    #[test]
    fn the_hash_is_lowercase_blake3_hex() {
        let hash = hash_flavor(&Flavor::new("text/plain", "hi"));
        assert_eq!(hash.len(), 64);
        assert!(hash
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));

        // Pinned so the identity of an entry cannot drift silently: changing
        // the domain prefix or the framing would change every stored hash and
        // silently split the history in two.
        let mut expected = blake3::Hasher::new();
        expected.update(b"clippo/entry/v1\0");
        expected.update(b"text/plain\0hi");
        assert_eq!(hash, expected.finalize().to_hex().to_string());
    }

    #[test]
    fn a_flavor_that_belongs_to_another_kind_is_not_a_candidate() {
        // The `text/plain` of a browser copy must not become the canonical
        // flavor of an entry the store has recorded as HTML.
        assert_eq!(rank(EntryKind::Html, "text/plain"), None);
        assert_eq!(rank(EntryKind::Text, "text/html"), None);
        assert_eq!(rank(EntryKind::Image, "text/plain"), None);
        assert_eq!(rank(EntryKind::Text, "x-kde-passwordManagerHint"), None);
    }
}
