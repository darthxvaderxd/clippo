//! Everything that can go wrong, said in one line on stderr.
//!
//! # The failure that matters
//!
//! By far the most common one is that `clippod` is not running, and zbus's own
//! words for it are `MethodError: org.freedesktop.DBus.Error.ServiceUnknown:
//! The name com.nilfactor.Clippo was not provided by any .service files`,
//! which tells a user who has never heard of D-Bus activation nothing they can
//! act on. [`CliError::from_call`] detects that one case by its D-Bus error
//! name and replaces it with a sentence naming the service and how to start it.
//!
//! Every other bus error keeps the daemon's own message — those are written for
//! a user already (`clippo cannot put entry 2 back on the clipboard yet…`) and
//! rewriting them here would put the explanation two crates away from the code
//! that knows why.

use std::io;

use zbus::DBusError as _;

use crate::ids::ResolveError;

/// The D-Bus error names that mean "there is no daemon".
///
/// `ServiceUnknown` is what a call to an unowned, non-activatable name gets;
/// `NameHasNoOwner` is what the bus says when asked about the name directly.
/// Both mean the same thing to a user.
const NO_DAEMON: [&str; 2] = [
    "org.freedesktop.DBus.Error.ServiceUnknown",
    "org.freedesktop.DBus.Error.NameHasNoOwner",
];

/// Anything that stops a subcommand finishing. Printed with a `clippo: ` prefix
/// on stderr; the process then exits non-zero.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error(
        "clippod is not running — nothing owns {bus} on the session bus. Start it with \
         `systemctl --user start clippod`, or run it in a host terminal with \
         `cargo run -p clippod`",
        bus = clippo_ipc::BUS_NAME
    )]
    DaemonNotRunning,

    #[error(
        "the clippo applet is not running — nothing owns {bus} on the session bus. `clippo \
         show` asks the panel applet to open its picker, so it needs the applet, not just the \
         daemon: add clippo to the panel in COSMIC Settings → Desktop → Panel → Configure \
         panel applets",
        bus = clippo_ipc::APPLET_BUS_NAME
    )]
    AppletNotRunning,

    #[error(
        "could not reach the session bus, which is where clippod serves ({0}). A desktop \
         session sets DBUS_SESSION_BUS_ADDRESS; a bare login shell or a container may not"
    )]
    NoSessionBus(#[source] zbus::Error),

    #[error("the daemon refused {member}: {message}")]
    Call {
        member: &'static str,
        message: String,
    },

    #[error("{0}")]
    Resolve(#[from] ResolveError),

    #[error(
        "`clippo clear` deletes the whole history and cannot be undone, so it needs \
         confirming — and stdin is not a terminal, so there is nobody to ask. Pass --yes if \
         you mean it"
    )]
    ClearNeedsConfirmation,

    #[error("nothing was cleared; the history is unchanged")]
    Aborted,

    #[error("could not read your answer ({0}); nothing was cleared")]
    Answer(#[source] io::Error),

    #[error("could not build the JSON output: {0}")]
    Json(#[source] serde_json::Error),

    #[error("could not write the output: {0}")]
    Stdout(#[source] io::Error),
}

impl CliError {
    /// Turn a failed D-Bus call into something worth printing.
    ///
    /// `member` is the interface member that failed, so a message that came
    /// back without a description still says which call it belongs to.
    pub fn from_call(member: &'static str, error: zbus::Error) -> Self {
        if error_name(&error).is_some_and(|name| NO_DAEMON.contains(&name.as_str())) {
            return CliError::DaemonNotRunning;
        }
        CliError::Call {
            member,
            message: describe(&error),
        }
    }

    /// The same, for the applet's interface rather than the daemon's.
    ///
    /// Separate from [`from_call`][Self::from_call] because the two absences
    /// have different fixes: starting `clippod` will not put the applet on the
    /// panel, and a user told to do the wrong one of those will not get their
    /// popup. The bus reports both the same way, so telling them apart is a
    /// matter of knowing which name was being called.
    pub fn from_applet_call(member: &'static str, error: zbus::Error) -> Self {
        if error_name(&error).is_some_and(|name| NO_DAEMON.contains(&name.as_str())) {
            return CliError::AppletNotRunning;
        }
        CliError::Call {
            member,
            message: describe(&error),
        }
    }

    /// Failing to connect at all: no bus, rather than no daemon on it.
    pub fn from_connect(error: zbus::Error) -> Self {
        CliError::NoSessionBus(error)
    }
}

/// The D-Bus error name of a failed call, when it has one.
fn error_name(error: &zbus::Error) -> Option<String> {
    match error {
        zbus::Error::MethodError(name, _, _) => Some(name.as_str().to_owned()),
        zbus::Error::FDO(error) => Some(error.name().as_str().to_owned()),
        _ => None,
    }
}

/// What the far end said, preferring its own words.
///
/// A D-Bus error reply carries a name and, by convention, a human description.
/// The description is the sentence the daemon wrote; the name on its own
/// (`…Error.NoReply`) is all there is when it did not.
fn describe(error: &zbus::Error) -> String {
    match error {
        zbus::Error::MethodError(name, description, _) => description
            .clone()
            .unwrap_or_else(|| name.as_str().to_owned()),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::fdo;

    #[test]
    fn an_absent_daemon_is_reported_as_an_absent_daemon() {
        let error = CliError::from_call(
            "List",
            zbus::Error::FDO(Box::new(fdo::Error::ServiceUnknown(
                "The name com.nilfactor.Clippo was not provided by any .service files".to_owned(),
            ))),
        );
        assert!(matches!(error, CliError::DaemonNotRunning));

        // Naming the service and the way to start it is the whole point.
        let printed = error.to_string();
        assert!(printed.contains(clippo_ipc::BUS_NAME), "{printed}");
        assert!(
            printed.contains("systemctl --user start clippod"),
            "{printed}"
        );
        assert!(!printed.contains(".service files"), "{printed}");
    }

    #[test]
    fn an_unowned_name_is_the_same_answer() {
        assert!(matches!(
            CliError::from_call(
                "Paused",
                zbus::Error::FDO(Box::new(fdo::Error::NameHasNoOwner("nope".to_owned()))),
            ),
            CliError::DaemonNotRunning
        ));
    }

    /// Every *other* bus error keeps the daemon's own sentence, which is
    /// written for the user. `Copy` today is exactly this case.
    #[test]
    fn any_other_failure_keeps_the_daemons_own_words() {
        let error = CliError::from_call(
            "Copy",
            zbus::Error::FDO(Box::new(fdo::Error::NotSupported(
                "clippo cannot put entry 2 back on the clipboard yet".to_owned(),
            ))),
        );
        let printed = error.to_string();
        assert!(matches!(error, CliError::Call { member: "Copy", .. }));
        assert!(printed.contains("Copy"), "{printed}");
        assert!(
            printed.contains("clippo cannot put entry 2 back on the clipboard yet"),
            "{printed}"
        );
    }

    /// A transport failure has no D-Bus error name and must not be mistaken
    /// for a missing daemon — the fix for it is a different one.
    #[test]
    fn a_failure_with_no_error_name_is_not_read_as_a_missing_daemon() {
        let error = CliError::from_call("List", zbus::Error::InvalidReply);
        assert!(matches!(error, CliError::Call { .. }), "{error:?}");
        assert!(error.to_string().contains("List"), "{error}");
    }

    #[test]
    fn the_no_daemon_names_are_the_ones_the_bus_actually_sends() {
        // `fdo::Error` derives its names from its variants; pinning the
        // strings here means a typo in NO_DAEMON fails a test rather than
        // quietly leaving the raw zbus text in front of a user.
        assert_eq!(
            fdo::Error::ServiceUnknown(String::new()).name().as_str(),
            NO_DAEMON[0]
        );
        assert_eq!(
            fdo::Error::NameHasNoOwner(String::new()).name().as_str(),
            NO_DAEMON[1]
        );
    }
}
