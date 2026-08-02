//! Rule 3 — the entropy heuristic, and the only rule that guesses.
//!
//! Four gates, all of which must pass, verbatim from DESIGN.md: a single token
//! with no whitespace, [`MIN_CHARS`]–[`MAX_CHARS`] characters, at least
//! [`MIN_CHARACTER_CLASSES`] character classes, and Shannon entropy above
//! [`ENTROPY_THRESHOLD_BITS_PER_CHAR`]. Plus one gate the corpus added,
//! [`is_locator`], for the false positive those four do not catch.
//!
//! Each gate is a public function, so a report of "clippo flagged my UUID" can
//! be answered by evaluating them one at a time against the value rather than
//! by reading this file. This is also the rule behind
//! [`SecretsConfig::entropy_rule`][crate::SecretsConfig::entropy_rule], the
//! escape hatch DESIGN.md's risk table asks for — turning it off leaves the
//! MIME and shape rules, which do not guess, still working.

use std::collections::HashMap;

/// Shortest value the rule will look at.
///
/// DESIGN.md's floor. Worth knowing that the threshold makes it stricter than
/// it looks: the most entropy a string of *n* characters can have is log₂(n)
/// bits/char, so nothing shorter than 12 characters can clear
/// [`ENTROPY_THRESHOLD_BITS_PER_CHAR`] however random it is. Twelve is the real
/// floor, and this constant is the one DESIGN.md wrote down.
///
/// That leaves a gap — an 8-to-11-character password is out of this rule's
/// reach — and the gap is deliberate rather than unnoticed: closing it means a
/// threshold below 3.0, which flags every short identifier a developer copies.
/// Short passwords come from password managers, and a password manager sets the
/// MIME marker that [`super::hint`] reads without guessing at all.
pub const MIN_CHARS: usize = 8;

/// Longest value the rule will look at.
///
/// DESIGN.md's ceiling, and the gate that does most of the false-positive work
/// in the corpus: a minified JavaScript bundle and a base64-encoded blob are
/// both long single tokens of high entropy, and both run past this. Real
/// credentials do not — the longest in the corpus is a `github_pat_` at 93
/// characters. A PEM private key is longer still, and is caught by its shape.
pub const MAX_CHARS: usize = 128;

/// How many of the character classes below a value must use.
pub const MIN_CHARACTER_CLASSES: usize = 3;

/// The threshold, in bits per character. Above this, the value is suspected.
///
/// **Tuned against `crates/clippo-core/tests/corpus.toml`**, the fixture corpus
/// DESIGN.md requires, and that corpus is the only justification this number
/// has. Measured over it — the figures below are asserted in
/// `the_documented_entropy_figures_are_the_ones_this_computes`, so they cannot
/// drift from what the code computes:
///
/// | Fixture | bits/char | Fires |
/// |---|---|---|
/// | 20-character generated password | 4.32 | yes |
/// | 16-character generated password | 4.00 | yes |
/// | 12-character generated password | 3.59 | yes |
/// | **threshold** | **3.50** | |
/// | ISO-8601 timestamp | 3.49 | no |
/// | short base64 string | 3.45 | no |
/// | joined English words | 3.36 | no |
///
/// The nearest true positive is at 3.59 and the nearest true negative at 3.49,
/// so 3.5 sits in a real gap rather than on top of the data. The gap is narrow
/// on purpose: a 12-character value has at most log₂(12) = 3.59 bits/char
/// available, so any threshold above that misses 12-character passwords
/// entirely, and a missed secret is the failure DESIGN.md names as the actual
/// risk. Below 3.4 the timestamps and short base64 come in with it.
///
/// Two shapes that *would* land inside the gap's upper half — a git SHA at 3.95
/// and a UUID at 4.02 — never reach this comparison. They are stopped by
/// [`character_classes`]: hex is two classes, and a UUID's hyphens do not buy
/// it a third. Separating those by entropy alone would need a threshold above
/// 4.0, which flags almost nothing. The same is true of a long URL, which
/// [`is_locator`] stops.
///
/// Move this number and run `cargo test -p clippo-core`: the corpus test names
/// every fixture that changed sides, and fails loudest on a secret it now
/// misses.
pub const ENTROPY_THRESHOLD_BITS_PER_CHAR: f64 = 3.5;

/// Whether the entropy heuristic fires for this value.
///
/// The gates are evaluated cheapest-first, so the overwhelmingly common case —
/// a copy with a space in it — costs one scan and no allocation.
pub fn fires(value: &str) -> bool {
    is_single_token(value)
        && !is_locator(value)
        && length_is_in_range(value)
        && character_classes(value) >= MIN_CHARACTER_CLASSES
        && shannon_bits_per_char(value) > ENTROPY_THRESHOLD_BITS_PER_CHAR
}

/// Whether the value is a URL or a filesystem path — structure, not randomness.
///
/// The gate the corpus added. A URL is a single token of mixed case, digits and
/// punctuation, and a long one scores 4.4 bits/char: without this,
/// `https://github.com/nilfactor/clippo/blob/main/docs/DESIGN.md` is flagged,
/// and so is every other link anybody copies. A clipboard history that renders
/// `ht••••••••md` where the user's links should be is not a working clipboard
/// history, and DESIGN.md's own list of things to copy is full of them —
/// `text/uri-list` is one of the flavors clippo goes out of its way to capture.
///
/// It is safe to exempt these here because it happens *after* the shape rules,
/// which is where a locator carrying a credential is caught: a
/// `postgres://user:pass@host/db` matches `shape:database-url-password`, and an
/// API token or JWT in a query string matches its own provider's shape. What
/// gets through is the case DESIGN.md's three signals never claimed to cover —
/// an opaque high-entropy token in a link that matches no known provider, a
/// password-reset URL being the obvious one. That is a real false negative, it
/// is written down here rather than discovered later, and closing it means a
/// fourth signal rather than a different threshold.
pub fn is_locator(value: &str) -> bool {
    if let Some(scheme) = value.split("://").next().filter(|_| value.contains("://")) {
        let mut characters = scheme.chars();
        let starts_well = characters
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic());
        let rest_is_scheme =
            characters.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '.' || c == '-');
        if starts_well && rest_is_scheme {
            return true;
        }
    }
    // Absolute, relative and home-relative paths. clippo is a Linux clipboard
    // manager, so these are the three spellings there are.
    value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with("~/")
}

/// Whether the value is one token: non-empty, and no whitespace anywhere.
///
/// This is the gate that excludes prose, and it excludes it completely — a
/// secret with a space in it is not something this rule can see, which is what
/// the other two rules are for. Control characters count as whitespace here:
/// a value split across lines is not one token either.
pub fn is_single_token(value: &str) -> bool {
    !value.is_empty()
        && !value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
}

/// Whether the value's length in *characters* is in range.
///
/// Characters rather than bytes, so that a value of accented text is measured
/// the way a reader would count it rather than by its UTF-8 encoding.
pub fn length_is_in_range(value: &str) -> bool {
    let chars = value.chars().count();
    (MIN_CHARS..=MAX_CHARS).contains(&chars)
}

/// How many character classes the value draws on, out of four: lowercase,
/// uppercase, digits, and everything else.
///
/// **`-` and `_` are structure, not a class.** They are separators — they carry
/// no entropy and they appear in UUIDs, ISO dates, kebab-case identifiers and
/// snake\_case names, all of which people copy constantly. Counting them would
/// give a canonical UUID three classes (lowercase, digit, separator) and, at
/// 4.0 bits/char, a flag; DESIGN.md's own false-positive check copies a UUID
/// and expects nothing to happen. Real credentials that use separators — a
/// `github_pat_`, a Slack token — are caught by their shape, one rule earlier,
/// and generated passwords that use them are mixed-case anyway.
pub fn character_classes(value: &str) -> usize {
    const LOWER: u8 = 1 << 0;
    const UPPER: u8 = 1 << 1;
    const DIGIT: u8 = 1 << 2;
    const OTHER: u8 = 1 << 3;

    let mut seen = 0_u8;
    for character in value.chars() {
        seen |= if character.is_numeric() {
            DIGIT
        } else if character.is_lowercase() {
            LOWER
        } else if character.is_uppercase() {
            UPPER
        } else if character == '-' || character == '_' {
            continue;
        } else {
            OTHER
        };
    }
    seen.count_ones() as usize
}

/// Shannon entropy of the value, in bits per character.
///
/// `-Σ p·log₂ p` over the distribution of the characters actually present. This
/// measures how *varied* a string is, not how hard it is to guess — a
/// 40-character hex hash scores well because hex digits are evenly spread, even
/// though there are only sixteen of them. That is exactly why the class gate
/// exists, and why this number is never asked on its own.
///
/// Empty input is zero rather than a division by zero.
pub fn shannon_bits_per_char(value: &str) -> f64 {
    let mut frequencies: HashMap<char, usize> = HashMap::new();
    let mut total = 0_usize;
    for character in value.chars() {
        *frequencies.entry(character).or_insert(0) += 1;
        total += 1;
    }
    if total == 0 {
        return 0.0;
    }

    let total = total as f64;
    -frequencies
        .values()
        .map(|&count| {
            let p = count as f64 / total;
            p * p.log2()
        })
        .sum::<f64>()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tuning table in [`ENTROPY_THRESHOLD_BITS_PER_CHAR`]'s doc comment,
    /// asserted. A threshold justified by numbers in a comment is justified by
    /// nothing unless the numbers are checked.
    #[test]
    fn the_documented_entropy_figures_are_the_ones_this_computes() {
        for (value, expected) in [
            ("Xr4$Tp9!Lm2#Wq7&Zc5%", 4.32),      // 20-character password
            ("mK8vT2qXw9RzB4nP", 4.00),          // 16-character password
            ("aB3!xY9zQ7#w", 3.59),              // 12-character password
            ("2026-08-02T04:51:06.489Z", 3.49),  // ISO-8601 timestamp
            ("SGVsbG8gV29ybGQh", 3.45),          // short base64
            ("correcthorsebatterystaple", 3.36), // joined English words
        ] {
            // The table is written to two decimal places, so that is the
            // tolerance: this catches a figure that is wrong, not one rounded.
            let measured = shannon_bits_per_char(value);
            assert!(
                (measured - expected).abs() < 0.01,
                "{value:?} measures {measured:.3}, the doc comment says {expected:.2}"
            );
        }

        // The gap the threshold sits in: the three passwords are above it and
        // the three ordinary strings below.
        assert!(fires("Xr4$Tp9!Lm2#Wq7&Zc5%"));
        assert!(fires("mK8vT2qXw9RzB4nP"));
        assert!(fires("aB3!xY9zQ7#w"));
        assert!(!fires("2026-08-02T04:51:06.489Z"));
        assert!(!fires("SGVsbG8gV29ybGQh"));
        assert!(!fires("correcthorsebatterystaple"));
    }

    /// The two shapes the *class* gate handles rather than the threshold, also
    /// as documented: both score well above 3.5 and neither fires.
    #[test]
    fn a_git_sha_and_a_uuid_are_stopped_by_the_class_gate_not_the_threshold() {
        let sha = "e2f9b1c4a7d63805fe1a2b3c4d5e6f7089abcdef";
        let uuid = "3f2b9c7e-8a41-4d6b-95e0-1c7a2f8d4b63";

        assert!((shannon_bits_per_char(sha) - 3.95).abs() < 0.005);
        assert!((shannon_bits_per_char(uuid) - 4.02).abs() < 0.005);
        assert_eq!(character_classes(sha), 2);
        assert_eq!(character_classes(uuid), 2);
        assert!(!fires(sha));
        assert!(!fires(uuid));
    }

    /// …and the one the locator gate handles, for the same reason: high
    /// entropy, three classes, in range, and not a secret.
    #[test]
    fn a_url_is_stopped_by_the_locator_gate() {
        let url = "https://github.com/nilfactor/clippo/blob/main/docs/DESIGN.md";
        assert!(shannon_bits_per_char(url) > ENTROPY_THRESHOLD_BITS_PER_CHAR);
        assert!(character_classes(url) >= MIN_CHARACTER_CLASSES);
        assert!(length_is_in_range(url));
        assert!(is_locator(url));
        assert!(!fires(url));
    }

    #[test]
    fn the_locator_gate_knows_a_url_or_a_path_from_a_token() {
        assert!(is_locator("https://example.com/x"));
        assert!(is_locator("HTTPS://EXAMPLE.COM/X"));
        assert!(is_locator("file:///home/richard/notes.md"));
        assert!(is_locator("/home/Richard/Projects/Clippo7"));
        assert!(is_locator("./target/debug/clippod"));
        assert!(is_locator("../crates/clippo-core"));
        assert!(is_locator("~/.local/share/clippo/history.db"));

        assert!(!is_locator("mK8vT2qXw9RzB4nP"));
        assert!(!is_locator("ghp_Rk8Wt2Nq6Xz4Lb9Mv3Cd7Yh1Ps5Fj0Ag"));
        assert!(!is_locator("home/richard/notes.md"));
        // Not a scheme: `://` has to be preceded by one.
        assert!(!is_locator("Xr4$Tp://9!Lm2#Wq7"));
    }

    #[test]
    fn entropy_is_bits_per_character_over_the_characters_present() {
        // Two equally likely symbols: exactly one bit each.
        assert!((shannon_bits_per_char("abab") - 1.0).abs() < 1e-9);
        // Four equally likely symbols: two bits each.
        assert!((shannon_bits_per_char("abcdabcd") - 2.0).abs() < 1e-9);
        // One symbol carries no information at all.
        assert_eq!(shannon_bits_per_char("aaaaaaaa"), 0.0);
        assert_eq!(shannon_bits_per_char(""), 0.0);
    }

    #[test]
    fn a_value_with_whitespace_is_not_a_single_token() {
        assert!(is_single_token("Tq7vLp2Nx9Wz4Km"));
        assert!(!is_single_token("Tq7vLp2 Nx9Wz4Km"));
        assert!(!is_single_token("Tq7vLp2\nNx9Wz4Km"));
        assert!(!is_single_token("Tq7vLp2\u{7}Nx9"));
        assert!(!is_single_token(""));
    }

    #[test]
    fn the_length_gate_is_design_mds_range_in_characters() {
        assert!(!length_is_in_range(&"a".repeat(MIN_CHARS - 1)));
        assert!(length_is_in_range(&"a".repeat(MIN_CHARS)));
        assert!(length_is_in_range(&"a".repeat(MAX_CHARS)));
        assert!(!length_is_in_range(&"a".repeat(MAX_CHARS + 1)));
        // Characters, not bytes: 128 accented characters are 256 bytes.
        assert!(length_is_in_range(&"é".repeat(MAX_CHARS)));
    }

    #[test]
    fn separators_do_not_count_as_a_character_class() {
        assert_eq!(character_classes("abcdef"), 1);
        assert_eq!(character_classes("abcDEF"), 2);
        assert_eq!(character_classes("abcDEF123"), 3);
        assert_eq!(character_classes("abcDEF123!"), 4);
        // A UUID: lowercase and digits, with hyphens that buy it nothing.
        assert_eq!(character_classes("550e8400-e29b-41d4-a716-446655440000"), 2);
        assert_eq!(character_classes("kebab-case-name"), 1);
        assert_eq!(character_classes("SCREAMING_SNAKE_CASE"), 1);
        assert_eq!(character_classes(""), 0);
    }

    /// The interaction the `MIN_CHARS` doc describes: a value of *n* characters
    /// cannot exceed log₂(n) bits/char, so the threshold puts the real floor at
    /// twelve characters and not at eight.
    #[test]
    fn nothing_shorter_than_twelve_characters_can_clear_the_threshold() {
        assert!(!fires("aB3!xY9")); // 7 characters: below MIN_CHARS outright.
        assert!(!fires("aB3!xY9z")); // 8 distinct characters: exactly 3.0 bits.
        assert!(fires("aB3!xY9zQ7#w")); // 12 distinct: 3.58 bits, and it fires.
    }

    #[test]
    fn a_long_token_is_above_the_ceiling_however_random_it_looks() {
        let long = "aB3!xY9z".repeat(20);
        assert!(long.chars().count() > MAX_CHARS);
        assert!(!fires(&long));
    }
}
