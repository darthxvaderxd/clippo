//! The masking contract: `mask("supersecretvalue")` → `su••••••••ue`.
//!
//! **Display only.** Nothing here changes what is stored or what is pasted —
//! it produces the string a frontend shows in place of a suspected secret. The
//! value itself stays in the `flavors` table, whole, and leaves the daemon
//! through `Reveal` and through the copy-back path and nowhere else.
//!
//! Two properties are the whole point of the function, and both are tested
//! below:
//!
//! 1. **The bullet run is fixed width.** It is [`MASK_BULLETS`] bullets for a
//!    12-character password and [`MASK_BULLETS`] bullets for a 4096-character
//!    private key. A mask proportional to the input leaks the length of the
//!    value, and the length of a password is a real thing to know about it.
//! 2. **Short values are masked completely.** If there are no more characters
//!    than the visible prefix and suffix together, there is no middle to hide
//!    and the whole value would otherwise be on screen. Those are exactly the
//!    values that can least afford it.
//!
//! Both counts are configurable — [`SecretsConfig::mask_prefix`] and
//! [`SecretsConfig::mask_suffix`] — and the config loader already caps their
//! sum, so no setting can turn masking into an echo.

use unicode_segmentation::UnicodeSegmentation;

use crate::SecretsConfig;

/// The character the hidden middle is drawn with.
pub const MASK_BULLET: char = '\u{2022}';

/// How many bullets the hidden middle is, always.
///
/// Eight, which is what DESIGN.md's `ab••••••••yz` shows. Any fixed number
/// would satisfy the contract; this one is wide enough to read as "something
/// was hidden" and narrow enough to leave room for a preview column.
pub const MASK_BULLETS: usize = 8;

/// Mask a value for display, per the configured prefix and suffix.
///
/// ```
/// use clippo_core::{secrets::mask, SecretsConfig};
///
/// assert_eq!(mask("supersecretvalue", &SecretsConfig::default()), "su••••••••ue");
/// ```
pub fn mask(value: &str, config: &SecretsConfig) -> String {
    mask_with(value, config.mask_prefix, config.mask_suffix)
}

/// Mask a value with explicit prefix and suffix counts.
///
/// The counts are in **grapheme clusters**, not characters and not bytes.
/// Bytes would panic on multi-byte UTF-8; characters would not panic but would
/// cut inside a cluster, so that the first two "characters" of a flag emoji are
/// half of one symbol and the first two of `é` written as `e` + combining acute
/// are the letter and then the accent on its own. Cutting there renders as
/// something the user did not copy, and — for a value made of one long
/// cluster — could show most of it while claiming to show two characters.
///
/// A value with no more clusters than `prefix + suffix` is returned as bullets
/// alone. Empty input is bullets too: masking is display-only and never has to
/// answer for a value that is not there.
pub fn mask_with(value: &str, prefix: usize, suffix: usize) -> String {
    let visible = prefix.saturating_add(suffix);
    let bullets: String = std::iter::repeat(MASK_BULLET).take(MASK_BULLETS).collect();

    // Counting `visible + 1` clusters rather than all of them: the question is
    // only whether there is a middle to hide, and a whole-history `List` should
    // not walk a multi-megabyte copy to answer it.
    if value.graphemes(true).take(visible + 1).count() <= visible {
        return bullets;
    }

    let head: String = value.graphemes(true).take(prefix).collect();
    // `rev()` walks back from the end, so the tail costs `suffix` steps rather
    // than a full pass. It comes out backwards, hence the second reverse.
    let mut tail: Vec<&str> = value.graphemes(true).rev().take(suffix).collect();
    tail.reverse();

    let mut masked = String::with_capacity(head.len() + bullets.len() + suffix * 4);
    masked.push_str(&head);
    masked.push_str(&bullets);
    for cluster in tail {
        masked.push_str(cluster);
    }
    masked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(prefix: usize, suffix: usize) -> SecretsConfig {
        SecretsConfig {
            mask_prefix: prefix,
            mask_suffix: suffix,
            ..SecretsConfig::default()
        }
    }

    #[test]
    fn the_documented_example_is_what_comes_out() {
        assert_eq!(
            mask("supersecretvalue", &SecretsConfig::default()),
            "su\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}ue"
        );
    }

    /// Property 1: the mask is the same width whatever the value's length, so
    /// it cannot be read backwards for how long the password was.
    #[test]
    fn the_bullet_run_does_not_reveal_the_length_of_the_value() {
        let short = mask("abcde", &SecretsConfig::default());
        let long = mask(&"x".repeat(4096), &SecretsConfig::default());
        assert_eq!(short.chars().count(), long.chars().count());
        assert_eq!(short.chars().count(), 2 + MASK_BULLETS + 2);
        assert_eq!(
            short.chars().filter(|&c| c == MASK_BULLET).count(),
            MASK_BULLETS
        );
        assert_eq!(
            long.chars().filter(|&c| c == MASK_BULLET).count(),
            MASK_BULLETS
        );
    }

    /// Property 2: too short to have a middle means nothing is shown.
    #[test]
    fn a_value_no_longer_than_the_visible_ends_is_hidden_completely() {
        let default = SecretsConfig::default();
        for value in ["", "a", "ab", "abc", "abcd"] {
            let masked = mask(value, &default);
            assert_eq!(masked.chars().count(), MASK_BULLETS, "{value:?}");
            assert!(
                masked.chars().all(|c| c == MASK_BULLET),
                "{value:?} leaked as {masked}"
            );
        }
        // One character more than the ends, and the ends appear.
        assert!(mask("abcde", &default).starts_with("ab"));
        assert!(mask("abcde", &default).ends_with("de"));
    }

    #[test]
    fn multi_byte_characters_are_counted_not_sliced() {
        // Four bytes per character: a byte-index mask would panic here.
        let masked = mask("日本語のパスワード", &SecretsConfig::default());
        assert!(masked.starts_with("日本"), "{masked}");
        assert!(masked.ends_with("ード"), "{masked}");
        assert_eq!(masked.chars().count(), 2 + MASK_BULLETS + 2);
    }

    /// A value that is one grapheme cluster — a family emoji is four code
    /// points joined by zero-width joiners — must not be cut in the middle of
    /// it. By clusters it is one, which is fewer than prefix + suffix, so it is
    /// hidden completely; by `chars()` it would be cut after the first joiner.
    #[test]
    fn a_value_that_is_a_single_grapheme_is_not_cut_in_half() {
        let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
        assert!(family.chars().count() > 4);
        let masked = mask(family, &SecretsConfig::default());
        assert!(masked.chars().all(|c| c == MASK_BULLET), "{masked}");

        // The same for a combining accent: `e` + U+0301 is one cluster, so a
        // three-cluster value shows whole letters rather than a bare accent.
        let combined = "e\u{301}o\u{301}u\u{301}";
        let masked = mask_with(combined, 1, 1);
        assert_eq!(masked.graphemes(true).next(), Some("e\u{301}"));
        assert!(masked.ends_with("u\u{301}"), "{masked}");
    }

    #[test]
    fn zero_visible_characters_masks_everything() {
        let masked = mask("supersecretvalue", &config(0, 0));
        assert_eq!(masked.chars().count(), MASK_BULLETS);
        assert!(masked.chars().all(|c| c == MASK_BULLET), "{masked}");
    }

    #[test]
    fn the_visible_ends_follow_the_configuration() {
        assert_eq!(
            mask_with("supersecretvalue", 4, 0),
            "supe\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}"
        );
        assert_eq!(
            mask_with("supersecretvalue", 0, 3),
            "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}lue"
        );
        assert_eq!(
            mask("supersecretvalue", &config(3, 3)),
            "sup\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}lue"
        );
    }

    /// The prefix and suffix cannot overlap, however they are configured: the
    /// short-value branch has already returned by the time either is taken.
    #[test]
    fn the_two_ends_never_show_the_same_character_twice() {
        assert_eq!(mask_with("abcdefg", 4, 4).chars().count(), MASK_BULLETS);
        assert_eq!(mask_with("abcdefgh", 4, 4).chars().count(), MASK_BULLETS);
        let masked = mask_with("abcdefghi", 4, 4);
        assert_eq!(masked.chars().count(), 4 + MASK_BULLETS + 4);
        assert!(
            masked.starts_with("abcd") && masked.ends_with("fghi"),
            "{masked}"
        );
    }
}
