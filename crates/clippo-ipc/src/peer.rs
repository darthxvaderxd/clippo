//! Who is on the other end of this connection — and, more importantly, what
//! that answer is worth.
//!
//! # What this is
//!
//! Given a peer's unique bus name, [`check`] asks the bus for its pid, reads
//! `/proc/<pid>/exe`, and compares that against the clippo binaries this
//! machine has installed. A peer inside a Flatpak sandbox — one with
//! `/.flatpak-info` in its mount namespace — fails regardless of what its exe
//! resolves to.
//!
//! It is used in **both** directions, which is the whole reason it is one
//! function rather than two:
//!
//! - `clippod` points it at the *caller* of `Paste`, so a peer that is not a
//!   clippo frontend cannot have the daemon type into a focused window.
//! - `clippo` and `clippo-applet` point it at the *owner* of
//!   [`BUS_NAME`][crate::BUS_NAME], so a peer that took the name while the
//!   daemon was down does not get handed every search keystroke and every row
//!   the user is shown.
//!
//! # What this is not
//!
//! **It is not authentication.** Nothing here establishes that the peer is who
//! it says it is; it establishes that the peer is *running a file we recognise*,
//! which is a much weaker claim, and one with at least three holes in it:
//!
//! - **Pids are reusable.** The process behind a pid can exit between the bus
//!   answering and this code reading `/proc`, and the number can be handed to
//!   something else. The bus's own answer is a snapshot, not a lease.
//! - **An allowlisted binary can be driven by whoever started it.** `clippo` is
//!   on the list, so anything that can run `clippo` inherits everything `clippo`
//!   is allowed to do. The check bounds the *file*, not the intent behind it.
//! - **A uid check would be vacuous**, which is why there is not one. Every peer
//!   on a session bus is the same uid by construction, so "is the owner my user"
//!   is true for an impostor too. The same reasoning rules out D-Bus policy
//!   files: `<deny own="…"/>` can only discriminate by uid.
//!
//! So what this buys is narrowing impersonation from *anything on the bus* to
//! *anything that can be, or can drive, the real binary*. **That is a speed
//! bump, not a boundary.** It is worth having because it is a handful of lines
//! and because it is the same helper in both directions — not because the gap
//! is closed. The genuinely correct answer for a clipboard manager on Wayland
//! is compositor-mediated access, which does not exist yet; see DESIGN.md's
//! "Known risks" table, which says the same thing in the same words so that a
//! reader of either one cannot come away thinking this is a boundary.
//!
//! # Failing closed
//!
//! Every way of *not* getting an answer — the bus refusing to say, no pid in
//! the credentials, `/proc/<pid>/exe` unreadable — is a refusal, not a pass.
//! "We could not tell" and "it is not ours" have the same consequence, and a
//! check that admits the peers it could not identify would be worth nothing at
//! all.

use std::fmt;
use std::path::{Path, PathBuf};

use zbus::names::{BusName, UniqueName};
use zbus::{fdo, Connection};

/// The binaries that legitimately speak this interface.
///
/// `clippod` serves it; `clippo` and `clippo-applet` call it. `clippo-watch` is
/// deliberately absent — it is a debugging tool that talks to the compositor
/// and never touches the bus.
pub const CLIPPO_BINARIES: [&str; 3] = ["clippod", "clippo", "clippo-applet"];

/// Where an installed clippo is looked for, besides the directory the running
/// process is in.
///
/// `just install` puts the binaries in `$prefix/bin` with `prefix` defaulting to
/// `~/.local`; a distribution package would use `/usr`, and a hand-built
/// `make install` `/usr/local`. `~/.local/bin` is added at runtime from `$HOME`,
/// which is why it is not in this list.
const SYSTEM_BIN_DIRS: [&str; 2] = ["/usr/bin", "/usr/local/bin"];

/// A peer that passed the check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// Its unique name on the bus, `:1.42`-style.
    pub unique_name: String,
    /// The pid the bus reported for it.
    pub pid: u32,
    /// What `/proc/<pid>/exe` resolved to.
    pub exe: PathBuf,
}

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (pid {}, {})",
            self.unique_name,
            self.pid,
            self.exe.display()
        )
    }
}

/// Why a peer was not accepted.
///
/// Every variant is a refusal. There is no "probably fine" case — see the
/// module docs on failing closed.
#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    /// The message carried no sender. A bus always sets one, so this means the
    /// call did not come over a bus at all.
    #[error("the call carried no sender, so there is no peer to identify")]
    NoSender,

    /// The bus would not say who the peer is.
    #[error("the bus would not say which process is behind {name}: {source}")]
    Credentials {
        /// The name that was asked about.
        name: String,
        /// What the bus said instead.
        #[source]
        source: zbus::Error,
    },

    /// The credentials came back without a process id.
    #[error(
        "the bus reported no unix process id for {name}, so there is nothing to check — this is \
         a session bus that is not on a unix socket"
    )]
    NoPid {
        /// The name that was asked about.
        name: String,
    },

    /// `/proc/<pid>/exe` could not be read.
    #[error("could not read /proc/{pid}/exe to see what {name} is running: {source}")]
    NoExe {
        /// The name that was asked about.
        name: String,
        /// The pid the bus reported.
        pid: u32,
        /// Why the link would not resolve.
        #[source]
        source: std::io::Error,
    },

    /// The peer is inside a Flatpak sandbox.
    #[error(
        "{name} is pid {pid}, which is running inside a Flatpak sandbox (/.flatpak-info) — no \
         clippo binary runs sandboxed, and a sandboxed peer is exactly the case this check exists \
         for"
    )]
    Sandboxed {
        /// The name that was asked about.
        name: String,
        /// The pid the bus reported.
        pid: u32,
    },

    /// The peer's exe is not one of the installed clippo binaries.
    #[error(
        "{name} is pid {pid}, which is running {exe} — not one of the clippo binaries installed \
         beside this one"
    )]
    NotAllowlisted {
        /// The name that was asked about.
        name: String,
        /// The pid the bus reported.
        pid: u32,
        /// What `/proc/<pid>/exe` resolved to.
        exe: PathBuf,
    },
}

impl PeerError {
    /// The refused peer's pid, when the failure got far enough to have one.
    ///
    /// For the log line, not for the message — [`Display`][fmt::Display]
    /// already names it. A structured field is what makes `journalctl -o json`
    /// able to filter on it, and this is the record an operator goes back to.
    pub fn pid(&self) -> Option<u32> {
        match self {
            PeerError::NoSender | PeerError::Credentials { .. } | PeerError::NoPid { .. } => None,
            PeerError::NoExe { pid, .. }
            | PeerError::Sandboxed { pid, .. }
            | PeerError::NotAllowlisted { pid, .. } => Some(*pid),
        }
    }

    /// What the refused peer was running, when it got as far as being read.
    pub fn exe(&self) -> Option<&Path> {
        match self {
            PeerError::NotAllowlisted { exe, .. } => Some(exe),
            _ => None,
        }
    }
}

/// The executables a clippo peer may be running.
///
/// Built once and reused: [`installed`][Self::installed] stats the filesystem,
/// and doing that on every `Paste` would be a syscall storm for an answer that
/// does not change while the daemon is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerPolicy {
    allowed: Vec<PathBuf>,
}

impl PeerPolicy {
    /// The clippo binaries this machine has: [`CLIPPO_BINARIES`] in the
    /// directory the running process is in, in `~/.local/bin`, and in
    /// `SYSTEM_BIN_DIRS`.
    ///
    /// **The running process's own directory is the important one**, because
    /// clippo's three binaries are installed together and built together: an
    /// installed daemon in `~/.local/bin` finds an installed `clippo` beside
    /// it, and a `cargo run -p clippod` finds `target/debug/clippo` beside it.
    /// The standard prefixes are what let a daemon started out of a checkout
    /// still answer an installed frontend.
    ///
    /// The combination this deliberately does *not* admit is an **installed**
    /// daemon talking to a frontend from a build directory — `Paste` from
    /// `target/debug/clippo` against `~/.local/bin/clippod` is refused, and
    /// says which exe it saw. Widening the rule to cover it means admitting
    /// arbitrary paths, which is the whole of the check.
    pub fn installed() -> Self {
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
        {
            dirs.push(dir);
        }
        if let Some(home) = std::env::var_os("HOME") {
            dirs.push(Path::new(&home).join(".local/bin"));
        }
        dirs.extend(SYSTEM_BIN_DIRS.iter().map(PathBuf::from));

        Self::from_paths(
            dirs.iter()
                .flat_map(|dir| CLIPPO_BINARIES.iter().map(|name| dir.join(name))),
        )
    }

    /// An explicit list of executables, for tests and for callers that know
    /// better than [`installed`][Self::installed] does.
    ///
    /// Each path is canonicalised if it exists, because `/proc/<pid>/exe`
    /// resolves symlinks and an allowlist that did not would refuse the very
    /// binary it names. A path that does not exist is kept verbatim: an
    /// allowlist entry for a binary that is not installed simply never matches,
    /// which is the right outcome and not worth an error.
    pub fn from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut allowed: Vec<PathBuf> = paths
            .into_iter()
            .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
            .collect();
        allowed.sort();
        allowed.dedup();
        Self { allowed }
    }

    /// Whether an exe is on the list. Exact paths, not names: a file called
    /// `clippo` in a directory of the attacker's choosing is not this one.
    pub fn allows(&self, exe: &Path) -> bool {
        self.allowed.iter().any(|allowed| allowed == exe)
    }

    /// The paths on the list, for the daemon's startup log.
    pub fn paths(&self) -> &[PathBuf] {
        &self.allowed
    }
}

impl Default for PeerPolicy {
    fn default() -> Self {
        Self::installed()
    }
}

/// What is behind a well-known name right now.
///
/// Three answers rather than two, because "nobody is there" and "somebody
/// unrecognised is there" mean opposite things to a frontend: the first is a
/// daemon to start, the second is a peer to refuse.
#[derive(Debug)]
pub enum Owner {
    /// Nothing owns the name.
    Absent,
    /// A peer owns it and passed the check.
    Trusted(Peer),
    /// A peer owns it and did not pass the check. Do not send it anything.
    Untrusted(PeerError),
}

/// Check the peer behind a unique bus name.
///
/// `name` must be a unique name (`:1.42`): a well-known one would have the bus
/// answer about whoever owns it *now*, which is a different question from "who
/// sent this message". Use [`owner_of`] when the well-known name is what you
/// have.
pub async fn check(
    connection: &Connection,
    name: &UniqueName<'_>,
    policy: &PeerPolicy,
) -> Result<Peer, PeerError> {
    let dbus = fdo::DBusProxy::new(connection)
        .await
        .map_err(|source| PeerError::Credentials {
            name: name.to_string(),
            source,
        })?;
    let credentials = dbus
        .get_connection_credentials(BusName::from(name.as_ref()))
        .await
        .map_err(|source| PeerError::Credentials {
            name: name.to_string(),
            source: source.into(),
        })?;

    judge(name.as_str(), credentials.process_id(), policy)
}

/// [`check`], for a caller with no async runtime.
///
/// The transport is the only difference: the decision below it is the same
/// `judge` both directions run.
pub fn check_blocking(
    connection: &zbus::blocking::Connection,
    name: &UniqueName<'_>,
    policy: &PeerPolicy,
) -> Result<Peer, PeerError> {
    let dbus = zbus::blocking::fdo::DBusProxy::new(connection).map_err(|source| {
        PeerError::Credentials {
            name: name.to_string(),
            source,
        }
    })?;
    let credentials = dbus
        .get_connection_credentials(BusName::from(name.as_ref()))
        .map_err(|source| PeerError::Credentials {
            name: name.to_string(),
            source: source.into(),
        })?;

    judge(name.as_str(), credentials.process_id(), policy)
}

/// Resolve a well-known name and check whoever holds it.
pub async fn owner_of(
    connection: &Connection,
    name: &str,
    policy: &PeerPolicy,
) -> Result<Owner, zbus::Error> {
    let dbus = fdo::DBusProxy::new(connection).await?;
    let bus_name = BusName::try_from(name).expect("clippo's bus names are valid ones");
    let owner = match dbus.get_name_owner(bus_name).await {
        Ok(owner) => owner,
        Err(fdo::Error::NameHasNoOwner(_)) => return Ok(Owner::Absent),
        Err(error) => return Err(error.into()),
    };

    Ok(judged(check(connection, owner.inner(), policy).await))
}

/// [`owner_of`], for a caller with no async runtime.
pub fn owner_of_blocking(
    connection: &zbus::blocking::Connection,
    name: &str,
    policy: &PeerPolicy,
) -> Result<Owner, zbus::Error> {
    let dbus = zbus::blocking::fdo::DBusProxy::new(connection)?;
    let bus_name = BusName::try_from(name).expect("clippo's bus names are valid ones");
    let owner = match dbus.get_name_owner(bus_name) {
        Ok(owner) => owner,
        Err(fdo::Error::NameHasNoOwner(_)) => return Ok(Owner::Absent),
        Err(error) => return Err(error.into()),
    };

    Ok(judged(check_blocking(connection, owner.inner(), policy)))
}

/// One `Result` into one [`Owner`]. Here rather than inline so the two
/// transports cannot disagree about which failures are refusals — they all are.
fn judged(checked: Result<Peer, PeerError>) -> Owner {
    match checked {
        Ok(peer) => Owner::Trusted(peer),
        Err(error) => Owner::Untrusted(error),
    }
}

/// The decision, with the bus already out of the picture.
///
/// Split out from [`check`] for two reasons: it is what makes the async and
/// blocking paths one rule rather than two copies, and it is the only part that
/// can be tested without a live peer to be.
fn judge(name: &str, pid: Option<u32>, policy: &PeerPolicy) -> Result<Peer, PeerError> {
    judge_with(name, pid, policy, is_sandboxed)
}

/// [`judge`], with the sandbox test as a parameter.
///
/// It is a parameter for one reason: **the sandbox refusal is otherwise
/// unreachable from a test.** Nothing in this suite runs inside a Flatpak, so
/// `is_sandboxed` answers `false` for every pid a test could name, and a
/// refusal that has never executed is a refusal nobody has checked. It is the
/// case the whole check exists for, so it is the last one that should go
/// untested.
fn judge_with(
    name: &str,
    pid: Option<u32>,
    policy: &PeerPolicy,
    sandboxed: impl Fn(u32) -> bool,
) -> Result<Peer, PeerError> {
    let pid = pid.ok_or_else(|| PeerError::NoPid {
        name: name.to_owned(),
    })?;

    // Before the exe, deliberately: a sandboxed peer's `/proc/<pid>/exe`
    // resolves inside its own mount namespace, so `/app/bin/clippod` would read
    // as a plausible path to somebody who had not thought about it.
    if sandboxed(pid) {
        return Err(PeerError::Sandboxed {
            name: name.to_owned(),
            pid,
        });
    }

    let exe =
        std::fs::read_link(format!("/proc/{pid}/exe")).map_err(|source| PeerError::NoExe {
            name: name.to_owned(),
            pid,
            source,
        })?;

    if !policy.allows(&exe) {
        return Err(PeerError::NotAllowlisted {
            name: name.to_owned(),
            pid,
            exe,
        });
    }

    Ok(Peer {
        unique_name: name.to_owned(),
        pid,
        exe,
    })
}

/// Whether a pid is inside a Flatpak sandbox.
///
/// `/.flatpak-info` is what the Flatpak runtime mounts into every sandbox and
/// what portals themselves use to tell a sandboxed caller apart; reaching it
/// through `/proc/<pid>/root` is reaching into that peer's mount namespace
/// rather than our own. A pid that has gone away answers `false`, and the exe
/// read below then fails — which is the same refusal by a different route.
fn is_sandboxed(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}/root/.flatpak-info")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_of(paths: &[&str]) -> PeerPolicy {
        PeerPolicy::from_paths(paths.iter().map(PathBuf::from))
    }

    /// The positive case against a real process: this test's own executable,
    /// resolved through `/proc/<pid>/exe` exactly as a live peer's would be.
    /// Nothing here is a fixture path — if the reader of `/proc` or the
    /// comparison were wrong, the binary that is genuinely on the list would be
    /// refused.
    #[test]
    fn a_peer_running_an_allowlisted_binary_is_accepted() {
        let me = std::env::current_exe().expect("this test process has an exe");
        let policy = PeerPolicy::from_paths([me.clone()]);

        let peer = judge(":1.7", Some(std::process::id()), &policy).expect("this process");
        assert_eq!(peer.pid, std::process::id());
        assert_eq!(peer.exe, std::fs::canonicalize(&me).unwrap_or(me));
        assert_eq!(peer.unique_name, ":1.7");
    }

    /// And the same process against a list it is not on. Same pid, same
    /// `/proc` read, opposite answer — so the accept above is the allowlist
    /// agreeing rather than the check never refusing anything.
    #[test]
    fn a_peer_running_anything_else_is_refused() {
        let policy = policy_of(&["/usr/bin/clippod"]);

        let error = judge(":1.7", Some(std::process::id()), &policy).expect_err("not on the list");
        assert!(
            matches!(&error, PeerError::NotAllowlisted { pid, .. } if *pid == std::process::id()),
            "{error:?}"
        );
        // The refusal has to name what it saw, or the journal line it becomes
        // is unactionable — in the message, and as fields the journal can be
        // filtered on.
        let me = std::env::current_exe().expect("an exe");
        let me = std::fs::canonicalize(&me).unwrap_or(me);
        let printed = error.to_string();
        assert!(printed.contains(&me.display().to_string()), "{printed}");
        assert_eq!(error.pid(), Some(std::process::id()));
        assert_eq!(error.exe(), Some(me.as_path()));
    }

    /// Failing closed, both ways of failing.
    #[test]
    fn a_peer_that_cannot_be_identified_is_refused_rather_than_admitted() {
        let policy = PeerPolicy::installed();

        let no_pid = judge(":1.7", None, &policy).expect_err("no pid is a refusal");
        assert!(matches!(no_pid, PeerError::NoPid { .. }), "{no_pid:?}");

        // Pid 0 is never a process, so `/proc/0/exe` cannot be read.
        let no_exe = judge(":1.7", Some(0), &policy).expect_err("no exe is a refusal");
        assert!(matches!(no_exe, PeerError::NoExe { .. }), "{no_exe:?}");
    }

    /// The rule an installed tree relies on: the three binaries, in the
    /// directory the running process is in.
    #[test]
    fn the_installed_policy_covers_the_binaries_beside_this_one() {
        let policy = PeerPolicy::installed();
        let dir = std::env::current_exe()
            .expect("an exe")
            .parent()
            .expect("an exe has a directory")
            .to_path_buf();

        for name in CLIPPO_BINARIES {
            assert!(
                policy.allows(&dir.join(name)),
                "{name} beside this process should be allowed; list is {:?}",
                policy.paths()
            );
        }
        assert!(
            !policy.allows(&dir.join("clippo-watch")),
            "{:?}",
            policy.paths()
        );
        assert!(!policy.allows(Path::new("/usr/bin/python3")));
    }

    /// An allowlist entry naming a symlink has to match what `/proc` reports,
    /// which is the target. Canonicalising at construction is what makes the
    /// two comparable at all.
    #[test]
    fn an_allowlisted_symlink_matches_the_binary_it_points_at() {
        let me = std::env::current_exe().expect("an exe");
        let dir = tempfile::tempdir().expect("a temporary directory");
        let link = dir.path().join("clippo");
        std::os::unix::fs::symlink(&me, &link).expect("a symlink to this test binary");

        let policy = PeerPolicy::from_paths([link]);
        assert!(policy.allows(&std::fs::canonicalize(&me).unwrap_or(me)));
    }

    /// The case the check exists for: F1's Flatpak application, which was
    /// denied data-control and `zwp_virtual_keyboard_v1` by the proxied Wayland
    /// socket and is trying to get them back through clippo.
    ///
    /// It is refused even when its exe is on the list — a sandbox's
    /// `/proc/<pid>/exe` resolves inside its own mount namespace, so a peer
    /// that mounted its payload at the right path would otherwise pass. The
    /// sandbox test comes first for exactly that reason, and this is the test
    /// that says so.
    #[test]
    fn a_sandboxed_peer_is_refused_even_when_its_exe_looks_right() {
        let me = std::env::current_exe().expect("an exe");
        let allowed = PeerPolicy::from_paths([me]);

        // Same pid, same allowlist, one bit different.
        judge_with(":1.7", Some(std::process::id()), &allowed, |_| false)
            .expect("not sandboxed, and on the list");

        let error = judge_with(":1.7", Some(std::process::id()), &allowed, |_| true)
            .expect_err("a sandboxed peer is refused whatever its exe says");
        assert!(matches!(error, PeerError::Sandboxed { .. }), "{error:?}");
    }

    /// A name that is on nothing's list still produces a message a user can
    /// act on, because the picker and the CLI both print it verbatim.
    #[test]
    fn every_refusal_names_the_peer_it_refused() {
        let sandboxed = PeerError::Sandboxed {
            name: ":1.9".to_owned(),
            pid: 4321,
        };
        let printed = sandboxed.to_string();
        assert!(printed.contains(":1.9"), "{printed}");
        assert!(printed.contains("4321"), "{printed}");
        assert!(printed.contains("flatpak-info"), "{printed}");
    }
}
