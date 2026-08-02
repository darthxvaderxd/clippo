//! Encrypted clipboard history: SQLCipher-backed entries and flavor blobs, with
//! dedup.
//!
//! This crate is the security floor of the whole project. Everything clippo
//! captures ends up here, passwords included, so "encrypted at rest" has to be
//! true of the file on disk and not merely of the fields someone remembered to
//! wrap. Three decisions carry that:
//!
//! - **Whole-database encryption.** `rusqlite` with
//!   `bundled-sqlcipher-vendored-openssl`, keyed with `PRAGMA key` before the
//!   first statement runs. No column in this crate is encrypted by hand,
//!   because a per-column scheme is a list of places to forget one. The
//!   vendored build makes the first compile slow; DESIGN.md's risk table
//!   accepts that, and the alternative is worse.
//! - **A random key, kept out of the file's reach.** 32 bytes from the
//!   operating system's CSPRNG, in the Secret Service, falling back to a
//!   `0600` file *with a warning*. See [`key`] — that module is where the
//!   ordering, the refusals and the reasons live.
//! - **Dedup on a documented canonical flavor.** BLAKE3 over one chosen flavor
//!   per entry, enforced by `entries.hash UNIQUE` rather than by a check the
//!   caller is trusted to have made. See [`dedup`].
//!
//! # Using it
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use clippo_core::{EntryKind, Flavor, NewEntry, Timestamp};
//! use clippo_store::{dedup, key, Store};
//!
//! let (key, source) = key::acquire().await?;
//! tracing::info!("clippo's database key came from {source}");
//! let mut store = Store::open_default(&key)?;
//!
//! let flavors = vec![Flavor::new("text/plain;charset=utf-8", "hello")];
//! let kind = EntryKind::for_flavors(&flavors).expect("a text flavor implies a kind");
//! let hash = dedup::hash(kind, &flavors).expect("a text flavor is canonical");
//!
//! let insertion = store.insert(&NewEntry {
//!     created_at: Timestamp::now(),
//!     kind,
//!     preview: "hello".to_owned(),
//!     hash,
//!     sensitive: false,
//!     flavors,
//! })?;
//! println!("entry {} (repeat: {})", insertion.id(), insertion.was_deduplicated());
//! # Ok(())
//! # }
//! ```
//!
//! Retention, image thumbnails and search are not here yet; they arrive with
//! the rest of M2 and M3. See `docs/ROADMAP.md`.

pub mod dedup;
pub mod key;
mod schema;
mod store;

use std::path::PathBuf;

pub use key::{Key, KeyError, KeySource};
pub use schema::SCHEMA_VERSION;
pub use store::{Insertion, Store, StoredEntry};

/// Why the history database could not be opened, read or written.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The file could not be opened at all.
    #[error("could not open the clippo history database at {path}")]
    Open {
        /// The database file.
        path: PathBuf,
        /// Why it failed.
        #[source]
        source: rusqlite::Error,
    },

    /// The file is not decryptable with the key clippo just used.
    ///
    /// From SQLCipher's point of view a wrong key and a file that is not a
    /// database are the same thing — both are pages that will not decrypt — so
    /// this message names both possibilities rather than guessing between them.
    #[error(
        "{path} could not be decrypted: either it is not a clippo history database, or the key \
         clippo has is not the one it was encrypted with. If clippo has just fallen back to a \
         key file because the Secret Service was unreachable, that is the likely cause"
    )]
    WrongKey {
        /// The database file.
        path: PathBuf,
    },

    /// The database was written by a newer clippo.
    #[error(
        "the clippo history database at {path} is at schema version {found}, but this build \
         understands version {supported}; it was written by a newer version of clippo. Upgrade \
         clippo rather than letting this build write to a schema it does not know"
    )]
    SchemaTooNew {
        /// The database file.
        path: PathBuf,
        /// The version stamped in the file.
        found: i64,
        /// The version this build writes.
        supported: i64,
    },

    /// The database is at a version this build has no migration from.
    #[error(
        "the clippo history database at {path} is at schema version {found}, and this build \
         (schema version {supported}) has no migration from it"
    )]
    SchemaTooOld {
        /// The database file.
        path: PathBuf,
        /// The version stamped in the file.
        found: i64,
        /// The version this build writes.
        supported: i64,
    },

    /// The database has tables but no schema version.
    #[error(
        "the clippo history database at {path} has tables but no schema version, so clippo \
         cannot tell what shape they are in; it was not written by clippo, or was left behind \
         by an interrupted first run"
    )]
    SchemaUnversioned {
        /// The database file.
        path: PathBuf,
    },

    /// The key could not be obtained.
    #[error(transparent)]
    Key(#[from] KeyError),

    /// There is nowhere to put the database.
    #[error(transparent)]
    Path(#[from] clippo_core::PathError),

    /// Any other SQLite failure.
    #[error("the clippo history database could not be read or written")]
    Sqlite(#[from] rusqlite::Error),
}

#[cfg(test)]
pub(crate) mod testing {
    //! A temp database per test.
    //!
    //! Every test gets its own directory and its own random key; nothing is
    //! shared, so tests neither see each other's rows nor race on a file, and
    //! the directory goes away when the fixture drops.

    use std::path::PathBuf;

    use clippo_core::paths;
    use tempfile::TempDir;

    use crate::{Key, Store, StoreError};

    pub(crate) struct Temp {
        /// Removed on drop, so the fixture has to outlive every store it opens.
        pub dir: TempDir,
        pub key: Key,
    }

    impl Temp {
        pub(crate) fn new() -> Self {
            Self {
                dir: tempfile::tempdir().expect("a temp dir for the test database"),
                key: Key::random().expect("a key for the test database"),
            }
        }

        /// The database file inside this fixture's directory.
        pub(crate) fn db(&self) -> PathBuf {
            self.dir.path().join(paths::DB_FILE_NAME)
        }

        /// Open the database, creating it on the first call.
        pub(crate) fn open(&self) -> Store {
            self.try_open().expect("the test database should open")
        }

        /// Open the database, keeping the failure instead of panicking.
        pub(crate) fn try_open(&self) -> Result<Store, StoreError> {
            Store::open(self.db(), &self.key)
        }
    }
}
