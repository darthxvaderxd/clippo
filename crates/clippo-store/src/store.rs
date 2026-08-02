//! The encrypted history database itself.

use std::path::{Path, PathBuf};

use clippo_core::{Entry, EntryId, EntryKind, Flavor, NewEntry, Timestamp};
use rusqlite::{params, Connection, OpenFlags};

use crate::key::Key;
use crate::{schema, StoreError};

/// One stored copy with every flavor it was captured with.
///
/// The list view does not need the blobs — that is [`Entry`] on its own — so
/// they are only loaded by [`Store::get`], which is what the copy-back path and
/// `Reveal` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEntry {
    /// The `entries` row.
    pub entry: Entry,
    /// Every `flavors` row belonging to it, in the order it was captured.
    pub flavors: Vec<Flavor>,
}

/// What a [`Store::insert`] turned out to be.
///
/// A repeat copy is not an error and not a no-op: it moves an entry that is
/// already in the history back to the front. Callers need to tell the two apart
/// — the daemon emits `HistoryChanged` either way, but only a `Created` is a
/// new row for the applet to animate in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Insertion {
    /// A copy clippo had not seen: a new row, with its flavors.
    Created(EntryId),
    /// A copy clippo already had: `last_used_at` bumped, no second row.
    Bumped(EntryId),
}

impl Insertion {
    /// The entry that now holds this copy, however it got there.
    pub const fn id(self) -> EntryId {
        match self {
            Self::Created(id) | Self::Bumped(id) => id,
        }
    }

    /// Whether this copy was already in the history.
    pub const fn was_deduplicated(self) -> bool {
        matches!(self, Self::Bumped(_))
    }
}

/// The columns of `entries`, in the order [`entry_from_row`] reads them.
const ENTRY_COLUMNS: &str = "id, created_at, last_used_at, kind, preview, hash, pinned, sensitive";

/// An open, keyed connection to the encrypted history database.
///
/// Encryption is whole-database and happens below the query layer: every method
/// here writes ordinary SQL, and nothing in this crate encrypts a column by
/// hand. The one crypto-aware line in the whole store is the `PRAGMA key` in
/// [`Store::open`], which runs before any statement that touches the file.
pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    /// Open the history database at clippo's usual location.
    ///
    /// Creates `~/.local/share/clippo` at mode `0700` if it is not there yet,
    /// then opens `history.db` inside it.
    pub fn open_default(key: &Key) -> Result<Self, StoreError> {
        let dir = clippo_core::paths::data_dir()?;
        crate::key::create_data_dir(&dir)?;
        Self::open(dir.join(clippo_core::paths::DB_FILE_NAME), key)
    }

    /// Open — or create — the encrypted history database at `path`.
    ///
    /// The order here is the security-critical part. `PRAGMA key` is the first
    /// statement on the connection, so SQLCipher has the key before anything
    /// reads or writes a page; a fresh file is therefore encrypted from its
    /// first byte, and there is no window in which plaintext is written and
    /// re-encrypted later.
    ///
    /// The read straight afterwards is the key check. SQLCipher accepts any
    /// `PRAGMA key` without complaint — it cannot know the key is wrong until
    /// it tries to decrypt a page — so the first query is what turns a wrong
    /// key into [`StoreError::WrongKey`] rather than an incomprehensible
    /// failure several calls later.
    pub fn open(path: impl AsRef<Path>, key: &Key) -> Result<Self, StoreError> {
        let path = path.as_ref().to_path_buf();
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|source| StoreError::Open {
            path: path.clone(),
            source,
        })?;

        conn.execute_batch(&key.pragma())
            .map_err(|source| StoreError::Open {
                path: path.clone(),
                source,
            })?;

        if let Err(source) = conn.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
            row.get::<_, i64>(0)
        }) {
            return Err(if is_not_a_database(&source) {
                StoreError::WrongKey { path }
            } else {
                StoreError::Open { path, source }
            });
        }

        // SQLite defaults this off, per connection. The cascade from `entries`
        // to `flavors` does nothing without it, which would leave clipboard
        // blobs behind after a delete.
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;

        schema::ensure(&conn, &path)?;
        Ok(Self { conn, path })
    }

    /// The file this store is backed by.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The underlying connection, for the schema tests.
    #[cfg(test)]
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Store a captured selection, or bump the entry it duplicates.
    ///
    /// The whole write — the `entries` row and every one of its `flavors` rows
    /// — is one transaction, so a failure part way through leaves no entry
    /// behind with half its flavors. An entry with a missing flavor would paste
    /// as the wrong thing, silently, which is worse than not being stored.
    ///
    /// Dedup is the `hash UNIQUE` constraint doing the work: the insert is
    /// simply attempted, and a uniqueness violation is what says "already have
    /// this". There is no `SELECT` first — with one, two writers racing on the
    /// same copy could both find nothing and both insert. The bump sets
    /// `last_used_at` to the repeat copy's `created_at`, which is what moves it
    /// back to the front of [`Store::list`].
    pub fn insert(&mut self, new: &NewEntry) -> Result<Insertion, StoreError> {
        let flavors = distinct_flavors(&new.flavors);
        let captured_at = new.created_at.as_unix_millis();

        let tx = self.conn.transaction()?;
        let insertion = match tx.execute(
            "INSERT INTO entries (created_at, last_used_at, kind, preview, hash, pinned, sensitive)
             VALUES (?1, ?1, ?2, ?3, ?4, 0, ?5)",
            params![
                captured_at,
                new.kind.as_str(),
                new.preview,
                new.hash,
                new.sensitive,
            ],
        ) {
            Ok(_) => {
                let id = EntryId::new(tx.last_insert_rowid());
                {
                    let mut statement = tx.prepare(
                        "INSERT INTO flavors (entry_id, mime, data) VALUES (?1, ?2, ?3)",
                    )?;
                    for flavor in &flavors {
                        statement.execute(params![id.get(), flavor.mime, flavor.data])?;
                    }
                }
                Insertion::Created(id)
            }
            // A failed statement rolls back only itself, so the transaction is
            // still live here and the bump goes in the same one.
            Err(error) if is_unique_violation(&error) => {
                let id: i64 = tx.query_row(
                    "SELECT id FROM entries WHERE hash = ?1",
                    [&new.hash],
                    |row| row.get(0),
                )?;
                tx.execute(
                    "UPDATE entries SET last_used_at = ?1 WHERE id = ?2",
                    params![captured_at, id],
                )?;
                Insertion::Bumped(EntryId::new(id))
            }
            Err(error) => return Err(error.into()),
        };
        tx.commit()?;

        Ok(insertion)
    }

    /// The history, newest use first.
    ///
    /// Ordered by `last_used_at` descending, so a re-copied entry comes back to
    /// the front; ties break on `id` descending, so two copies made in the same
    /// millisecond still have a stable order.
    pub fn list(&self, limit: usize, offset: usize) -> Result<Vec<Entry>, StoreError> {
        let mut statement = self.conn.prepare(&format!(
            "SELECT {ENTRY_COLUMNS} FROM entries
             ORDER BY last_used_at DESC, id DESC
             LIMIT ?1 OFFSET ?2"
        ))?;
        let entries = statement
            .query_map(params![as_i64(limit), as_i64(offset)], entry_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(entries)
    }

    /// How many entries the history holds.
    pub fn count(&self) -> Result<usize, StoreError> {
        let count: i64 = self
            .conn
            .query_row("SELECT count(*) FROM entries", [], |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// One entry with all of its flavors, or `None` if there is no such id.
    pub fn get(&self, id: EntryId) -> Result<Option<StoredEntry>, StoreError> {
        let entry = match self.conn.query_row(
            &format!("SELECT {ENTRY_COLUMNS} FROM entries WHERE id = ?1"),
            [id.get()],
            entry_from_row,
        ) {
            Ok(entry) => entry,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(error) => return Err(error.into()),
        };

        let mut statement = self
            .conn
            .prepare("SELECT mime, data FROM flavors WHERE entry_id = ?1 ORDER BY rowid")?;
        let flavors = statement
            .query_map([id.get()], |row| {
                Ok(Flavor {
                    mime: row.get(0)?,
                    data: row.get(1)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Some(StoredEntry { entry, flavors }))
    }

    /// Delete an entry and, through the foreign key's cascade, its flavors.
    ///
    /// Returns whether there was anything to delete.
    pub fn delete(&mut self, id: EntryId) -> Result<bool, StoreError> {
        let deleted = self
            .conn
            .execute("DELETE FROM entries WHERE id = ?1", [id.get()])?;
        Ok(deleted > 0)
    }

    /// Pin or unpin an entry. Returns whether there was such an entry.
    pub fn set_pinned(&mut self, id: EntryId, pinned: bool) -> Result<bool, StoreError> {
        let updated = self.conn.execute(
            "UPDATE entries SET pinned = ?1 WHERE id = ?2",
            params![pinned, id.get()],
        )?;
        Ok(updated > 0)
    }

    /// Move an entry to the front of the history without re-inserting it.
    ///
    /// What `Copy(id)` does: pasting something out of the history is a use of
    /// it, and the self-echo guard means the copy-back never comes round again
    /// as a fresh selection to be deduplicated.
    pub fn touch(&mut self, id: EntryId, at: Timestamp) -> Result<bool, StoreError> {
        let updated = self.conn.execute(
            "UPDATE entries SET last_used_at = ?1 WHERE id = ?2",
            params![at.as_unix_millis(), id.get()],
        )?;
        Ok(updated > 0)
    }
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("path", &self.path).finish()
    }
}

/// Map one `entries` row, in [`ENTRY_COLUMNS`] order.
fn entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    let kind: String = row.get(3)?;
    let kind = kind.parse::<EntryKind>().map_err(|problem| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(problem))
    })?;

    Ok(Entry {
        id: EntryId::new(row.get(0)?),
        created_at: Timestamp::from_unix_millis(row.get(1)?),
        last_used_at: Timestamp::from_unix_millis(row.get(2)?),
        kind,
        preview: row.get(4)?,
        hash: row.get(5)?,
        pinned: row.get(6)?,
        sensitive: row.get(7)?,
    })
}

/// The captured flavors with any repeated MIME type dropped, keeping the first.
///
/// `flavors` is keyed on `(entry_id, mime)`, so a selection that advertised the
/// same type twice would otherwise abort the whole insert and lose an entry
/// that is perfectly storable. Dropping the duplicate is the smaller harm, but
/// it is not silent.
fn distinct_flavors(flavors: &[Flavor]) -> Vec<&Flavor> {
    let mut kept: Vec<&Flavor> = Vec::with_capacity(flavors.len());
    for flavor in flavors {
        if kept.iter().any(|seen| seen.mime == flavor.mime) {
            tracing::warn!(
                mime = %flavor.mime,
                "the selection advertised the same flavor twice; storing only the first"
            );
            continue;
        }
        kept.push(flavor);
    }
    kept
}

/// A count from the caller as SQLite's `INTEGER`, saturating rather than
/// wrapping a `usize` that will not fit.
fn as_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Whether this error is the `entries.hash UNIQUE` constraint firing.
fn is_unique_violation(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
    )
}

/// Whether this error is SQLCipher saying it could not make sense of the file
/// — which, for a file clippo itself wrote, means the key was wrong.
fn is_not_a_database(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(failure, _)
            if failure.code == rusqlite::ErrorCode::NotADatabase
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Temp;
    use crate::{dedup, Key, StoreError};

    /// A selection, hashed by the same rule the daemon will use.
    fn selection(at: i64, flavors: Vec<Flavor>) -> NewEntry {
        let kind = EntryKind::for_flavors(&flavors).expect("the test flavors imply a kind");
        NewEntry {
            created_at: Timestamp::from_unix_millis(at),
            kind,
            preview: flavors[0].as_str().unwrap_or("<binary>").to_owned(),
            hash: dedup::hash(kind, &flavors).expect("the test flavors have a canonical one"),
            sensitive: false,
            flavors,
        }
    }

    fn text(at: i64, body: &str) -> NewEntry {
        selection(at, vec![Flavor::new("text/plain;charset=utf-8", body)])
    }

    #[test]
    fn an_entry_goes_in_and_comes_back_out_whole() {
        let temp = Temp::new();
        let mut store = temp.open();

        let mut new = text(1_000, "hello");
        new.sensitive = true;
        let insertion = store.insert(&new).unwrap();
        assert!(matches!(insertion, Insertion::Created(_)));
        assert!(!insertion.was_deduplicated());

        let stored = store.get(insertion.id()).unwrap().unwrap();
        assert_eq!(stored.entry.id, insertion.id());
        assert_eq!(stored.entry.created_at, Timestamp::from_unix_millis(1_000));
        assert_eq!(
            stored.entry.last_used_at,
            Timestamp::from_unix_millis(1_000)
        );
        assert_eq!(stored.entry.kind, EntryKind::Text);
        assert_eq!(stored.entry.preview, "hello");
        assert_eq!(stored.entry.hash, new.hash);
        assert!(!stored.entry.pinned);
        assert!(stored.entry.sensitive);
        assert_eq!(stored.flavors, new.flavors);
    }

    #[test]
    fn every_flavor_of_one_copy_round_trips_in_the_order_it_was_captured() {
        let temp = Temp::new();
        let mut store = temp.open();

        // A browser copy: rich text, its plain-text fallback, and an image.
        let flavors = vec![
            Flavor::new("text/html", "<b>hi</b>"),
            Flavor::new("text/plain;charset=utf-8", "hi"),
            Flavor::new("image/png", [0x89, b'P', b'N', b'G', 0x00, 0xff]),
        ];
        let insertion = store.insert(&selection(2_000, flavors.clone())).unwrap();

        let stored = store.get(insertion.id()).unwrap().unwrap();
        assert_eq!(
            stored.entry.kind,
            EntryKind::Image,
            "the richest flavor wins"
        );
        assert_eq!(stored.flavors, flavors);
        // Binary survives as binary rather than as lossy text.
        assert_eq!(
            stored.flavors[2].data,
            vec![0x89, b'P', b'N', b'G', 0x00, 0xff]
        );
    }

    #[test]
    fn a_repeat_copy_bumps_the_timestamp_instead_of_adding_a_row() {
        let temp = Temp::new();
        let mut store = temp.open();

        let first = store.insert(&text(1_000, "hello")).unwrap();
        let again = store.insert(&text(5_000, "hello")).unwrap();

        assert_eq!(again, Insertion::Bumped(first.id()));
        assert!(again.was_deduplicated());
        assert_eq!(store.count().unwrap(), 1);

        let stored = store.get(first.id()).unwrap().unwrap();
        assert_eq!(
            stored.entry.created_at,
            Timestamp::from_unix_millis(1_000),
            "the first sighting is when it was created"
        );
        assert_eq!(
            stored.entry.last_used_at,
            Timestamp::from_unix_millis(5_000),
            "the repeat is when it was last used"
        );
        assert_eq!(stored.flavors.len(), 1, "no second set of flavors");
    }

    #[test]
    fn a_repeat_copy_comes_back_to_the_front_of_the_history() {
        // ROADMAP.md verification 2: copy the same text twice, still one entry,
        // with a bumped timestamp — and it is the one the applet shows first.
        let temp = Temp::new();
        let mut store = temp.open();

        let first = store.insert(&text(1_000, "one")).unwrap().id();
        let second = store.insert(&text(2_000, "two")).unwrap().id();
        assert_eq!(ids(&store), vec![second, first]);

        store.insert(&text(3_000, "one")).unwrap();
        assert_eq!(ids(&store), vec![first, second]);
        assert_eq!(store.count().unwrap(), 2);
    }

    #[test]
    fn dedup_is_the_unique_constraint_and_not_a_pre_check() {
        // The pre-`SELECT` is an optimization we do not have; this is the
        // guarantee. A row inserted behind the store's back — as a second
        // writer would — still cannot be duplicated by the next insert.
        let temp = Temp::new();
        let mut store = temp.open();
        let new = text(1_000, "hello");

        store
            .conn
            .execute(
                "INSERT INTO entries (created_at, last_used_at, kind, preview, hash)
                 VALUES (1, 1, 'text', 'hello', ?1)",
                [&new.hash],
            )
            .unwrap();

        let insertion = store.insert(&new).unwrap();
        assert!(insertion.was_deduplicated());
        assert_eq!(store.count().unwrap(), 1);
        assert_eq!(
            store
                .get(insertion.id())
                .unwrap()
                .unwrap()
                .entry
                .last_used_at,
            Timestamp::from_unix_millis(1_000)
        );
    }

    #[test]
    fn two_copies_that_differ_only_in_bytes_are_two_entries() {
        let temp = Temp::new();
        let mut store = temp.open();
        store.insert(&text(1_000, "hello")).unwrap();
        store.insert(&text(2_000, "hellO")).unwrap();
        assert_eq!(store.count().unwrap(), 2);
    }

    #[test]
    fn deleting_an_entry_takes_its_flavors_with_it() {
        let temp = Temp::new();
        let mut store = temp.open();

        let id = store
            .insert(&selection(
                1_000,
                vec![
                    Flavor::new("text/html", "<b>hi</b>"),
                    Flavor::new("text/plain", "hi"),
                ],
            ))
            .unwrap()
            .id();
        assert_eq!(flavor_rows(&store), 2);

        assert!(store.delete(id).unwrap());
        assert_eq!(store.count().unwrap(), 0);
        assert_eq!(
            flavor_rows(&store),
            0,
            "an orphaned flavor row is a clipboard blob that outlived the delete"
        );
        assert!(store.get(id).unwrap().is_none());

        // Deleting again is false rather than an error.
        assert!(!store.delete(id).unwrap());
    }

    #[test]
    fn pinning_is_set_and_cleared_by_id() {
        let temp = Temp::new();
        let mut store = temp.open();
        let id = store.insert(&text(1_000, "hello")).unwrap().id();

        assert!(store.set_pinned(id, true).unwrap());
        assert!(store.get(id).unwrap().unwrap().entry.pinned);

        assert!(store.set_pinned(id, false).unwrap());
        assert!(!store.get(id).unwrap().unwrap().entry.pinned);

        assert!(!store.set_pinned(EntryId::new(404), true).unwrap());
    }

    #[test]
    fn the_list_pages_newest_use_first() {
        let temp = Temp::new();
        let mut store = temp.open();
        let ids: Vec<EntryId> = (0..5)
            .map(|n| {
                store
                    .insert(&text(1_000 + n, &format!("copy {n}")))
                    .unwrap()
                    .id()
            })
            .collect();

        let newest_first: Vec<EntryId> = ids.iter().rev().copied().collect();
        assert_eq!(ids_with(&store, 10, 0), newest_first);
        assert_eq!(ids_with(&store, 2, 0), newest_first[..2]);
        assert_eq!(ids_with(&store, 2, 2), newest_first[2..4]);
        assert_eq!(ids_with(&store, 10, 5), Vec::<EntryId>::new());
        assert_eq!(ids_with(&store, 0, 0), Vec::<EntryId>::new());
    }

    #[test]
    fn copying_out_of_the_history_moves_an_entry_to_the_front() {
        let temp = Temp::new();
        let mut store = temp.open();
        let first = store.insert(&text(1_000, "one")).unwrap().id();
        let second = store.insert(&text(2_000, "two")).unwrap().id();

        assert!(store
            .touch(first, Timestamp::from_unix_millis(9_000))
            .unwrap());
        assert_eq!(ids(&store), vec![first, second]);
        assert!(!store.touch(EntryId::new(404), Timestamp::now()).unwrap());
    }

    #[test]
    fn an_id_that_is_not_there_is_none_rather_than_an_error() {
        let temp = Temp::new();
        let store = temp.open();
        assert!(store.get(EntryId::new(404)).unwrap().is_none());
    }

    #[test]
    fn a_selection_that_advertised_a_flavor_twice_is_still_stored() {
        let temp = Temp::new();
        let mut store = temp.open();
        let insertion = store
            .insert(&selection(
                1_000,
                vec![
                    Flavor::new("text/plain", "first"),
                    Flavor::new("text/plain", "second"),
                ],
            ))
            .unwrap();

        let stored = store.get(insertion.id()).unwrap().unwrap();
        assert_eq!(stored.flavors, vec![Flavor::new("text/plain", "first")]);
    }

    #[test]
    fn the_history_survives_a_restart() {
        // ROADMAP.md verification 6, as far as a test can take it: the same
        // key opens the same file and finds the same rows.
        let temp = Temp::new();
        let id = {
            let mut store = temp.open();
            store.insert(&text(1_000, "hello")).unwrap().id()
        };

        let store = temp.open();
        assert_eq!(store.count().unwrap(), 1);
        assert_eq!(
            store.get(id).unwrap().unwrap().flavors[0].as_str(),
            Some("hello")
        );
    }

    #[test]
    fn the_wrong_key_is_refused_with_a_message_rather_than_a_puzzle() {
        let temp = Temp::new();
        drop(temp.open());

        let error = Store::open(temp.db(), &Key::random().unwrap()).unwrap_err();
        assert!(matches!(error, StoreError::WrongKey { .. }), "{error:?}");
        let message = error.to_string();
        assert!(message.contains("could not be decrypted"), "{message}");
        assert!(message.contains("key file"), "{message}");
    }

    #[test]
    fn what_was_copied_is_not_in_the_file() {
        // ROADMAP.md verification 3, automated:
        //     strings ~/.local/share/clippo/history.db | grep '<copied string>'
        // must find nothing. This is the criterion the whole crate exists for —
        // if it fails, M4's masking is decoration over a plaintext history.
        const SECRET: &str = "correct-horse-battery-staple-9f3a";
        const HTML: &str = "<b>correct-horse-battery-staple-9f3a</b>";

        let temp = Temp::new();
        {
            let mut store = temp.open();
            let mut new = selection(
                1_000,
                vec![
                    Flavor::new("text/html", HTML),
                    Flavor::new("text/plain;charset=utf-8", SECRET),
                ],
            );
            new.preview = SECRET.to_owned();
            store.insert(&new).unwrap();
            // Readable through the API, which is what makes the absence below
            // evidence of encryption rather than of nothing being written.
            assert_eq!(store.count().unwrap(), 1);
        }

        // Every file the store touched, not just history.db: a journal or a
        // WAL alongside it would hold the same pages.
        let mut checked = 0;
        for entry in std::fs::read_dir(temp.dir.path()).unwrap() {
            let path = entry.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            for needle in [SECRET.as_bytes(), HTML.as_bytes()] {
                assert!(
                    !contains(&bytes, needle),
                    "{} holds clipboard contents in the clear",
                    path.display()
                );
            }
            checked += 1;
        }
        assert!(checked > 0, "the database file should exist");

        // And the second half of that verification step: `sqlite3 … .tables`
        // must fail with "not a database", which it does because the header is
        // encrypted too rather than left as the usual magic string.
        let bytes = std::fs::read(temp.db()).unwrap();
        assert!(!bytes.is_empty());
        assert!(
            !bytes.starts_with(b"SQLite format 3\0"),
            "the file still looks like a plain SQLite database"
        );
    }

    /// The ids in the history, newest use first.
    fn ids(store: &Store) -> Vec<EntryId> {
        ids_with(store, 100, 0)
    }

    fn ids_with(store: &Store, limit: usize, offset: usize) -> Vec<EntryId> {
        store
            .list(limit, offset)
            .unwrap()
            .into_iter()
            .map(|entry| entry.id)
            .collect()
    }

    fn flavor_rows(store: &Store) -> i64 {
        store
            .conn
            .query_row("SELECT count(*) FROM flavors", [], |row| row.get(0))
            .unwrap()
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }
}
