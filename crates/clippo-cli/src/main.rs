//! `clippo` — the command-line client for the clipboard daemon.
//!
//! DESIGN.md: *"Ship it before the GUI — it makes every layer below testable
//! without touching a UI."* Everything the daemon can do is reachable from a
//! terminal here, which is what makes M1's capture and M2's storage checkable
//! by hand, and what ROADMAP.md's round-trip verification is written against.
//!
//! ```text
//!            D-Bus  com.nilfactor.Clippo
//!   clippo ─────────────────────────────▶ clippod ──▶ SQLCipher history
//! ```
//!
//! **No database access anywhere in this binary.** The store is the daemon's,
//! and a second process opening the same SQLCipher file would need the keyring
//! key, would race the daemon's writes, and would leave the daemon's in-memory
//! search cache describing a history that no longer exists. Every subcommand is
//! a proxy call, and the proxy is `clippo-ipc`'s — the same declarations the
//! daemon serves, so a signature cannot drift between the two.
//!
//! # Layout
//!
//! - [`cli`] — the argument surface, and the `--help` a user reads.
//! - [`client`] — the blocking proxy, one method per member.
//! - [`ids`] — turning a typed `2` or `14` into one entry's id.
//! - [`render`] — the table, the JSON, and making a preview safe to print.
//! - [`run`] — what each subcommand actually does.
//! - [`error`] — one line on stderr, including the "clippod is not running"
//!   case that every other error would otherwise be mistaken for.

mod cli;
mod client;
mod error;
mod ids;
mod render;
mod run;

use std::io::{self, Write};
use std::process::ExitCode;

use clap::Parser;

use crate::cli::Cli;

fn main() -> ExitCode {
    // clap handles `--help`, `--version` and a misspelled flag itself, exiting
    // non-zero with its own message; what reaches here is a command to run.
    match run::run(Cli::parse().command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Not `eprintln!`: `clippo list 2>&1 | head` can close stderr too,
            // and panicking while reporting a failure is a worse failure.
            let _ = writeln!(io::stderr(), "clippo: {error}");
            ExitCode::FAILURE
        }
    }
}
