//! Where `clippod`'s output goes.
//!
//! DESIGN.md asks for `tracing` + `tracing-journald`, debuggable with
//! `journalctl --user -u clippod -f`. Two details make that work in both of the
//! ways clippod actually gets run:
//!
//! - **Under systemd**, the journald layer is used, so each line carries a real
//!   priority (`journalctl -p err` works) and its structured fields stay fields
//!   rather than being flattened into the message.
//! - **From a terminal** — `just run-daemon`, or the host-terminal run DESIGN.md
//!   insists on — output goes to stderr, where the person who typed the command
//!   can see it.
//!
//! Which of the two is decided by `$JOURNAL_STREAM`, the variable systemd sets
//! on a service whose stderr it has connected to the journal. It is exactly the
//! question being asked, and it needs no configuration. Sending to journald
//! unconditionally would make a hand-run daemon print nothing at all; sending to
//! both would log every line twice under systemd, because stderr is already
//! going to the journal there.
//!
//! Verbosity comes from `$CLIPPO_LOG`, falling back to `$RUST_LOG`, falling
//! back to `info`. Both take the usual `tracing` filter syntax, so
//! `CLIPPO_LOG=clippod=debug,clippo_wayland=trace` works.

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// The variable that overrides verbosity, checked before `RUST_LOG`.
///
/// Its own name so that turning clippo up to `debug` does not also turn up
/// every other Rust program in a shell that exports `RUST_LOG`.
const LOG_ENV: &str = "CLIPPO_LOG";

/// The variable systemd sets on a service whose stderr it has connected to the
/// journal.
const JOURNAL_STREAM_ENV: &str = "JOURNAL_STREAM";

/// Start logging. Call once, before anything worth logging.
///
/// Returns a one-line description of where the output went, which the caller
/// logs — so the first line in the journal says how the rest got there.
pub fn init() -> &'static str {
    let filter = EnvFilter::try_from_env(LOG_ENV)
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("info"));

    if std::env::var_os(JOURNAL_STREAM_ENV).is_some() {
        match tracing_journald::layer() {
            Ok(journald) => {
                tracing_subscriber::registry()
                    .with(filter)
                    .with(journald)
                    .init();
                return "the journal, via tracing-journald";
            }
            Err(error) => {
                // Fall through to stderr, which systemd is already forwarding to
                // the journal — so nothing is lost, only the structure.
                tracing_subscriber::registry()
                    .with(filter)
                    .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
                    .init();
                tracing::warn!(
                    error = %error,
                    "clippo could not open the journal socket, so it is logging to stderr; \
                     systemd forwards that to the journal too, without the structured fields"
                );
                return "stderr, forwarded to the journal by systemd";
            }
        }
    }

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();
    "stderr"
}
