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
//!    Service cannot be reached — with a `WARN` naming the file and saying the
//!    key is on disk unencrypted. Silent fallback is the failure worth avoiding
//!    here: a user whose key quietly moved to a plain file should be able to
//!    find that out from the daemon's log.
//!
//! An existing key file whose mode lets anyone but its owner read it is
//! **refused**, not used. Wider-than-`0600` on a file that is the only thing
//! standing between another local account and the clipboard history is a signal
//! that something went wrong, and carrying on would encrypt the next copy under
//! a key that account can already read.
//!
//! # Getting back to the keyring
//!
//! Because rule 1 is absolute, there is no automatic migration from the file
//! back to the Secret Service. Deleting both `key` and `history.db` starts over
//! with a keyring-held key; deleting only `key` leaves a database nothing can
//! decrypt. That is deliberate — clippo will not quietly discard history it can
//! still read.

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

/// Mode bits that must not be set on the fallback key file: anything that gives
/// group or other any access at all.
const FORBIDDEN_MODE_BITS: u32 = 0o077;

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
    pub(crate) fn pragma(&self) -> Zeroizing<String> {
        let hex = self.to_hex();
        Zeroizing::new(format!("PRAGMA key = \"x'{}'\";", hex.as_str()))
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
/// The three-step rule is in the module docs. In short: an existing key file
/// wins, then the Secret Service, then a new key file with a warning.
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
            tracing::warn!(
                error = %problem,
                key_file = %path.display(),
                "no Secret Service could be reached, so clippo is storing its database key \
                 unencrypted in a file with mode 0600; anyone who can read that file can read \
                 the whole clipboard history. If a history database already exists and was \
                 encrypted with a key from the Secret Service, it will not open with this one"
            );
            let key = create_key_file(&path)?;
            Ok((key, KeySource::File(path)))
        }
    }
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
    let mut contents = key.to_hex();
    contents.push('\n');

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
    file.write_all(contents.as_bytes())
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
pub(crate) fn create_data_dir(dir: &Path) -> Result<(), KeyError> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent).map_err(|source| KeyError::CreateDataDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(source) => Err(KeyError::CreateDataDir {
            path: dir.to_path_buf(),
            source,
        }),
    }
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
