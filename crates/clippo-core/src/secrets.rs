//! Secret detection and masking — the differentiating feature.
//!
//! DESIGN.md gives this module its own section and its own test suite, because
//! it is the reason clippo exists rather than one of the clipboard managers
//! that already work. A clipboard history that renders passwords in plain text
//! in a panel is a password on a screen in an open-plan office.
//!
//! # Three signals, three separate rules
//!
//! Detection produces one `bool` — `entries.sensitive` — but it is deliberately
//! **not** one fused predicate. Each rule is its own module with its own
//! entry point, and [`detect`] returns *which* one fired:
//!
//! | Rule | Module | Confidence | Config |
//! |---|---|---|---|
//! | The `x-kde-passwordManagerHint` marker | [`hint`] | The application said so | always on |
//! | Provider-token shapes | [`shapes`] | Prefix match, near-zero false positives | always on |
//! | The entropy heuristic | [`entropy`] | A guess, tuned against the corpus | [`SecretsConfig::entropy_rule`] |
//!
//! Separability is a feature with a user-visible purpose: when somebody reports
//! that clippo flagged their UUID, the answer is a rule name and a fixture, not
//! an afternoon in a debugger. It is also what makes the escape hatch in
//! DESIGN.md's risk table possible — turning the entropy rule off leaves the
//! two rules that do not guess still working.
//!
//! # Masking is display-only
//!
//! [`mask()`] renders `ab••••••••yz` and nothing else changes: the value is
//! stored whole, `Copy` pastes it verbatim, and `Reveal` is the one member that
//! returns it. See [`masking`] for the two properties that matter — a
//! fixed-width bullet run, and short values hidden completely.
//!
//! # The corpus
//!
//! `crates/clippo-core/tests/corpus.toml` holds the fixture corpus DESIGN.md
//! requires, in both directions: real-shaped tokens and high-entropy secrets
//! that must be caught, and git SHAs, UUIDs, base64 blobs, minified JavaScript
//! and prose that must not be. `tests/corpus.rs` asserts the classification of
//! every fixture and names each miss. The entropy threshold is tuned against
//! it, and the tuning is written down next to
//! [`ENTROPY_THRESHOLD_BITS_PER_CHAR`][entropy::ENTROPY_THRESHOLD_BITS_PER_CHAR].

pub mod entropy;
pub mod hint;
pub mod masking;
pub mod shapes;

use std::fmt;

pub use hint::{is_password_manager_hint, PASSWORD_MANAGER_HINT_MIME};
pub use masking::{mask, mask_with, MASK_BULLET, MASK_BULLETS};
pub use shapes::Shape;

use crate::SecretsConfig;

/// How much of a copy the shape rules are run over, in bytes.
///
/// A copy can be a whole file. The shape patterns are linear-time, but linear
/// in a hundred megabytes is still time spent on the capture path, where the
/// user is waiting to see their copy appear. A credential sits at the front of
/// what people copy — a PEM block starts with its header, a connection string
/// is the whole value — so a prefix is where the answer is.
///
/// The entropy rule needs no cap: it refuses anything longer than 128
/// characters before it looks at the contents.
pub const DETECTION_MAX_BYTES: usize = 64 * 1024;

/// Which rule decided a value was a secret.
///
/// Carried into the daemon's log line so the reason a particular entry is
/// masked is recoverable from `journalctl` rather than from a rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// The copy carried the password-manager MIME marker. See [`hint`].
    PasswordManagerHint,
    /// The value matched a known provider-token shape. See [`shapes`].
    Shape(Shape),
    /// The value looks random. See [`entropy`].
    Entropy,
}

impl Signal {
    /// The stable name of the rule that fired, for logs and bug reports.
    ///
    /// `mime-hint`, `shape:<name>` or `entropy`. The corpus file names the rule
    /// it expects for each fixture in exactly this spelling.
    pub fn rule(self) -> String {
        match self {
            Self::PasswordManagerHint => "mime-hint".to_owned(),
            Self::Shape(shape) => format!("shape:{}", shape.name()),
            Self::Entropy => "entropy".to_owned(),
        }
    }
}

impl fmt::Display for Signal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.rule())
    }
}

/// Whether a captured copy is a suspected secret, and which rule says so.
///
/// `hinted` is [`hint::fires`]'s answer for the selection's MIME types. It is
/// the caller's to compute because only the capture path can see it: the marker
/// flavor may be advertised and never received, and it is not stored.
///
/// `text` is the copy's text, whole and unflattened. An image has none — pass
/// an empty string, and only the MIME marker can fire.
///
/// The rules are tried highest-confidence first, so the reported signal is the
/// most defensible one available rather than whichever happened to run last.
/// Nothing after the first match runs; a value is either suspected or it is
/// not, and the rest is only an explanation.
///
/// The value is trimmed before any rule sees it. Surrounding whitespace is not
/// part of a secret — `cat token | wl-copy` and double-clicking the last line
/// of a file both add a newline — and the entropy rule refuses anything that is
/// not a single token, so without this a trailing `\n` would be enough to hide
/// a password from it. That is a false negative bought for nothing.
pub fn detect(text: &str, hinted: bool, config: &SecretsConfig) -> Option<Signal> {
    if hinted {
        return Some(Signal::PasswordManagerHint);
    }
    let text = text.trim();
    if let Some(shape) = shapes::matched(head(text)) {
        return Some(Signal::Shape(shape));
    }
    if config.entropy_rule && entropy::fires(text) {
        return Some(Signal::Entropy);
    }
    None
}

/// Whether a captured copy is a suspected secret. [`detect`] without the why.
pub fn is_sensitive(text: &str, hinted: bool, config: &SecretsConfig) -> bool {
    detect(text, hinted, config).is_some()
}

/// The first [`DETECTION_MAX_BYTES`] of the text, cut at a character boundary.
///
/// Cutting mid-character would be a panic, so the cut walks back to the last
/// boundary at or before the limit — at most three bytes.
fn head(text: &str) -> &str {
    if text.len() <= DETECTION_MAX_BYTES {
        return text;
    }
    let mut end = DETECTION_MAX_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default() -> SecretsConfig {
        SecretsConfig::default()
    }

    #[test]
    fn each_rule_reports_itself_by_name() {
        assert_eq!(
            detect("anything at all", true, &default()),
            Some(Signal::PasswordManagerHint)
        );
        assert_eq!(
            detect("AKIAIOSFODNN7EXAMPLE", false, &default()),
            Some(Signal::Shape(Shape::AwsAccessKeyId))
        );
        assert_eq!(
            detect("mK8vT2qXw9RzB4nP", false, &default()),
            Some(Signal::Entropy)
        );
        assert_eq!(detect("a shopping list", false, &default()), None);

        assert_eq!(Signal::PasswordManagerHint.rule(), "mime-hint");
        assert_eq!(
            Signal::Shape(Shape::AwsAccessKeyId).rule(),
            "shape:aws-access-key-id"
        );
        assert_eq!(Signal::Entropy.rule(), "entropy");
        assert_eq!(Signal::Entropy.to_string(), "entropy");
    }

    /// The rule order is part of the contract: a password-manager copy that
    /// also matches a shape is reported as the marker, because the marker is
    /// the one signal that is not an inference.
    #[test]
    fn the_highest_confidence_rule_is_the_one_reported() {
        assert_eq!(
            detect("AKIAIOSFODNN7EXAMPLE", true, &default()),
            Some(Signal::PasswordManagerHint)
        );
        // A token that would also clear the entropy gates reports its shape.
        assert_eq!(
            detect("ghp_Rk8Wt2Nq6Xz4Lb9Mv3Cd7Yh1Ps5Fj0Ag", false, &default()),
            Some(Signal::Shape(Shape::GitHubToken))
        );
    }

    /// DESIGN.md's escape hatch: the knob turns off the guessing rule and
    /// leaves the other two alone. This is the config half of the acceptance
    /// criteria, asserted where the rules are combined.
    #[test]
    fn turning_off_the_entropy_rule_leaves_the_other_two_working() {
        let no_entropy = SecretsConfig {
            entropy_rule: false,
            ..default()
        };

        assert_eq!(detect("mK8vT2qXw9RzB4nP", false, &no_entropy), None);
        assert_eq!(
            detect("mK8vT2qXw9RzB4nP", true, &no_entropy),
            Some(Signal::PasswordManagerHint)
        );
        assert_eq!(
            detect("AKIAIOSFODNN7EXAMPLE", false, &no_entropy),
            Some(Signal::Shape(Shape::AwsAccessKeyId))
        );
    }

    /// A password copied out of a file arrives with the newline that ended the
    /// line. It is the same password.
    #[test]
    fn surrounding_whitespace_does_not_hide_a_secret() {
        for value in [
            "mK8vT2qXw9RzB4nP\n",
            "  mK8vT2qXw9RzB4nP  ",
            "\nmK8vT2qXw9RzB4nP\r\n",
        ] {
            assert_eq!(
                detect(value, false, &default()),
                Some(Signal::Entropy),
                "{value:?}"
            );
        }
        // Whitespace *inside* the value is a different thing, and still stops
        // the entropy rule: that is the single-token gate doing its job.
        assert_eq!(detect("mK8vT2q Xw9RzB4nP", false, &default()), None);
    }

    #[test]
    fn a_copy_with_no_text_can_still_carry_the_marker() {
        // An image copied out of a password manager: no text to inspect, and
        // one rule that does not need any.
        assert_eq!(detect("", false, &default()), None);
        assert_eq!(
            detect("", true, &default()),
            Some(Signal::PasswordManagerHint)
        );
    }

    #[test]
    fn is_sensitive_is_detect_without_the_reason() {
        assert!(is_sensitive("AKIAIOSFODNN7EXAMPLE", false, &default()));
        assert!(!is_sensitive("a shopping list", false, &default()));
    }

    #[test]
    fn only_the_first_chunk_of_a_huge_copy_is_scanned_and_the_cut_is_safe() {
        // The cut lands mid-character: `head` must walk back to a boundary
        // rather than panicking, and must not lose the front of the value.
        let mut huge = String::from("-----BEGIN RSA PRIVATE KEY-----\n");
        huge.push_str(&"é".repeat(DETECTION_MAX_BYTES));
        assert!(head(&huge).len() <= DETECTION_MAX_BYTES);
        assert!(head(&huge).starts_with("-----BEGIN"));
        assert_eq!(
            detect(&huge, false, &default()),
            Some(Signal::Shape(Shape::PrivateKeyBlock))
        );

        // A secret past the cap is out of reach, which is the documented cost.
        let mut buried = "x".repeat(DETECTION_MAX_BYTES);
        buried.push_str(" AKIAIOSFODNN7EXAMPLE");
        assert_eq!(detect(&buried, false, &default()), None);
    }
}
