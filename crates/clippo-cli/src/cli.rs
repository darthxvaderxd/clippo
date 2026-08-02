//! The argument surface: one subcommand per member of the D-Bus interface.
//!
//! DESIGN.md's `clippo list|search|copy|pin|rm|clear|pause|reveal`, mapped 1:1
//! onto `clippo-ipc`'s members. Nothing here talks to the bus — [`Cli::parse`]
//! produces a value, [`crate::run`] acts on it — so the whole surface is
//! testable without a daemon.
//!
//! `show` (M5) is the one subcommand that is not a member of that interface: it
//! calls `Toggle` on the *applet*, and is the command a user binds `Super+V`
//! to. It is listed here rather than in a second binary because a key binding
//! naming `clippo` is one thing to install and one thing to document.
//!
//! # stdout is data, stderr is talk
//!
//! Every subcommand keeps stdout for the thing a script would read: the table,
//! the JSON, the revealed value, the paused state. Confirmations ("entry 2 is
//! on the clipboard") and errors go to stderr. `clippo reveal 2 > secret.txt`
//! therefore writes the value and nothing else, and a failure is still visible
//! in the terminal.

use clap::{Parser, Subcommand, ValueEnum};

/// How much of a preview a table row shows before it is cut.
///
/// The daemon already caps a preview at 120 characters; this is narrower again
/// so that the row fits a terminal next to the other four columns. The whole
/// preview is still available with `--json`, and the whole *value* with
/// `reveal`.
pub const PREVIEW_COLUMN_CHARS: usize = 64;

/// How many entries `list` and `search` show when not told otherwise.
///
/// A screenful. `--limit 0` asks for everything, which is what the D-Bus
/// members mean by a limit of zero.
pub const DEFAULT_LIMIT: u32 = 20;

/// `clippo` — the command-line client for the clipboard daemon.
#[derive(Debug, Parser)]
#[command(
    name = "clippo",
    version,
    about = "Browse and reuse the clipboard history that clippod records",
    long_about = "Browse and reuse the clipboard history that clippod records.

Every subcommand is a call to the clippod daemon over the session bus; clippo \
never opens the history database itself. If clippod is not running, each one \
says so and exits non-zero.

Naming an entry: `clippo list` prints an ID column, and every command that \
takes an ID accepts either the whole number or an unambiguous start of one — \
`clippo copy 2`, or `clippo copy 14` for entry 142. A prefix that could mean \
more than one entry is an error listing the candidates; an ID that exists is \
always itself, never a prefix of a longer one.

Output: stdout carries data (the table, `--json`, a revealed value), stderr \
carries confirmations and errors."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// One variant per member of `com.nilfactor.Clippo`.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Show the history, most recently used first.
    #[command(long_about = "Show the history, most recently used first.

Columns are the ID to type at other subcommands, how long ago the entry was \
last used, its kind (text, html, uris, image), two flag characters — `p` when \
the entry is pinned, `s` when clippo suspects a password or token — and a \
one-line preview.

An `s` entry's preview is a mask: `ab••••••••yz`, the first and last couple of \
characters with a fixed-width run between them that does not say how long the \
value is. The value itself is unchanged — `clippo copy` pastes it in full, and \
`clippo reveal` prints it. Masking is what the list shows, not what clippo \
stores.

Previews are clipboard content, which is arbitrary bytes: newlines are \
collapsed and control characters are escaped before printing, so a copied \
terminal escape sequence cannot repaint your terminal. `--json` keeps whole, \
unflattened previews, and relies on JSON's own \\uXXXX escaping instead — \
which is equally safe to look at and decodes back to exactly what was stored. \
`reveal` is the one command that prints clipboard content as it is.")]
    List {
        /// How many entries to show; 0 means all of them.
        #[arg(short = 'n', long, default_value_t = DEFAULT_LIMIT, value_name = "N")]
        limit: u32,

        /// Skip this many entries first, for paging through a long history.
        #[arg(long, default_value_t = 0, value_name = "N")]
        offset: u32,

        /// Emit JSON instead of a table, for scripts.
        #[arg(
            long,
            long_help = "Emit JSON instead of a table, for scripts.

Every field the daemon sent, under its own name: whole previews rather than \
the column's share of one, and timestamps as Unix milliseconds rather than \
`3m`. Control, invisible and reordering characters are written as \\uXXXX — \
JSON's own spelling for them, so the output is safe to look at in a terminal \
and still decodes back to exactly what was stored."
        )]
        json: bool,
    },

    /// Fuzzy-match the previews, best match first.
    #[command(long_about = "Fuzzy-match the previews, best match first.

Matching is the daemon's, over the previews it holds in memory — the same \
fuzzy matcher COSMIC's launcher uses, so `clpo` finds `clippo`. An empty \
query matches everything.

Search only ever sees previews, so it cannot find text that was cut off the \
end of a long copy.")]
    Search {
        /// What to look for.
        #[arg(value_name = "QUERY")]
        query: String,

        /// How many matches to show; 0 means all of them.
        #[arg(short = 'n', long, default_value_t = DEFAULT_LIMIT, value_name = "N")]
        limit: u32,

        /// Emit JSON instead of a table, for scripts.
        #[arg(
            long,
            long_help = "Emit JSON instead of a table, for scripts.

Every field the daemon sent, under its own name: whole previews rather than \
the column's share of one, and timestamps as Unix milliseconds rather than \
`3m`. Control, invisible and reordering characters are written as \\uXXXX — \
JSON's own spelling for them, so the output is safe to look at in a terminal \
and still decodes back to exactly what was stored."
        )]
        json: bool,
    },

    /// Put an entry back on the clipboard.
    #[command(long_about = "Put an entry back on the clipboard.

The daemon holds the clipboard for as long as it runs: that is how Wayland \
works, and if clippod stops, the clipboard empties. Paste before you stop it.")]
    Copy {
        /// The entry's ID, in full or as an unambiguous prefix.
        #[arg(value_name = "ID")]
        id: String,
    },

    /// Pin an entry, exempting it from retention and from `clear`.
    Pin {
        /// The entry's ID, in full or as an unambiguous prefix.
        #[arg(value_name = "ID")]
        id: String,

        /// Unpin instead of pinning.
        #[arg(long)]
        off: bool,
    },

    /// Delete one or more entries, pinned or not.
    Rm {
        /// The entries' IDs, in full or as unambiguous prefixes.
        #[arg(value_name = "ID", required = true, num_args = 1..)]
        ids: Vec<String>,
    },

    /// Delete the whole history.
    #[command(long_about = "Delete the whole history.

Pinned entries are kept unless --include-pinned. Because this cannot be \
undone it needs confirming: answer the prompt, or pass --yes. With stdin not \
a terminal — in a script, or under a pipe — there is nobody to prompt, so \
--yes is required and its absence is an error rather than a silent deletion.")]
    Clear {
        /// Delete without asking. Required when stdin is not a terminal.
        #[arg(short = 'y', long)]
        yes: bool,

        /// Delete pinned entries too.
        #[arg(long)]
        include_pinned: bool,
    },

    /// Stop or resume recording new copies, or report which it is doing.
    #[command(long_about = "Stop or resume recording new copies.

  clippo pause on    stop recording
  clippo pause off   resume recording
  clippo pause       print `paused` or `recording`

Pausing only stops new copies being recorded. The history stays readable and \
`copy` still works, and the state is forgotten when clippod restarts.")]
    Pause {
        /// `on` to stop recording, `off` to resume. Omit to report the state.
        #[arg(value_name = "STATE")]
        state: Option<Recording>,
    },

    /// Print an entry's full value to stdout.
    #[command(long_about = "Print an entry's full value to stdout, exactly as \
stored — no truncation, no masking, and no trailing newline added, so it \
composes in a pipe:

  clippo reveal 2 > key.pem

This is how a masked `s` entry is read: `Reveal` is the only member of the \
daemon's interface that returns a suspected secret in full, and it does it \
only when asked for one entry by ID.

This is the one command that does not make what it prints safe for a \
terminal. A copy containing escape sequences will be acted on by the terminal \
you print it to, and a copy containing bidirectional overrides can display as \
something other than what it is. Redirect or pipe it when you do not know what \
the entry holds.

An image entry has no text to print and is refused.")]
    Reveal {
        /// The entry's ID, in full or as an unambiguous prefix.
        #[arg(value_name = "ID")]
        id: String,
    },

    /// Open the panel applet's picker (or close it, if it is already open).
    #[command(
        long_about = "Ask the clippo panel applet to open its picker, or to close it if \
it is already open.

This is the command to bind a key to. clippo does not register a global \
shortcut for itself — COSMIC owns those — so binding `Super+V` is a one-time \
manual edit of

  ~/.config/cosmic/com.system76.CosmicSettings.Shortcuts

The exact snippet is in the README's \"Global shortcut\" section.

Unlike every other subcommand this one talks to the applet rather than to \
clippod, so it needs clippo to be on the panel. A running daemon is not \
enough, and the error says so.

It returns as soon as the applet has the request, which is before any surface \
is on screen — the compositor decides that part, and there is nothing useful \
to wait for."
    )]
    Show,
}

/// The argument to `clippo pause`, naming the state of the *pause* rather than
/// of the recording: `on` pauses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Recording {
    /// Stop recording new copies.
    On,
    /// Resume recording new copies.
    Off,
}

impl Recording {
    /// What to pass to `SetPaused`.
    pub const fn paused(self) -> bool {
        matches!(self, Recording::On)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn parse(args: &[&str]) -> Command {
        Cli::try_parse_from(args)
            .expect("these arguments parse")
            .command
    }

    /// clap's own consistency check: duplicate short flags, a `default_value`
    /// that does not parse, an argument declared twice. It panics on a mistake
    /// that would otherwise only show up when a user typed that subcommand.
    #[test]
    fn the_argument_surface_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    /// DESIGN.md names eight subcommands. This is the list, and a missing one
    /// fails here rather than at the moment somebody types it.
    #[test]
    fn every_subcommand_design_names_exists() {
        for name in [
            "list", "search", "copy", "pin", "rm", "clear", "pause", "reveal", "show",
        ] {
            assert!(
                Cli::command()
                    .get_subcommands()
                    .any(|subcommand| subcommand.get_name() == name),
                "`clippo {name}` is missing"
            );
        }
    }

    #[test]
    fn list_defaults_to_a_screenful_from_the_top() {
        assert!(matches!(
            parse(&["clippo", "list"]),
            Command::List {
                limit: DEFAULT_LIMIT,
                offset: 0,
                json: false
            }
        ));
    }

    #[test]
    fn list_takes_a_limit_an_offset_and_json() {
        assert!(matches!(
            parse(&["clippo", "list", "-n", "5", "--offset", "10", "--json"]),
            Command::List {
                limit: 5,
                offset: 10,
                json: true
            }
        ));
    }

    /// Zero is the wire's "no limit", not a rejected value.
    #[test]
    fn a_limit_of_zero_parses() {
        assert!(matches!(
            parse(&["clippo", "list", "--limit", "0"]),
            Command::List { limit: 0, .. }
        ));
    }

    #[test]
    fn search_takes_the_query_first() {
        let Command::Search { query, limit, json } =
            parse(&["clippo", "search", "todo list", "-n", "3"])
        else {
            panic!("that is a search");
        };
        assert_eq!(query, "todo list");
        assert_eq!(limit, 3);
        assert!(!json);
    }

    #[test]
    fn search_without_a_query_is_an_error_rather_than_an_empty_one() {
        assert!(Cli::try_parse_from(["clippo", "search"]).is_err());
    }

    #[test]
    fn an_id_is_taken_as_written_so_it_can_be_a_prefix() {
        let Command::Copy { id } = parse(&["clippo", "copy", "07"]) else {
            panic!("that is a copy");
        };
        assert_eq!(id, "07", "the leading zero has to survive to the resolver");
    }

    #[test]
    fn pin_unpins_with_off() {
        let Command::Pin { id, off } = parse(&["clippo", "pin", "2", "--off"]) else {
            panic!("that is a pin");
        };
        assert_eq!(id, "2");
        assert!(off);
    }

    #[test]
    fn rm_takes_more_than_one_id_and_refuses_none() {
        let Command::Rm { ids } = parse(&["clippo", "rm", "1", "2", "3"]) else {
            panic!("that is an rm");
        };
        assert_eq!(ids, ["1", "2", "3"]);
        assert!(Cli::try_parse_from(["clippo", "rm"]).is_err());
    }

    #[test]
    fn clear_defaults_to_asking_and_to_sparing_pinned_entries() {
        assert!(matches!(
            parse(&["clippo", "clear"]),
            Command::Clear {
                yes: false,
                include_pinned: false
            }
        ));
        assert!(matches!(
            parse(&["clippo", "clear", "-y", "--include-pinned"]),
            Command::Clear {
                yes: true,
                include_pinned: true
            }
        ));
    }

    #[test]
    fn pause_with_no_state_is_a_question_and_with_one_is_an_order() {
        assert!(matches!(
            parse(&["clippo", "pause"]),
            Command::Pause { state: None }
        ));
        assert!(matches!(
            parse(&["clippo", "pause", "on"]),
            Command::Pause {
                state: Some(Recording::On)
            }
        ));
        assert!(matches!(
            parse(&["clippo", "pause", "off"]),
            Command::Pause {
                state: Some(Recording::Off)
            }
        ));
    }

    /// `on` pauses. The word names the state of the pause, and getting it
    /// backwards would silently stop recording when a user meant to resume.
    #[test]
    fn pause_on_is_the_one_that_sets_paused() {
        assert!(Recording::On.paused());
        assert!(!Recording::Off.paused());
    }

    #[test]
    fn an_unknown_pause_state_is_refused_rather_than_guessed() {
        assert!(Cli::try_parse_from(["clippo", "pause", "yes"]).is_err());
    }

    /// The warning is the only thing standing between a user and a terminal
    /// repainted by a copied escape sequence, so it is not left to review.
    #[test]
    fn reveals_help_says_it_does_not_sanitize() {
        let help = Cli::command()
            .get_subcommands()
            .find(|subcommand| subcommand.get_name() == "reveal")
            .and_then(|subcommand| subcommand.get_long_about())
            .expect("reveal documents itself")
            .to_string();
        assert!(help.contains("escape sequences"), "{help}");
        assert!(help.contains("safe for a terminal"), "{help}");
    }

    /// `reveal` is the *only* exception to the sanitising rule, and `--json`
    /// reads like a second one — it prints a preview that was never flattened.
    /// It is not one, and its help is where that is said.
    #[test]
    fn jsons_help_says_how_it_is_made_safe_rather_than_leaving_it_to_be_assumed() {
        for name in ["list", "search"] {
            let help = Cli::command()
                .get_subcommands()
                .find(|subcommand| subcommand.get_name() == name)
                .and_then(|subcommand| {
                    subcommand
                        .get_arguments()
                        .find(|argument| argument.get_id() == "json")
                        .and_then(clap::Arg::get_long_help)
                })
                .expect("--json documents itself")
                .to_string();
            assert!(help.contains("\\uXXXX"), "clippo {name}: {help}");
            assert!(help.contains("decodes back"), "clippo {name}: {help}");
        }
    }
}
