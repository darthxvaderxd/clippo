//! The fixture corpus, asserted.
//!
//! `corpus.toml` next to this file is the corpus DESIGN.md requires. This test
//! is what makes it a corpus rather than a document: every fixture is
//! classified, by rule name, in both directions.
//!
//! # Which way this test fails
//!
//! DESIGN.md: *"False positives here are annoying; false negatives are the
//! actual risk."* So a missed secret is reported first, on its own heading, and
//! the failure message says what it means. Every mismatch in a run is collected
//! before anything panics — tuning a threshold and being told about one fixture
//! at a time is how a tuning session takes an afternoon.

use clippo_core::secrets::{self, Shape, Signal};
use clippo_core::SecretsConfig;
use serde::Deserialize;

/// The corpus, compiled in. `include_str!` rather than a runtime read: the test
/// binary then has no working-directory dependency, and a missing corpus is a
/// compile error rather than a test that passes by finding nothing.
const CORPUS: &str = include_str!("corpus.toml");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Corpus {
    /// Values that must be detected, each naming the rule that must fire.
    secret: Vec<Fixture>,
    /// Values that must not be detected at all.
    safe: Vec<Fixture>,
    /// Values that are detected, are not secrets, and are known to be both.
    accepted_false_positive: Vec<Fixture>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    name: String,
    value: String,
    /// The rule expected to fire, in `Signal::rule` spelling. Absent for the
    /// `safe` fixtures, which expect no rule at all.
    #[serde(default)]
    rule: Option<String>,
    /// Whether the selection carried the password-manager MIME marker.
    #[serde(default)]
    hint: bool,
    /// Why this fixture classifies the way it does. Not asserted — it is the
    /// part a person reads when the assertion fails.
    #[allow(dead_code)]
    why: String,
}

fn corpus() -> Corpus {
    toml::from_str(CORPUS).expect("the fixture corpus should parse")
}

fn detect(fixture: &Fixture, config: &SecretsConfig) -> Option<Signal> {
    secrets::detect(&fixture.value, fixture.hint, config)
}

/// Panic with every failure at once, under a heading that says what it means.
fn report(heading: &str, failures: Vec<String>) {
    if failures.is_empty() {
        return;
    }
    panic!(
        "\n{heading}\n{}\n\nThe fixtures are in crates/clippo-core/tests/corpus.toml, \
         with a `why` on each saying how it is supposed to classify.\n",
        failures
            .iter()
            .map(|failure| format!("  - {failure}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The direction that matters. A secret that is not detected is a password
/// rendered in full in a panel, which is the failure clippo exists to prevent.
#[test]
fn every_secret_in_the_corpus_is_detected() {
    let corpus = corpus();
    let config = SecretsConfig::default();
    let mut missed = Vec::new();

    for fixture in &corpus.secret {
        let expected = fixture
            .rule
            .as_deref()
            .unwrap_or_else(|| panic!("secret fixture {} has no `rule`", fixture.name));
        match detect(fixture, &config) {
            None => missed.push(format!(
                "{}: NOT DETECTED — expected `{expected}` to fire",
                fixture.name
            )),
            Some(signal) if signal.rule() != expected => missed.push(format!(
                "{}: detected by `{}`, but the corpus expects `{expected}`",
                fixture.name,
                signal.rule()
            )),
            Some(_) => {}
        }
    }

    report(
        "MISSED SECRETS. Each of these would be stored unmasked and shown in full:",
        missed,
    );
}

/// The other direction. Annoying rather than dangerous, but a clipboard manager
/// that masks git SHAs is one nobody keeps running.
#[test]
fn nothing_safe_in_the_corpus_is_flagged() {
    let corpus = corpus();
    let config = SecretsConfig::default();
    let mut flagged = Vec::new();

    for fixture in &corpus.safe {
        assert!(
            fixture.rule.is_none(),
            "safe fixture {} names a rule; safe fixtures expect none",
            fixture.name
        );
        if let Some(signal) = detect(fixture, &config) {
            flagged.push(format!(
                "{}: flagged by `{}`, but the corpus expects nothing to fire",
                fixture.name,
                signal.rule()
            ));
        }
    }

    report(
        "FALSE POSITIVES. These are ordinary strings clippo would mask:",
        flagged,
    );
}

/// The two the corpus admits to. Pinned so that fixing one is a deliberate
/// change to this file rather than something noticed a year later.
#[test]
fn the_accepted_false_positives_are_still_the_ones_we_accepted() {
    let corpus = corpus();
    let config = SecretsConfig::default();

    for fixture in &corpus.accepted_false_positive {
        let expected = fixture.rule.as_deref().expect("an expected rule");
        let signal = detect(fixture, &config).unwrap_or_else(|| {
            panic!(
                "{} is no longer flagged. That may well be an improvement — move it to \
                 [[safe]] and say what changed.",
                fixture.name
            )
        });
        assert_eq!(signal.rule(), expected, "{}", fixture.name);
    }
}

/// DESIGN.md's escape hatch, over the whole corpus: turning the entropy rule
/// off must lose exactly the entropy fixtures and nothing else.
#[test]
fn turning_the_entropy_rule_off_costs_only_the_entropy_fixtures() {
    let corpus = corpus();
    let without = SecretsConfig {
        entropy_rule: false,
        ..SecretsConfig::default()
    };
    let mut wrong = Vec::new();

    for fixture in &corpus.secret {
        let expected = fixture.rule.as_deref().unwrap_or_default();
        let detected = detect(fixture, &without);
        if expected == "entropy" {
            if let Some(signal) = detected {
                wrong.push(format!(
                    "{}: still flagged by `{}` with the entropy rule off",
                    fixture.name,
                    signal.rule()
                ));
            }
        } else {
            match detected {
                Some(signal) if signal.rule() == expected => {}
                other => wrong.push(format!(
                    "{}: the entropy knob changed a `{expected}` fixture to {other:?}",
                    fixture.name
                )),
            }
        }
    }

    // …and the safe fixtures stay safe, which is the easy half.
    for fixture in &corpus.safe {
        if let Some(signal) = detect(fixture, &without) {
            wrong.push(format!(
                "{}: flagged by `{}` even with the entropy rule off",
                fixture.name,
                signal.rule()
            ));
        }
    }

    report(
        "The entropy knob is supposed to switch off exactly one rule:",
        wrong,
    );
}

/// Every shape has a fixture. Adding a pattern without one would ship a rule
/// nothing exercises, which is how a regex with a typo in it survives.
#[test]
fn every_shape_rule_has_at_least_one_fixture() {
    let corpus = corpus();
    let covered: Vec<String> = corpus
        .secret
        .iter()
        .filter_map(|fixture| fixture.rule.clone())
        .collect();

    let uncovered: Vec<String> = Shape::ALL
        .iter()
        .map(|shape| format!("shape:{}", shape.name()))
        .filter(|rule| !covered.contains(rule))
        .collect();

    report("Shape rules with no fixture in the corpus:", uncovered);

    // And the three rules DESIGN.md names, each represented.
    for rule in ["mime-hint", "entropy"] {
        assert!(
            covered.iter().any(|covered| covered == rule),
            "no corpus fixture exercises the `{rule}` rule"
        );
    }
}

/// The masking contract, over every secret in the corpus at once: whatever the
/// value, the mask must not contain it, must be a fixed width, and must not
/// give away how long it was.
#[test]
fn masking_a_corpus_secret_never_shows_the_secret() {
    let corpus = corpus();
    let config = SecretsConfig::default();
    let expected_width = config.mask_prefix + secrets::MASK_BULLETS + config.mask_suffix;

    let mut leaked = Vec::new();
    for fixture in corpus.secret.iter().chain(&corpus.accepted_false_positive) {
        let masked = secrets::mask(&fixture.value, &config);

        if masked.chars().count() > expected_width {
            leaked.push(format!(
                "{}: masked to {} characters, which is more than the fixed {expected_width}",
                fixture.name,
                masked.chars().count()
            ));
        }
        if fixture.value.chars().count() > expected_width && masked.contains(&fixture.value) {
            leaked.push(format!(
                "{}: the mask contains the whole value",
                fixture.name
            ));
        }
        if masked
            .chars()
            .filter(|&c| c == secrets::MASK_BULLET)
            .count()
            != secrets::MASK_BULLETS
        {
            leaked.push(format!(
                "{}: masked to {masked:?}, which is not {} bullets",
                fixture.name,
                secrets::MASK_BULLETS
            ));
        }
    }

    report("Masking leaked something it should have hidden:", leaked);
}

/// A corpus nobody added to is a corpus that stopped being tuned against. This
/// is a floor, not a target — it exists so that deleting fixtures to make a
/// change pass is a visible thing to have done.
#[test]
fn the_corpus_covers_both_directions_in_bulk() {
    let corpus = corpus();
    assert!(
        corpus.secret.len() >= 15,
        "only {} true positives in the corpus",
        corpus.secret.len()
    );
    assert!(
        corpus.safe.len() >= 15,
        "only {} true negatives in the corpus",
        corpus.safe.len()
    );

    // Every fixture explains itself, and no two share a name.
    let mut names: Vec<&str> = corpus
        .secret
        .iter()
        .chain(&corpus.safe)
        .chain(&corpus.accepted_false_positive)
        .map(|fixture| {
            assert!(
                !fixture.why.trim().is_empty(),
                "{} has no `why`",
                fixture.name
            );
            fixture.name.as_str()
        })
        .collect();
    let total = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), total, "two fixtures share a name");
}
