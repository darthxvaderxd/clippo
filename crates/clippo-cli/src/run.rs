//! What each subcommand does: resolve the id, make the call, print the answer.
//!
//! One function per member, in DESIGN.md's order. Everything printed goes
//! through [`emit`] (data, stdout) or [`note`] (confirmations, stderr), so the
//! split the top-level `--help` promises is visible in one place rather than
//! spread across nine `println!`s.

use std::io::{self, IsTerminal, Write};

use clippo_core::Timestamp;
use clippo_ipc::EntrySummary;

use crate::cli::{Command, Recording};
use crate::client::Client;
use crate::error::CliError;
use crate::render;

/// Run one subcommand against a running daemon.
pub fn run(command: Command) -> Result<(), CliError> {
    let client = Client::connect()?;

    match command {
        Command::List {
            limit,
            offset,
            json,
        } => {
            let entries = client.list(limit, offset)?;
            if entries.is_empty() && !json {
                note(if offset == 0 {
                    "the history is empty".to_owned()
                } else {
                    format!("no entries past the first {offset}")
                });
            }
            show(&entries, json)
        }

        Command::Search { query, limit, json } => {
            let entries = client.search(&query, limit)?;
            if entries.is_empty() && !json {
                note(format!("nothing matched `{query}`"));
            }
            show(&entries, json)
        }

        Command::Copy { id } => {
            let id = client.resolve(&id)?;
            client.copy(id)?;
            note(format!(
                "entry {id} is on the clipboard — clippod has to keep running to serve it"
            ));
            Ok(())
        }

        Command::Paste { id } => {
            let id = client.resolve(&id)?;
            // The daemon says whether it pressed anything, so this does not
            // claim a paste that did not happen: `auto_paste` may be off, the
            // compositor may offer no way to synthesise keys, or the attempt
            // may have failed. All three leave the entry on the clipboard,
            // which is the part worth telling the user either way.
            let pressed = client.paste(id)?;
            note(if pressed {
                format!(
                    "entry {id} is on the clipboard, and clippo pressed your paste shortcut \
                     wherever the focus was"
                )
            } else {
                format!(
                    "entry {id} is on the clipboard — clippo did not press anything; \
                     paste it yourself, and see clippod's log for why"
                )
            });
            Ok(())
        }

        Command::Pin { id, off } => {
            let id = client.resolve(&id)?;
            client.pin(id, !off)?;
            note(if off {
                format!("entry {id} is unpinned, and can now be cleared or aged out")
            } else {
                format!("entry {id} is pinned, and is now exempt from retention and `clear`")
            });
            Ok(())
        }

        Command::Rm { ids } => {
            // Resolve every reference before deleting any of them, so a typo
            // in the last argument does not leave the first ones deleted. Two
            // references naming the same entry are one id by here, so nothing
            // is ever deleted twice.
            let ids = client.resolve_all(&ids)?;
            let mut deleted: Vec<i64> = Vec::with_capacity(ids.len());
            for id in &ids {
                // The interface has no batch `Delete`, so a failure part-way
                // through a list genuinely leaves the earlier ones gone. Say
                // which before reporting it: the alternative is a user who has
                // to run `clippo list` to find out what their command did.
                if let Err(error) = client.delete(*id) {
                    if !deleted.is_empty() {
                        note(deleted_message(&deleted));
                    }
                    return Err(error);
                }
                deleted.push(*id);
            }
            note(deleted_message(&deleted));
            Ok(())
        }

        Command::Clear {
            yes,
            include_pinned,
        } => clear(&client, yes, include_pinned),

        Command::Pause { state: None } => {
            let paused = client.paused()?;
            emit(if paused { "paused\n" } else { "recording\n" })
        }

        Command::Pause { state: Some(state) } => {
            client.set_paused(state.paused())?;
            note(match state {
                Recording::On => "recording is paused; copies are not being recorded until \
                                  `clippo pause off`"
                    .to_owned(),
                Recording::Off => "recording again".to_owned(),
            });
            Ok(())
        }

        Command::Reveal { id } => {
            let id = client.resolve(&id)?;
            // Exactly what was stored: no trailing newline, no sanitising. See
            // this subcommand's --help.
            emit(&client.reveal(id)?)
        }

        Command::Show => {
            client.toggle_applet()?;
            // No confirmation: this is what a keypress runs, and a line on
            // stderr per press would be noise in the journal rather than
            // something anybody reads. The popup appearing is the feedback.
            Ok(())
        }
    }
}

/// What `rm` says it did, for however many entries it got through.
fn deleted_message(ids: &[i64]) -> String {
    match ids {
        [id] => format!("deleted entry {id}"),
        ids => format!(
            "deleted entries {}",
            ids.iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The history, as a table or as JSON.
fn show(entries: &[EntrySummary], json: bool) -> Result<(), CliError> {
    if json {
        emit(&render::json(entries)?)
    } else {
        emit(&render::table(entries, Timestamp::now()))
    }
}

/// `Clear`, with the confirmation it needs first.
///
/// Deleting a whole history is not undoable and `clippo clear` is three
/// keystrokes from `clippo copy`, so the default is to ask. Under a pipe or in
/// a script there is nobody to ask, and the answer to that is an error rather
/// than going ahead — a `clear` in a cron job that the author expected to
/// prompt would otherwise silently work.
fn clear(client: &Client, yes: bool, include_pinned: bool) -> Result<(), CliError> {
    if !yes {
        if !io::stdin().is_terminal() {
            return Err(CliError::ClearNeedsConfirmation);
        }

        let entries = client.list(0, 0)?;
        let pinned = entries.iter().filter(|entry| entry.pinned).count();
        let doomed = if include_pinned {
            entries.len()
        } else {
            entries.len() - pinned
        };
        if doomed == 0 {
            note("there is nothing to clear".to_owned());
            return Ok(());
        }

        let mut stderr = io::stderr();
        let _ = write!(stderr, "{}", prompt(doomed, pinned, include_pinned));
        let _ = stderr.flush();

        let mut answer = String::new();
        io::stdin()
            .read_line(&mut answer)
            .map_err(CliError::Answer)?;
        if !affirmative(&answer) {
            return Err(CliError::Aborted);
        }
    }

    client.clear(include_pinned)?;
    note(if include_pinned {
        "the history is cleared, pinned entries included".to_owned()
    } else {
        "the history is cleared; pinned entries were kept".to_owned()
    });
    Ok(())
}

/// The question, saying exactly what is about to happen to the pinned entries.
///
/// They are the ones a user chose to keep, so "7 entries" without saying which
/// side of the line the pinned ones fall on is the sentence that gets somebody
/// to type `y` and lose them.
fn prompt(doomed: usize, pinned: usize, include_pinned: bool) -> String {
    let what = format!("clippo: delete {}", count(doomed));
    match (include_pinned, pinned) {
        (_, 0) => format!("{what}? [y/N] "),
        (true, pinned) => format!("{what}, including {pinned} pinned? [y/N] "),
        (false, pinned) => format!("{what}, keeping {pinned} pinned? [y/N] "),
    }
}

/// `1 entry` / `2 entries`.
fn count(entries: usize) -> String {
    if entries == 1 {
        "1 entry".to_owned()
    } else {
        format!("{entries} entries")
    }
}

/// Whether an answer to `[y/N]` was a yes.
///
/// Anything else is a no, including an empty line and a closed stdin: the
/// default in `[y/N]` is the capital, and a deletion that cannot be undone is
/// not the thing to be generous about parsing.
fn affirmative(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Write finished output to stdout.
///
/// A closed pipe is a normal end, not a failure: `clippo list | head -3` closes
/// stdout after three lines, and Rust ignores `SIGPIPE`, so without this the
/// command would report an error for having been read successfully.
fn emit(text: &str) -> Result<(), CliError> {
    let mut stdout = io::stdout().lock();
    match stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
    {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(CliError::Stdout(error)),
    }
}

/// Say something to the user on stderr, so it stays out of a redirect.
///
/// Errors are ignored: a note is not worth failing a command that has already
/// done its work, and stderr being closed is not a reason to report that a
/// `copy` did not happen.
fn note(message: String) {
    let _ = writeln!(io::stderr(), "clippo: {message}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_rm_says_it_did_names_every_entry_it_deleted() {
        assert_eq!(deleted_message(&[7]), "deleted entry 7");
        assert_eq!(deleted_message(&[7, 12, 3]), "deleted entries 7, 12, 3");
    }

    #[test]
    fn the_prompt_says_what_happens_to_the_pinned_entries() {
        assert_eq!(
            prompt(7, 2, false),
            "clippo: delete 7 entries, keeping 2 pinned? [y/N] "
        );
        assert_eq!(
            prompt(9, 2, true),
            "clippo: delete 9 entries, including 2 pinned? [y/N] "
        );
    }

    #[test]
    fn the_prompt_does_not_mention_pins_when_there_are_none() {
        assert_eq!(prompt(3, 0, false), "clippo: delete 3 entries? [y/N] ");
        assert_eq!(prompt(3, 0, true), "clippo: delete 3 entries? [y/N] ");
    }

    #[test]
    fn one_entry_is_not_one_entries() {
        assert_eq!(count(1), "1 entry");
        assert_eq!(count(0), "0 entries");
        assert_eq!(count(2), "2 entries");
    }

    /// `[y/N]` means the default is no, so everything that is not a yes has to
    /// be one — a stray keystroke, an empty line, an EOF.
    #[test]
    fn only_a_yes_is_a_yes() {
        for yes in ["y", "Y", "yes", "YES", " y \n"] {
            assert!(affirmative(yes), "{yes:?}");
        }
        for no in ["", "\n", "n", "no", "yeah", "yes please", "1"] {
            assert!(!affirmative(no), "{no:?}");
        }
    }
}
