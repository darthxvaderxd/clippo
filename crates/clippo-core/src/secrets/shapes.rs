//! Rule 2 — the provider-token shapes.
//!
//! Cheap, precise, and easy to extend: a token that says what it is on the
//! front is not a heuristic problem. Every pattern here is anchored on a
//! prefix nobody types by accident (`sk-`, `ghp_`, `AKIA`, `xox`, `eyJ`,
//! `-----BEGIN`), so the false-positive rate is as close to zero as detection
//! gets, and none of these is affected by
//! [`SecretsConfig::entropy_rule`][crate::SecretsConfig::entropy_rule].
//!
//! DESIGN.md names seven; this module implements those seven, each widened
//! slightly to the rest of its provider's family where the family shares the
//! shape (`gho_`/`ghu_`/`ghs_` alongside `ghp_`, `ASIA` alongside `AKIA`,
//! `mysql://` alongside `postgres://`). Widening within a family is free —
//! it is the same prefix rule with one more alternative — and a missed secret
//! is the failure that matters.
//!
//! # Adding one
//!
//! Add a variant to [`Shape`], its kebab-case name to [`Shape::name`], its
//! pattern to [`Shape::pattern`], and a fixture to
//! `crates/clippo-core/tests/corpus.toml`. The corpus test fails if the shape
//! has no fixture, so the list here and the list there cannot drift.

use std::fmt;
use std::sync::OnceLock;

use regex::Regex;

/// A provider-token shape, named so a flagged entry can be explained.
///
/// The name is what reaches a log line and a bug report: "clippo flagged this
/// under `shape:aws-access-key-id`" is a complete answer, where "clippo thinks
/// this is a secret" is the start of an argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Shape {
    /// OpenAI-style API key: `sk-…`, including `sk-proj-…`.
    OpenAiApiKey,
    /// GitHub token: `ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`.
    GitHubToken,
    /// AWS access key id: `AKIA…`, and the `ASIA…` temporary form.
    AwsAccessKeyId,
    /// Slack token: `xoxb-`, `xoxa-`, `xoxp-`, `xoxr-`, `xoxs-`.
    SlackToken,
    /// A JSON Web Token: `eyJ` and two dot-separated segments after it.
    Jwt,
    /// A PEM private key block header, of any key type.
    PrivateKeyBlock,
    /// A database URL carrying a password: `postgres://user:pass@…`.
    DatabaseUrlPassword,
}

impl Shape {
    /// Every shape, in the order [`matched`] tries them.
    pub const ALL: &'static [Shape] = &[
        Shape::OpenAiApiKey,
        Shape::GitHubToken,
        Shape::AwsAccessKeyId,
        Shape::SlackToken,
        Shape::Jwt,
        Shape::PrivateKeyBlock,
        Shape::DatabaseUrlPassword,
    ];

    /// The name this shape is reported under, kebab-case and stable.
    pub const fn name(self) -> &'static str {
        match self {
            Self::OpenAiApiKey => "openai-api-key",
            Self::GitHubToken => "github-token",
            Self::AwsAccessKeyId => "aws-access-key-id",
            Self::SlackToken => "slack-token",
            Self::Jwt => "jwt",
            Self::PrivateKeyBlock => "private-key-block",
            Self::DatabaseUrlPassword => "database-url-password",
        }
    }

    /// The pattern, as a regex the `regex` crate accepts.
    ///
    /// Searched, not anchored: a connection string or a PEM header is usually
    /// part of a longer copy, and requiring the whole value to be the token
    /// would miss every one of those. The prefixes are what keep that safe,
    /// helped by `\b` where the prefix is bare letters.
    pub const fn pattern(self) -> &'static str {
        match self {
            // Two alternatives rather than one optional `proj-`: a project key
            // really does contain `-` and `_`, and a plain one really is
            // base62. Allowing separators in both would match `sk-` followed by
            // any long kebab-case identifier. 16 is well under a real key's 48
            // characters; the floor is only there to keep `sk-` in prose out.
            Self::OpenAiApiKey => r"\bsk-(?:proj-[A-Za-z0-9_-]{20,}|[A-Za-z0-9]{16,})",
            Self::GitHubToken => r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})",
            // Exactly 16 characters after the prefix: AWS ids are fixed-length,
            // so this is the one shape that can afford to be exact.
            Self::AwsAccessKeyId => r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
            Self::SlackToken => r"\bxox[baprs]-[A-Za-z0-9-]{10,}",
            // The third segment may be empty — an `alg: none` JWT is still a
            // JWT, and is if anything more interesting.
            Self::Jwt => r"\beyJ[A-Za-z0-9_=-]+\.[A-Za-z0-9_=-]+\.[A-Za-z0-9_=-]*",
            // `BEGIN … PRIVATE KEY`: RSA, EC, OPENSSH, ENCRYPTED, or none of
            // them for a bare PKCS#8 block.
            Self::PrivateKeyBlock => r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----",
            // A password is required: `postgres://localhost/db` is a URL
            // somebody may well have in a README, and carries nothing secret.
            Self::DatabaseUrlPassword => {
                r"(?i)\b(?:postgres|postgresql|mysql|mariadb|mongodb(?:\+srv)?|redis|rediss|amqp)://[^\s:/@]+:[^\s:/@]+@"
            }
        }
    }
}

impl fmt::Display for Shape {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Which shape the value matches, if any, in [`Shape::ALL`] order.
///
/// The first match wins and the rest are not tried: the answer is "which rule
/// fired", and a value matching two of these is a value nobody needs a second
/// opinion on.
pub fn matched(value: &str) -> Option<Shape> {
    compiled()
        .iter()
        .find(|(_, pattern)| pattern.is_match(value))
        .map(|(shape, _)| *shape)
}

/// The compiled patterns, built once on first use.
///
/// A `OnceLock` rather than compiling per call: `Regex::new` is milliseconds
/// and this runs on the capture path, once per copy. `expect` is sound here in
/// the way it usually is not — the patterns are `&'static str` literals in this
/// file, so a failure is a compile-time typo that the unit tests below catch,
/// not anything a user's clipboard can cause.
fn compiled() -> &'static [(Shape, Regex)] {
    static COMPILED: OnceLock<Vec<(Shape, Regex)>> = OnceLock::new();
    COMPILED.get_or_init(|| {
        Shape::ALL
            .iter()
            .map(|&shape| {
                let pattern = Regex::new(shape.pattern()).unwrap_or_else(|error| {
                    panic!("the {shape} pattern does not compile: {error}")
                });
                (shape, pattern)
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pattern_compiles_and_has_a_distinct_name() {
        assert_eq!(compiled().len(), Shape::ALL.len());
        let mut names: Vec<&str> = Shape::ALL.iter().map(|shape| shape.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Shape::ALL.len());
    }

    #[test]
    fn each_shape_matches_its_own_provider() {
        // The whole corpus lives in tests/corpus.toml; these are the one-line
        // checks that say which pattern is which.
        assert_eq!(
            matched("sk-Wq3nR8tVzLm2Ka7Jd0Xy5Pb1Ce6Gh4Fs9Ut8Nv2Iw7Qr3Zx"),
            Some(Shape::OpenAiApiKey)
        );
        assert_eq!(
            matched("ghp_Rk8Wt2Nq6Xz4Lb9Mv3Cd7Yh1Ps5Fj0Ag"),
            Some(Shape::GitHubToken)
        );
        assert_eq!(matched("AKIAIOSFODNN7EXAMPLE"), Some(Shape::AwsAccessKeyId));
        assert_eq!(
            matched("xoxb-2416923698-4183756201-Wq8Nt3Zr6Kd1Lp4"),
            Some(Shape::SlackToken)
        );
        assert_eq!(
            matched("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.Wq3nR8tVzLm2Ka7Jd0Xy5"),
            Some(Shape::Jwt)
        );
        assert_eq!(
            matched("-----BEGIN OPENSSH PRIVATE KEY-----"),
            Some(Shape::PrivateKeyBlock)
        );
        assert_eq!(
            matched("postgres://clippo:hunter2@db.internal:5432/app"),
            Some(Shape::DatabaseUrlPassword)
        );
    }

    #[test]
    fn a_token_inside_a_longer_copy_is_still_found() {
        assert_eq!(
            matched("export OPENAI_API_KEY=sk-Wq3nR8tVzLm2Ka7Jd0Xy5Pb1Ce6Gh4Fs9\n"),
            Some(Shape::OpenAiApiKey)
        );
        assert_eq!(
            matched("-----BEGIN RSA PRIVATE KEY-----\nMIIEow…\n-----END RSA PRIVATE KEY-----\n"),
            Some(Shape::PrivateKeyBlock)
        );
    }

    /// The prefixes are the whole defence, so this is where it is checked.
    #[test]
    fn an_ordinary_string_that_merely_contains_a_prefix_does_not_match() {
        // `sk-` inside a word, not at a boundary.
        assert_eq!(matched("task-management-application-2024"), None);
        // …and at a boundary, but followed by kebab-case rather than a key.
        assert_eq!(matched("sk-invoice-line-item-renderer"), None);
        // The right prefix, nothing like enough after it.
        assert_eq!(matched("sk-1"), None);
        assert_eq!(matched("ghp_short"), None);
        // AWS ids are exactly 20 characters; 19 and 21 are something else.
        assert_eq!(matched("AKIAIOSFODNN7EXAMPL"), None);
        assert_eq!(matched("AKIAIOSFODNN7EXAMPLES"), None);
        // A database URL with no password in it.
        assert_eq!(matched("postgres://localhost:5432/clippo"), None);
        assert_eq!(matched("https://example.com/eyJ"), None);
    }

    #[test]
    fn the_database_rule_reads_the_scheme_case_insensitively() {
        assert_eq!(
            matched("POSTGRESQL://clippo:hunter2@db/app"),
            Some(Shape::DatabaseUrlPassword)
        );
        assert_eq!(
            matched("mongodb+srv://clippo:hunter2@cluster.example.net/app"),
            Some(Shape::DatabaseUrlPassword)
        );
    }

    #[test]
    fn a_jwt_with_an_empty_signature_still_matches() {
        assert_eq!(
            matched("eyJhbGciOiJub25lIn0.eyJzdWIiOiIxIn0."),
            Some(Shape::Jwt)
        );
    }
}
