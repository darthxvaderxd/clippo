//! The database key: where it comes from, and where it is allowed to live.
//!
//! One 32-byte key encrypts the whole history database. It is **random or
//! nothing** — nothing here derives a key from a hostname, a username, a path,
//! a machine id or any other value an attacker holding the file could guess.
//! The only two sources are the operating system's CSPRNG and whichever of the
//! two stores below already holds a key put there by an earlier run.
//!
//! # Where the key lives, in order
//!
//! 1. **`~/.local/share/clippo/key`, if it already exists.** A key file that is
//!    there is the key the database next to it was encrypted with, so it wins
//!    over the Secret Service. Preferring the keyring here would mean that a
//!    machine which fell back once and later gained a working keyring would
//!    hand SQLCipher a key that cannot decrypt its own history — a working
//!    install that breaks on the day the keyring starts working. The warning
//!    below is logged every time this path is taken, so the situation is
//!    visible rather than silent.
//! 2. **The Secret Service, through [`oo7`].** A 32-byte key stored as
//!    lowercase hex under [`KEYRING_ATTRIBUTES`]. Created on first run.
//! 3. **A new `~/.local/share/clippo/key`, mode `0600`**, when the Secret
//!    Service cannot be reached *and there is no `history.db` yet* — with a
//!    `WARN` naming the file and saying the key is on disk unencrypted. Silent
//!    fallback is the failure worth avoiding here: a user whose key quietly
//!    moved to a plain file should be able to find that out from the daemon's
//!    log.
//! 4. **Otherwise, refuse to start.** If the Secret Service is unreachable and a
//!    history database already exists, minting a key file would be the last
//!    thing that ever happens to that database: rule 1 would hand the new file
//!    key to SQLCipher on every subsequent run, so the keyring key that does
//!    open it would never be consulted again, and a single transient keyring
//!    outage — `clippod.service` has no ordering against gnome-keyring — would
//!    cost the user their whole history permanently. Failing instead keeps the
//!    working key reachable, and the next start with a live Secret Service
//!    recovers on its own.
//!
//! An existing key file whose mode lets anyone but its owner read it is
//! **refused**, not used. Wider-than-`0600` on a file that is the only thing
//! standing between another local account and the clipboard history is a signal
//! that something went wrong, and carrying on would encrypt the next copy under
//! a key that account can already read.
//!
//! The directory holding it gets the same attention, in the milder form the
//! difference deserves: `~/.local/share/clippo` is created at `0700` and, if it
//! was already there at something wider, narrowed to `0700` rather than used as
//! found — see `create_data_dir`. A leaked key cannot be un-leaked, so that is
//! refused; a wide directory can simply be closed, and refusing to start over
//! something clippo can fix itself would be an outage in the name of a warning.
//!
//! # Getting back to the keyring
//!
//! Rule 1 is absolute *once a key file exists*, so there is no automatic
//! migration from the file back to the Secret Service; rule 4 is what stops one
//! ever being created underneath a database it cannot open. A machine that
//! genuinely has no keyring gets its file key on first run, when there is no
//! database to protect, and keeps it. Deleting both `key` and `history.db`
//! starts over with a keyring-held key; deleting only `key` leaves a database
//! nothing can decrypt. That is deliberate — clippo will not quietly discard
//! history it can still read.

use std::fmt;
use std::fmt::Write as _;
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use clippo_core::paths;
use zeroize::{Zeroize, Zeroizing};

/// Bytes of key material. SQLCipher's raw-key form takes exactly this many.
pub const KEY_BYTES: usize = 32;

/// The fallback key file's name inside the data directory.
pub const KEY_FILE_NAME: &str = "key";

/// Mode bits that must not be set on the fallback key file, or on the data
/// directory holding it: anything that gives group or other any access at all.
const FORBIDDEN_MODE_BITS: u32 = 0o077;

/// The mode clippo's data directory is kept at.
const DATA_DIR_MODE: u32 = 0o700;

/// The label the Secret Service shows for clippo's key.
pub const KEYRING_LABEL: &str = "clippo clipboard history database key";

/// The attribute set clippo's key is stored and looked up under.
///
/// Stable across versions on purpose — this is the lookup key, so changing it
/// orphans every key already in a user's keyring and silently generates a new
/// one that cannot open their history.
pub const KEYRING_ATTRIBUTES: &[(&str, &str)] = &[
    // The convention every Secret Service client follows for namespacing.
    ("xdg:schema", "com.nilfactor.Clippo.DatabaseKey"),
    ("application", "clippo"),
    ("purpose", "history-database-key"),
];

/// Which of the two stores the key in hand came from.
///
/// Returned alongside the key so the daemon can say so at startup: "key from
/// the Secret Service" and "key from a file on disk" are meaningfully different
/// security postures and the user is entitled to know which one they have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// The Secret Service, via `oo7`.
    SecretService,
    /// A file on disk, mode `0600`.
    File(PathBuf),
}

impl fmt::Display for KeySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SecretService => f.write_str("the Secret Service"),
            Self::File(path) => write!(f, "the file {} (unencrypted)", path.display()),
        }
    }
}

/// 32 bytes of key material for SQLCipher.
///
/// Zeroed on drop, and `Debug` prints nothing of the value: a key in a log line
/// is a key in the journal, which is exactly the place the encryption exists to
/// keep it out of.
pub struct Key([u8; KEY_BYTES]);

impl Key {
    /// A fresh key from the operating system's CSPRNG.
    ///
    /// `getrandom` reads `getrandom(2)` / `/dev/urandom`. There is no seeding
    /// and no user-supplied entropy — the only input is the kernel's.
    pub fn random() -> Result<Self, KeyError> {
        let mut bytes = [0_u8; KEY_BYTES];
        getrandom::fill(&mut bytes).map_err(KeyError::NoRandom)?;
        Ok(Self(bytes))
    }

    /// Wrap key material that came from somewhere else.
    pub const fn from_bytes(bytes: [u8; KEY_BYTES]) -> Self {
        Self(bytes)
    }

    /// The `PRAGMA key` statement that hands this key to SQLCipher.
    ///
    /// The `x'…'` form is SQLCipher's *raw* key: the 64 hex digits are used as
    /// the key directly, with no PBKDF2 pass over them. That is what we want —
    /// the value is already 32 uniformly random bytes, so a KDF would only add
    /// cost. The whole statement is built here so no caller ever holds the hex
    /// itself, and the returned string zeroes when it drops.
    ///
    /// Built by `push_str` into an exactly-sized buffer rather than by
    /// `format!`: a string that grows leaves its old, unzeroed allocation — hex
    /// key and all — behind for the allocator to hand out or the kernel to swap.
    pub(crate) fn pragma(&self) -> Zeroizing<String> {
        const PREFIX: &str = "PRAGMA key = \"x'";
        const SUFFIX: &str = "'\";";

        let hex = self.to_hex();
        let mut statement = Zeroizing::new(String::with_capacity(
            PREFIX.len() + hex.len() + SUFFIX.len(),
        ));
        statement.push_str(PREFIX);
        statement.push_str(&hex);
        statement.push_str(SUFFIX);
        statement
    }

    /// The key as 64 lowercase hex digits.
    fn to_hex(&self) -> Zeroizing<String> {
        let mut hex = String::with_capacity(KEY_BYTES * 2);
        for byte in self.0 {
            // `write!` to a String cannot fail.
            let _ = write!(&mut hex, "{byte:02x}");
        }
        Zeroizing::new(hex)
    }

    /// Parse 64 hex digits back into a key, ignoring surrounding whitespace.
    fn from_hex(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.len() != KEY_BYTES * 2 {
            return None;
        }
        let mut bytes = [0_u8; KEY_BYTES];
        for (byte, pair) in bytes.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
            let pair = std::str::from_utf8(pair).ok()?;
            *byte = u8::from_str_radix(pair, 16).ok()?;
        }
        Some(Self(bytes))
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Key(<redacted>)")
    }
}

/// Why clippo could not get hold of a database key.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// The operating system would not give us random bytes.
    #[error("could not read 32 random bytes from the operating system")]
    NoRandom(#[source] getrandom::Error),

    /// The key file is readable by more than its owner.
    #[error(
        "the clippo key file at {path} has mode {mode:04o}, which lets users other than its \
         owner read it; clippo will not use a database key that has already leaked. Fix the \
         permissions with `chmod 600 {path}` if you trust the file, or delete it (and \
         history.db with it) to start over"
    )]
    FilePermissions {
        /// The offending file.
        path: PathBuf,
        /// Its permission bits, masked to `0o777`.
        mode: u32,
    },

    /// The key file exists but does not hold a key.
    #[error(
        "the clippo key file at {path} does not contain a 32-byte key as 64 hex digits; \
         it was not written by clippo"
    )]
    MalformedFile {
        /// The offending file.
        path: PathBuf,
    },

    /// The key file could not be read.
    #[error("could not read the clippo key file at {path}")]
    ReadFile {
        /// The file clippo tried to read.
        path: PathBuf,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },

    /// The key file could not be written.
    #[error("could not write the clippo key file at {path}")]
    WriteFile {
        /// The file clippo tried to create.
        path: PathBuf,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },

    /// The data directory could not be created.
    #[error("could not create the clippo data directory at {path}")]
    CreateDataDir {
        /// The directory clippo tried to create.
        path: PathBuf,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },

    /// The data directory lets other accounts in and could not be narrowed.
    #[error(
        "the clippo data directory at {path} has mode {mode:04o}, which lets users other than \
         its owner in, and clippo could not narrow it to 0700; the encrypted history database \
         and the fallback key file both live here. Fix the permissions with `chmod 700 {path}`"
    )]
    DataDirPermissions {
        /// The offending directory.
        path: PathBuf,
        /// Its permission bits, masked to `0o777`, as clippo last saw them.
        mode: u32,
        /// Why it could not be narrowed.
        #[source]
        source: std::io::Error,
    },

    /// The Secret Service is unreachable and there is already a database that a
    /// key from it may be the only thing able to open.
    #[error(
        "no Secret Service could be reached ({reason}), and a history database already exists \
         at {database}. clippo will not create a key file at {key_file} in that situation: the \
         file would win over the Secret Service on every later start, so the key that does open \
         {database} would never be looked for again. Start the Secret Service \
         (org.freedesktop.secrets, usually gnome-keyring) and start clippo again; if the keyring \
         really is gone for good, delete {database} to start a fresh history"
    )]
    NoSecretServiceForExistingDatabase {
        /// The database that would have been shadowed.
        database: PathBuf,
        /// The key file clippo declined to create.
        key_file: PathBuf,
        /// Why the Secret Service could not be reached.
        reason: String,
    },

    /// The Secret Service holds an item under clippo's attributes, but it is
    /// not a clippo key.
    #[error(
        "the secret stored in the Secret Service under {schema} is not a 32-byte clippo key; \
         delete it and let clippo create a new one",
        schema = KEYRING_ATTRIBUTES[0].1
    )]
    MalformedSecret,

    /// There is nowhere to put the data directory.
    #[error(transparent)]
    Path(#[from] paths::PathError),
}

/// Get the database key, creating one on first run.
///
/// Uses `~/.local/share/clippo` (XDG respected) as the data directory; see
/// [`acquire_in`] for the rule it follows.
pub async fn acquire() -> Result<(Key, KeySource), KeyError> {
    acquire_in(&paths::data_dir()?).await
}

/// Get the database key, keeping any fallback file in `data_dir`.
///
/// The four-step rule is in the module docs. In short: an existing key file
/// wins, then the Secret Service, then a new key file with a warning — but only
/// while there is no `history.db` for that new key to shadow.
pub async fn acquire_in(data_dir: &Path) -> Result<(Key, KeySource), KeyError> {
    let path = data_dir.join(KEY_FILE_NAME);

    if let Some(key) = read_key_file(&path)? {
        tracing::warn!(
            key_file = %path.display(),
            "clippo's database key is stored unencrypted in a file rather than in the Secret \
             Service; anyone who can read that file can read the whole clipboard history"
        );
        return Ok((key, KeySource::File(path)));
    }

    match from_secret_service().await {
        Ok(key) => {
            tracing::debug!("clippo's database key came from the Secret Service");
            Ok((key, KeySource::SecretService))
        }
        Err(problem) => {
            let database = data_dir.join(paths::DB_FILE_NAME);
            let key = fall_back_to_file(&path, &database, &problem.to_string())?;
            Ok((key, KeySource::File(path)))
        }
    }
}

/// Mint the fallback key file — unless there is a database it would shadow.
///
/// Split out from [`acquire_in`] because this is the decision worth testing on
/// its own: reaching it through `acquire_in` means going through
/// [`from_secret_service`], and a test that only fails on machines without a
/// running keyring is not a test of anything.
///
/// The refusal is the recoverable direction. Failing to start is annoying and
/// fixes itself the moment the Secret Service comes back; writing the file is
/// silent, looks like success, and permanently hides the key that opens the
/// history behind rule 1. That asymmetry is also why an unanswerable `exists`
/// counts as "there is one".
fn fall_back_to_file(path: &Path, database: &Path, reason: &str) -> Result<Key, KeyError> {
    if database.try_exists().unwrap_or(true) {
        return Err(KeyError::NoSecretServiceForExistingDatabase {
            database: database.to_path_buf(),
            key_file: path.to_path_buf(),
            reason: reason.to_owned(),
        });
    }

    tracing::warn!(
        error = %reason,
        key_file = %path.display(),
        "no Secret Service could be reached, so clippo is storing its database key unencrypted \
         in a file with mode 0600; anyone who can read that file can read the whole clipboard \
         history. This key will be preferred over the Secret Service from now on, so move it \
         (with history.db) out of the way if you want to go back to the keyring"
    );
    create_key_file(path)
}

/// Fetch clippo's key from the Secret Service, creating one if there is none.
///
/// Any failure here — no D-Bus, no keyring daemon, a locked collection the user
/// declined to unlock — is reported to the caller, which falls back to a file.
async fn from_secret_service() -> Result<Key, SecretServiceError> {
    let keyring = oo7::Keyring::new().await?;
    keyring.unlock().await?;

    let attributes = KEYRING_ATTRIBUTES.to_vec();
    if let Some(item) = keyring.search_items(&attributes).await?.into_iter().next() {
        let secret = item.secret().await?;
        let text = Zeroizing::new(String::from_utf8_lossy(&secret).into_owned());
        // An item that is there but is not a key is a hard error rather than a
        // reason to overwrite it: something else may own it, and replacing it
        // would destroy whatever it holds.
        return Key::from_hex(&text).ok_or(SecretServiceError::Key(KeyError::MalformedSecret));
    }

    let key = Key::random().map_err(SecretServiceError::Key)?;
    keyring
        .create_item(KEYRING_LABEL, &attributes, key.to_hex().as_bytes(), true)
        .await?;
    Ok(key)
}

/// A Secret Service lookup that did not produce a key.
///
/// Only ever reported through the fallback warning, which is why it stays
/// private: callers get a key and a [`KeySource`], not this.
#[derive(Debug, thiserror::Error)]
enum SecretServiceError {
    #[error(transparent)]
    Oo7(#[from] oo7::Error),
    #[error(transparent)]
    Key(KeyError),
}

/// Read the fallback key file, or `Ok(None)` if there is not one.
///
/// Refuses a file whose mode is wider than `0600`. Narrower — `0400`, say — is
/// fine: the check is for bits that let *other* accounts in, not for an exact
/// match.
fn read_key_file(path: &Path) -> Result<Option<Key>, KeyError> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(KeyError::ReadFile {
                path: path.to_path_buf(),
                source,
            })
        }
    };

    if !metadata.is_file() {
        return Err(KeyError::MalformedFile {
            path: path.to_path_buf(),
        });
    }

    let mode = metadata.permissions().mode() & 0o777;
    if mode & FORBIDDEN_MODE_BITS != 0 {
        return Err(KeyError::FilePermissions {
            path: path.to_path_buf(),
            mode,
        });
    }

    let text =
        Zeroizing::new(
            std::fs::read_to_string(path).map_err(|source| KeyError::ReadFile {
                path: path.to_path_buf(),
                source,
            })?,
        );

    Key::from_hex(&text)
        .map(Some)
        .ok_or_else(|| KeyError::MalformedFile {
            path: path.to_path_buf(),
        })
}

/// Create the fallback key file with a fresh key, mode `0600`.
fn create_key_file(path: &Path) -> Result<Key, KeyError> {
    if let Some(parent) = path.parent() {
        create_data_dir(parent)?;
    }

    let key = Key::random()?;
    let hex = key.to_hex();

    // `create_new` rather than `create`: the caller has already established
    // that there is no key file, so finding one now means something else is
    // writing to the same data directory and overwriting it would destroy a
    // key that a database is already encrypted with.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|source| KeyError::WriteFile {
            path: path.to_path_buf(),
            source,
        })?;

    // umask can only clear bits, so the file is at most 0600 and possibly
    // narrower; both pass the check `read_key_file` will make next startup.
    //
    // The newline is a second `write_all` rather than a `push` onto the hex:
    // pushing reallocates past the exact capacity `to_hex` asked for, and the
    // freed buffer holding the whole key is not what `Zeroizing` wipes.
    file.write_all(hex.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .and_then(|()| file.sync_all())
        .map_err(|source| KeyError::WriteFile {
            path: path.to_path_buf(),
            source,
        })?;

    Ok(key)
}

/// Create clippo's data directory at `0700`, and its parents at their default.
///
/// Only clippo's own directory gets the narrow mode. Creating `~/.local` and
/// `~/.local/share` at `0700` on a machine that happens not to have them yet
/// would be clippo reaching well outside its own business.
///
/// A directory that is **already there** is checked and narrowed rather than
/// accepted as it stands: the mode a `DirBuilder` asks for only applies to a
/// directory it actually creates, so without this the `0700` above holds on
/// first run and never again. A pre-existing `0755` would leave the encrypted
/// database world-readable — the contents are ciphertext, but its size and the
/// times it changes are not, and both are readable by any local account.
pub(crate) fn create_data_dir(dir: &Path) -> Result<(), KeyError> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|source| KeyError::CreateDataDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    match DirBuilder::new().mode(DATA_DIR_MODE).create(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => narrow_data_dir(dir),
        Err(source) => Err(KeyError::CreateDataDir {
            path: dir.to_path_buf(),
            source,
        }),
    }
}

/// Bring an existing data directory to `0700`, or fail naming it.
///
/// The same shape of check [`read_key_file`] makes on the key file, with one
/// deliberate difference: a wider-than-`0600` key file is *refused*, because by
/// then the secret has already been readable and clippo cannot un-leak it,
/// whereas a wide directory is fixed. Narrowing it is both sufficient and the
/// only thing a user could do by hand anyway, and refusing to start over a
/// mode clippo can correct itself would be an outage in the name of a warning.
///
/// Narrower than `0700` — `0500`, say — is left alone: the check is for bits
/// that let *other* accounts in, not for an exact match.
fn narrow_data_dir(dir: &Path) -> Result<(), KeyError> {
    narrow_data_dir_with(dir, |dir| {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(DATA_DIR_MODE))
    })
}

/// [`narrow_data_dir`], with the `chmod` passed in.
///
/// Split out only so the two refusals below are testable. Neither can be
/// provoked through the real `chmod` in a unit test — the owner of a directory
/// can always chmod it, and a filesystem that ignores the call is not something
/// a test can mount — and a refusal that has never been executed is a refusal
/// nobody has checked.
fn narrow_data_dir_with(
    dir: &Path,
    chmod: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), KeyError> {
    let mode = data_dir_mode(dir)?;
    if mode & FORBIDDEN_MODE_BITS == 0 {
        return Ok(());
    }

    chmod(dir).map_err(|source| KeyError::DataDirPermissions {
        path: dir.to_path_buf(),
        mode,
        source,
    })?;

    // Re-read rather than trust the call: `chmod` is allowed to succeed and do
    // nothing on filesystems that do not carry unix modes, and a data
    // directory on one of those really is open to every local account. This is
    // the "cannot be brought to 0700" case that has to be an error rather than
    // a silent continue — the directory is not what the check above assumed.
    let now = data_dir_mode(dir)?;
    if now & FORBIDDEN_MODE_BITS != 0 {
        return Err(KeyError::DataDirPermissions {
            path: dir.to_path_buf(),
            mode: now,
            source: std::io::Error::other(
                "chmod reported success but the mode did not change; the filesystem may not \
                 support unix permissions",
            ),
        });
    }

    tracing::warn!(
        path = %dir.display(),
        was = format!("{mode:04o}"),
        "clippo's data directory was readable by other accounts; narrowed it to 0700"
    );
    Ok(())
}

/// The permission bits of an existing data directory, masked to `0o777`.
fn data_dir_mode(dir: &Path) -> Result<u32, KeyError> {
    let metadata = std::fs::metadata(dir).map_err(|source| KeyError::CreateDataDir {
        path: dir.to_path_buf(),
        source,
    })?;

    // `create` reports `AlreadyExists` for a *file* in the directory's place
    // too, and chmodding that to 0700 would be pointless: nothing can be
    // written inside it. Report it as the creation failure it is.
    if !metadata.is_dir() {
        return Err(KeyError::CreateDataDir {
            path: dir.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "exists but is not a directory",
            ),
        });
    }

    Ok(metadata.permissions().mode() & 0o777)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_key_is_thirty_two_bytes_of_randomness_not_a_derivation() {
        let first = Key::random().unwrap();
        let second = Key::random().unwrap();
        assert_eq!(first.0.len(), 32);
        assert_ne!(
            first.0, second.0,
            "two keys from the CSPRNG must not be the same value"
        );
        assert_ne!(first.0, [0_u8; KEY_BYTES]);
    }

    #[test]
    fn a_key_round_trips_through_its_hex_form() {
        let key = Key::random().unwrap();
        let hex = key.to_hex();
        assert_eq!(hex.len(), 64);
        assert!(hex
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
        assert_eq!(Key::from_hex(&hex).unwrap().0, key.0);
        // Trailing newline, which is how the file stores it.
        assert_eq!(
            Key::from_hex(&format!("{}\n", hex.as_str())).unwrap().0,
            key.0
        );
    }

    #[test]
    fn anything_that_is_not_sixty_four_hex_digits_is_not_a_key() {
        assert!(Key::from_hex("").is_none());
        assert!(Key::from_hex(&"a".repeat(63)).is_none());
        assert!(Key::from_hex(&"a".repeat(65)).is_none());
        assert!(Key::from_hex(&"z".repeat(64)).is_none());
    }

    #[test]
    fn the_pragma_uses_sqlciphers_raw_key_form() {
        let key = Key::from_bytes([0xab; KEY_BYTES]);
        assert_eq!(
            key.pragma().as_str(),
            format!("PRAGMA key = \"x'{}'\";", "ab".repeat(32))
        );
    }

    #[test]
    fn a_key_never_prints_itself() {
        let key = Key::from_bytes([0xab; KEY_BYTES]);
        let rendered = format!("{key:?}");
        assert!(!rendered.contains("ab"), "{rendered}");
        assert_eq!(rendered, "Key(<redacted>)");
    }

    #[test]
    fn the_fallback_file_is_created_at_0600_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(KEY_FILE_NAME);

        let created = create_key_file(&path).unwrap();
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(mode_of(path.parent().unwrap()), 0o700);

        let read_back = read_key_file(&path).unwrap().unwrap();
        assert_eq!(read_back.0, created.0);
    }

    #[test]
    #[cfg(unix)]
    fn a_data_directory_clippo_creates_is_0700() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("share").join("clippo");

        create_data_dir(&data).unwrap();

        assert_eq!(mode_of(&data), 0o700);
    }

    #[test]
    #[cfg(unix)]
    fn a_data_directory_that_was_already_there_is_narrowed_not_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("clippo");

        // Every mode that lets some other account in, including the 0755 a
        // `mkdir -p` at the default umask leaves behind.
        for mode in [0o755, 0o750, 0o705, 0o777, 0o701] {
            std::fs::create_dir_all(&data).unwrap();
            std::fs::set_permissions(&data, std::fs::Permissions::from_mode(mode)).unwrap();

            create_data_dir(&data).unwrap();

            assert_eq!(mode_of(&data), 0o700, "starting from {mode:o}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_narrower_data_directory_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("clippo");
        std::fs::create_dir(&data).unwrap();

        // 0500 lets nobody else in, which is all this check is about. Widening
        // it to 0700 would be clippo overruling a deliberate choice.
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o500)).unwrap();
        create_data_dir(&data).unwrap();
        assert_eq!(mode_of(&data), 0o500);
    }

    #[test]
    #[cfg(unix)]
    fn a_data_directory_that_cannot_be_narrowed_is_a_named_error() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("clippo");
        std::fs::create_dir(&data).unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();

        // A chmod that fails outright — someone else's directory.
        let refused = narrow_data_dir_with(&data, |_| {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        })
        .unwrap_err();
        assert!(
            matches!(refused, KeyError::DataDirPermissions { mode: 0o755, .. }),
            "{refused:?}"
        );

        // And a chmod that reports success and changes nothing — a filesystem
        // that does not carry unix modes. Trusting the return value here would
        // be a silent continue over a world-readable directory, which is the
        // thing this check exists to stop.
        let ignored = narrow_data_dir_with(&data, |_| Ok(())).unwrap_err();
        assert!(
            matches!(ignored, KeyError::DataDirPermissions { mode: 0o755, .. }),
            "{ignored:?}"
        );

        let message = ignored.to_string();
        assert!(message.contains("0755"), "{message}");
        assert!(message.contains("chmod 700"), "{message}");
        assert!(message.contains(&data.display().to_string()), "{message}");

        // The real chmod does move it, so the refusals above are reserved for
        // the cases where it does not.
        create_data_dir(&data).unwrap();
        assert_eq!(mode_of(&data), 0o700);
    }

    #[test]
    #[cfg(unix)]
    fn a_file_where_the_data_directory_should_be_is_reported_not_chmodded() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("clippo");
        std::fs::write(&data, "not a directory").unwrap();
        std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o644)).unwrap();

        let error = create_data_dir(&data).unwrap_err();

        assert!(matches!(error, KeyError::CreateDataDir { .. }), "{error:?}");
        // Left as it was found: chmodding a stranger's file to 0700 would not
        // make it a directory clippo can write a database into.
        assert!(data.is_file());
        assert_eq!(mode_of(&data), 0o644);
    }

    #[test]
    fn no_file_is_not_an_error_it_is_just_no_key() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_key_file(&dir.path().join(KEY_FILE_NAME))
            .unwrap()
            .is_none());
    }

    #[test]
    fn a_key_file_wider_than_0600_is_refused_rather_than_used() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE_NAME);
        create_key_file(&path).unwrap();

        for mode in [0o604, 0o640, 0o644, 0o660, 0o666, 0o601] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let error = read_key_file(&path).unwrap_err();
            let message = error.to_string();
            assert!(
                matches!(error, KeyError::FilePermissions { .. }),
                "{mode:o}"
            );
            assert!(message.contains(&format!("{mode:04o}")), "{message}");
            assert!(message.contains("chmod 600"), "{message}");
        }

        // Narrower than 0600 is not "wider than 0600" — it is fine. A file the
        // owner cannot read is a different problem, and not this check's.
        for mode in [0o600, 0o400] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert!(read_key_file(&path).is_ok(), "{mode:o}");
        }
    }

    #[test]
    fn a_file_that_is_not_a_key_is_an_error_not_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE_NAME);
        std::fs::write(&path, "hunter2\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let error = read_key_file(&path).unwrap_err();
        assert!(matches!(error, KeyError::MalformedFile { .. }), "{error:?}");
        assert!(error.to_string().contains("64 hex digits"), "{error}");
    }

    #[test]
    fn creating_a_key_file_never_overwrites_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE_NAME);
        let first = create_key_file(&path).unwrap();

        let error = create_key_file(&path).unwrap_err();
        assert!(matches!(error, KeyError::WriteFile { .. }), "{error:?}");
        assert_eq!(read_key_file(&path).unwrap().unwrap().0, first.0);
    }

    #[tokio::test]
    async fn an_existing_key_file_wins_over_the_secret_service() {
        // The rule that keeps a database openable once the fallback has been
        // taken. Exercised without a Secret Service in the loop at all: if the
        // file were not consulted first, this would either reach D-Bus or
        // create a second, different key.
        let dir = tempfile::tempdir().unwrap();
        let expected = create_key_file(&dir.path().join(KEY_FILE_NAME)).unwrap();

        let (key, source) = acquire_in(dir.path()).await.unwrap();
        assert_eq!(key.0, expected.0);
        assert_eq!(source, KeySource::File(dir.path().join(KEY_FILE_NAME)));
        assert!(source.to_string().contains("unencrypted"));
    }

    #[test]
    fn no_key_file_is_minted_underneath_a_database_it_could_not_open() {
        // The trap rule 4 exists for: the keyring is transiently down for one
        // start, and without this the file key written here would win over the
        // (working) keyring key on every start after it, forever.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE_NAME);
        let database = dir.path().join(paths::DB_FILE_NAME);
        std::fs::write(&database, b"pretend ciphertext").unwrap();

        let error = fall_back_to_file(&path, &database, "no D-Bus session bus").unwrap_err();
        assert!(
            matches!(error, KeyError::NoSecretServiceForExistingDatabase { .. }),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(message.contains("no D-Bus session bus"), "{message}");
        assert!(message.contains("org.freedesktop.secrets"), "{message}");
        assert!(!path.exists(), "the key file must not have been created");
    }

    #[test]
    fn a_first_run_with_no_keyring_still_gets_its_file_key() {
        // The other half of rule 4: a machine that genuinely has no Secret
        // Service is not blocked, because there is no history to lose yet.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE_NAME);

        let key =
            fall_back_to_file(&path, &dir.path().join(paths::DB_FILE_NAME), "no keyring").unwrap();
        assert_eq!(mode_of(&path), 0o600);
        assert_eq!(read_key_file(&path).unwrap().unwrap().0, key.0);
    }

    #[tokio::test]
    async fn a_refused_key_file_stops_startup_rather_than_being_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE_NAME);
        create_key_file(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let error = acquire_in(dir.path()).await.unwrap_err();
        assert!(
            matches!(error, KeyError::FilePermissions { .. }),
            "{error:?}"
        );
    }
}
