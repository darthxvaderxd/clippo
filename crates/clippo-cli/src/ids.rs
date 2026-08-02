//! Turning what a user typed into the id of one entry.
//!
//! The store's ids are SQLite row ids — small integers, which is already
//! typeable, and ROADMAP.md's round-trip check is literally `clippo copy 2`.
//! But a history that has been running for a week is into four digits, and
//! retyping `1487` from a list is worse than retyping `148`. So an id may be
//! given as a **prefix** of itself.
//!
//! Three rules, in this order:
//!
//! 1. An id that exists is itself. `12` is entry 12 even when entries 120 and
//!    127 also exist — otherwise adding an entry could change what an id a user
//!    just read off a list means.
//! 2. Otherwise, a prefix matching exactly one id is that id.
//! 3. A prefix matching several is an error naming all of them. Never a guess:
//!    the commands this resolves for are `copy`, `rm` and `reveal`, and two of
//!    those are irreversible or leak a secret if aimed at the wrong entry.
//!
//! Matching is on the decimal spelling of the id, so it is a string prefix
//! (`14` matches `142`), not a numeric one.

use std::fmt;

use clippo_ipc::EntrySummary;

use crate::render;

/// Why the id a user typed does not name exactly one entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// Not a decimal number, so it cannot be an id or the start of one.
    NotAnId { typed: String },
    /// A well-formed id that no entry has, or starts with.
    NoSuchEntry { typed: String, history_empty: bool },
    /// A prefix of more than one id.
    Ambiguous {
        typed: String,
        candidates: Vec<Candidate>,
    },
}

/// One of the entries an ambiguous prefix could have meant, as the error
/// prints it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: i64,
    /// Sanitised and shortened: this goes to a terminal like any other preview.
    pub preview: String,
}

/// How much of a preview an ambiguity error shows. Shorter than a table row —
/// it is there to tell two entries apart, not to be read.
const CANDIDATE_PREVIEW_CHARS: usize = 40;

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::NotAnId { typed } => write!(
                f,
                "`{typed}` is not an entry id. Ids are the numbers in the ID column of \
                 `clippo list`, and may be shortened to any unambiguous start"
            ),
            ResolveError::NoSuchEntry {
                typed,
                history_empty: true,
            } => write!(
                f,
                "there is no entry `{typed}`: the history is empty. Copy something with \
                 clippod running, then try again"
            ),
            ResolveError::NoSuchEntry {
                typed,
                history_empty: false,
            } => write!(
                f,
                "there is no entry `{typed}`, and no entry whose id starts with it. \
                 Run `clippo list` to see what is there"
            ),
            ResolveError::Ambiguous { typed, candidates } => {
                write!(
                    f,
                    "`{typed}` could mean any of these {} entries — type more of the id:",
                    candidates.len()
                )?;
                let width = candidates
                    .iter()
                    .map(|candidate| candidate.id.to_string().len())
                    .max()
                    .unwrap_or(1);
                for candidate in candidates {
                    // The newline leads rather than trails: the caller ends the
                    // message, and an error that printed its own blank line
                    // would leave one in the middle of a terminal session.
                    write!(f, "\n  {:>width$}  {}", candidate.id, candidate.preview)?;
                }
                Ok(())
            }
        }
    }
}

/// Hand-written rather than derived: the ambiguous case prints a list of
/// candidates over several lines, which is a `Display` body rather than an
/// `#[error("…")]` attribute.
impl std::error::Error for ResolveError {}

/// The one entry `typed` names, out of `entries`.
///
/// `entries` is the whole history — the caller fetches it with `List(0, 0)` —
/// because both "no such entry" and "that could be several" need to see every
/// id, not a page of them.
pub fn resolve(typed: &str, entries: &[EntrySummary]) -> Result<i64, ResolveError> {
    if typed.is_empty() || !typed.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ResolveError::NotAnId {
            typed: typed.to_owned(),
        });
    }

    // Rule 1: an exact id wins outright, however many longer ids start with it.
    if let Some(entry) = entries.iter().find(|entry| entry.id.to_string() == *typed) {
        return Ok(entry.id);
    }

    let matches: Vec<&EntrySummary> = entries
        .iter()
        .filter(|entry| entry.id.to_string().starts_with(typed))
        .collect();

    match matches.as_slice() {
        [] => Err(ResolveError::NoSuchEntry {
            typed: typed.to_owned(),
            history_empty: entries.is_empty(),
        }),
        [only] => Ok(only.id),
        several => Err(ResolveError::Ambiguous {
            typed: typed.to_owned(),
            candidates: several
                .iter()
                .map(|entry| Candidate {
                    id: entry.id,
                    preview: render::one_line(&entry.preview, CANDIDATE_PREVIEW_CHARS),
                })
                .collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: i64, preview: &str) -> EntrySummary {
        EntrySummary {
            id,
            created_at: 0,
            last_used_at: 0,
            kind: "text".to_owned(),
            preview: preview.to_owned(),
            pinned: false,
            sensitive: false,
        }
    }

    fn history() -> Vec<EntrySummary> {
        vec![
            entry(3, "three"),
            entry(12, "twelve"),
            entry(120, "one hundred and twenty"),
            entry(127, "one hundred and twenty-seven"),
        ]
    }

    #[test]
    fn a_whole_id_resolves_to_itself() {
        assert_eq!(resolve("3", &history()).unwrap(), 3);
        assert_eq!(resolve("127", &history()).unwrap(), 127);
    }

    #[test]
    fn a_prefix_matching_one_entry_resolves() {
        assert_eq!(resolve("1270", &[entry(1270, "x")]).unwrap(), 1270);
        assert_eq!(resolve("120", &history()).unwrap(), 120);
    }

    /// The point of rule 1: `12` is entry 12, not an ambiguity between 12, 120
    /// and 127. Copying an entry must not start meaning something else because
    /// a longer id appeared.
    #[test]
    fn an_id_that_exists_beats_the_longer_ids_it_prefixes() {
        assert_eq!(resolve("12", &history()).unwrap(), 12);
    }

    #[test]
    fn a_prefix_matching_several_entries_lists_them_rather_than_guessing() {
        let error = resolve("1", &history()).unwrap_err();
        let ResolveError::Ambiguous { typed, candidates } = &error else {
            panic!("{error:?}");
        };
        assert_eq!(typed, "1");
        assert_eq!(
            candidates.iter().map(|c| c.id).collect::<Vec<_>>(),
            [12, 120, 127]
        );

        let printed = error.to_string();
        assert!(printed.contains("type more of the id"), "{printed}");
        for id in ["12", "120", "127"] {
            assert!(printed.contains(id), "{printed}");
        }
        assert!(printed.contains("twelve"), "{printed}");
    }

    /// A candidate preview is clipboard content going to a terminal, exactly
    /// like a table row, so it is sanitised the same way.
    #[test]
    fn candidate_previews_are_made_safe_for_the_terminal() {
        let entries = vec![entry(12, "a\u{1b}[31mb"), entry(13, "c\nd")];
        let error = resolve("1", &entries).unwrap_err();
        let printed = error.to_string();
        assert!(!printed.contains('\u{1b}'), "{printed:?}");
        assert!(printed.contains("\\u{1b}"), "{printed:?}");
        assert!(printed.contains("c d"), "{printed:?}");
    }

    #[test]
    fn a_prefix_matching_nothing_says_so() {
        let error = resolve("9", &history()).unwrap_err();
        assert_eq!(
            error,
            ResolveError::NoSuchEntry {
                typed: "9".to_owned(),
                history_empty: false
            }
        );
        assert!(error.to_string().contains("clippo list"));
    }

    /// "no entry 2" when nothing has ever been copied sends a user looking for
    /// a deleted entry. Say which it is.
    #[test]
    fn an_empty_history_says_it_is_empty_rather_than_that_the_id_is_wrong() {
        let error = resolve("2", &[]).unwrap_err();
        assert_eq!(
            error,
            ResolveError::NoSuchEntry {
                typed: "2".to_owned(),
                history_empty: true
            }
        );
        assert!(error.to_string().contains("the history is empty"));
    }

    #[test]
    fn something_that_is_not_a_number_is_not_an_id() {
        for typed in ["", "abc", "2a", "-1", "1.0", " 1", "١٢"] {
            assert!(
                matches!(
                    resolve(typed, &history()),
                    Err(ResolveError::NotAnId { .. })
                ),
                "{typed:?} was accepted"
            );
        }
    }

    /// Prefixes are on the decimal spelling, so a leading zero is not the same
    /// id — and saying "no such entry" is better than quietly copying entry 7.
    #[test]
    fn a_leading_zero_is_not_the_same_id() {
        assert!(matches!(
            resolve("03", &history()),
            Err(ResolveError::NoSuchEntry { .. })
        ));
    }
}
