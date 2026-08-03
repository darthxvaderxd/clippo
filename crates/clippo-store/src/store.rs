//! The encrypted history database itself.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use clippo_core::{Config, Entry, EntryId, EntryKind, Flavor, NewEntry, Timestamp};
use rusqlite::{params, Connection, OpenFlags};

use crate::key::Key;
use crate::retention::{self, Retention, Sweep};
use crate::{dedup, images, schema, StoreError};

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
    retention: Retention,
    max_image_bytes: u64,
}

impl Store {
    /// Open the history database at clippo's usual location.
    ///
    /// Creates `~/.local/share/clippo` at mode `0700` if it is not there yet —
    /// and narrows it to `0700` if it is there and wider — then opens
    /// `history.db` inside it.
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
    ///
    /// The `chmod` between the two is `restrict_db_file`; see there for why the
    /// mode cannot be set as part of the open.
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

        restrict_db_file(&path)?;

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

        // Before `schema::ensure`, because a fresh database only takes this
        // from the pragma while it still has no tables. See
        // `convert_to_incremental_vacuum` for the other half.
        conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;

        schema::ensure(&conn, &path)?;

        // After `ensure`, because converting an existing database is a full
        // rewrite and nothing should rewrite a file clippo has just decided it
        // does not understand.
        convert_to_incremental_vacuum(&conn)?;

        Ok(Self {
            conn,
            path,
            retention: Retention::default(),
            max_image_bytes: clippo_core::config::DEFAULT_MAX_IMAGE_BYTES,
        })
    }

    /// The file this store is backed by.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Apply a loaded [`Config`]: the retention limits and the image cap.
    ///
    /// A store that is never configured behaves as `Config::default()` does,
    /// so forgetting this weakens nothing — it only means the user's file was
    /// not read.
    #[must_use]
    pub fn with_config(mut self, config: &Config) -> Self {
        self.set_config(config);
        self
    }

    /// [`Store::with_config`] on an already-opened store.
    pub fn set_config(&mut self, config: &Config) {
        self.retention = Retention::from_config(config);
        self.max_image_bytes = config.max_image_bytes;
    }

    /// Set only the retention limits, leaving the image cap alone.
    #[must_use]
    pub fn with_retention(mut self, retention: Retention) -> Self {
        self.retention = retention;
        self
    }

    /// [`Store::with_retention`] on an already-opened store.
    pub fn set_retention(&mut self, retention: Retention) {
        self.retention = retention;
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
    ///
    /// A bump also ORs `sensitive` in rather than dropping it. The same bytes
    /// can arrive twice by different routes — `hunter2` out of a text editor,
    /// then the same `hunter2` out of a password manager — and only the second
    /// carries the marker M4 reads. The marker is never the canonical flavor, so
    /// the two hash alike and the second arrival is a bump; discarding its
    /// `sensitive` would leave a password the applet renders in full. OR is the
    /// safe direction for a heuristic: an entry that was ever sensitive stays
    /// sensitive, and clearing the flag stays something a user gesture asks for
    /// rather than something a later copy does by accident.
    ///
    /// **The preview travels with the flag.** Since M4 the preview is masked
    /// before it is stored, so it is the thing that decides whether a password
    /// is on screen — raising `sensitive` while leaving the unmasked preview in
    /// place would flag the row correctly and still show the value, which is
    /// worse than not flagging it at all. The bump therefore takes the new
    /// capture's preview whenever that capture is sensitive. `CASE` rather than
    /// an unconditional assignment keeps the two columns moving in the same
    /// direction: a sensitive arrival always overwrites with its mask, and a
    /// *non*-sensitive repeat of an already-masked entry leaves the mask alone,
    /// exactly as `sensitive = sensitive | ?2` leaves the flag alone. This is
    /// also what upgrades a row captured before M4, or captured while
    /// `entropy_rule` was off, the next time the user copies it.
    ///
    /// Three things happen around the write itself:
    ///
    /// - **The image cap.** An image flavor over
    ///   [`Config::max_image_bytes`] is not stored. If it is
    ///   the entry's canonical flavor, the whole insert is refused with
    ///   [`StoreError::ImageTooLarge`] rather than stored short — see
    ///   [`Store::storable_flavors`].
    /// - **The thumbnail.** An image entry gets a PNG thumbnail generated and
    ///   stored beside it as a second flavor. A failure here is logged and the
    ///   entry stored without one.
    /// - **Retention**, in this same transaction, so the history is never
    ///   observably over its limits. See [`crate::retention`] for why here
    ///   rather than on a timer.
    pub fn insert(&mut self, new: &NewEntry) -> Result<Insertion, StoreError> {
        let flavors = self.storable_flavors(new)?;
        // Before the transaction rather than inside it: decoding a screenshot
        // is the slowest thing on this path and a write transaction is the last
        // place to spend that time. The cost is that a repeat copy of the same
        // image thumbnails it again for nothing, which is a rare copy paying
        // for the common one.
        let thumbnail = self.thumbnail_for(new, &flavors);
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
                    for flavor in flavors.iter().copied().chain(thumbnail.iter()) {
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
                    "UPDATE entries
                     SET last_used_at = ?1,
                         preview = CASE WHEN ?2 THEN ?4 ELSE preview END,
                         sensitive = sensitive | ?2
                     WHERE id = ?3",
                    params![captured_at, new.sensitive, id, new.preview],
                )?;
                Insertion::Bumped(EntryId::new(id))
            }
            Err(error) => return Err(error.into()),
        };
        let swept = retention::sweep(&tx, &self.retention, new.created_at)?;
        tx.commit()?;

        if !swept.is_empty() {
            self.reclaim_after("insert");
        }
        Ok(insertion)
    }

    /// The flavors of `new` that this store will actually write.
    ///
    /// [`distinct_flavors`] first, then the image cap. The cap is applied per
    /// flavor, and what happens when one is over it depends on whether the
    /// entry's identity rests on it:
    ///
    /// - **The canonical flavor is over the cap** → the whole insert is
    ///   refused. `entries.hash` is BLAKE3 of that flavor, so storing the entry
    ///   without it would leave a row whose identity refers to bytes that are
    ///   not in the database: it would dedup against future copies of an image
    ///   it cannot produce, and paste as nothing. A truncated blob would be
    ///   worse still — a corrupt image that looks like a successful copy.
    /// - **Any other image flavor is over the cap** → it is dropped and the
    ///   entry stored without it, because the entry is still whole without a
    ///   redundant second encoding of the same picture.
    ///
    /// Both paths log. A copy that silently did not happen is the failure mode
    /// this whole crate is least able to explain after the fact.
    fn storable_flavors<'a>(&self, new: &'a NewEntry) -> Result<Vec<&'a Flavor>, StoreError> {
        let distinct = distinct_flavors(&new.flavors);

        if let Some(canonical) = dedup::canonical_flavor(new.kind, distinct.iter().copied()) {
            if self.is_over_image_cap(canonical) {
                tracing::warn!(
                    mime = %canonical.mime,
                    bytes = canonical.data.len(),
                    cap = self.max_image_bytes,
                    "clippo did not store a copied image: it is over max_image_bytes"
                );
                return Err(StoreError::ImageTooLarge {
                    mime: canonical.mime.clone(),
                    bytes: canonical.data.len() as u64,
                    cap: self.max_image_bytes,
                });
            }
        }

        Ok(distinct
            .into_iter()
            .filter(|flavor| {
                let over = self.is_over_image_cap(flavor);
                if over {
                    tracing::warn!(
                        mime = %flavor.mime,
                        bytes = flavor.data.len(),
                        cap = self.max_image_bytes,
                        "clippo dropped one image flavor of a copy: it is over max_image_bytes. \
                         The entry is stored without it"
                    );
                }
                !over
            })
            .collect())
    }

    /// Whether this flavor is an image bigger than the configured cap.
    ///
    /// Only images are capped. A text flavor is bounded by what a compositor
    /// will hand over and by `clippo-wayland`'s own per-flavor limit; an image
    /// is the one thing routinely large enough to be worth a policy.
    fn is_over_image_cap(&self, flavor: &Flavor) -> bool {
        EntryKind::from_mime(&flavor.mime) == Some(EntryKind::Image)
            && flavor.data.len() as u64 > self.max_image_bytes
    }

    /// The thumbnail flavor to store beside an image entry, if there is one.
    ///
    /// `None` — with a logged reason where there was one — in four cases: the
    /// entry is not an image, the capture already carried a thumbnail, nothing
    /// image-shaped survived the cap, or the image would not decode. DESIGN.md
    /// asks for the entry either way: a list row without a picture is a much
    /// better outcome than a copy that vanished because it was a format this
    /// build of `image` has no decoder for.
    fn thumbnail_for(&self, new: &NewEntry, flavors: &[&Flavor]) -> Option<Flavor> {
        if new.kind != EntryKind::Image {
            return None;
        }
        if flavors
            .iter()
            .any(|flavor| images::is_thumbnail(&flavor.mime))
        {
            return None;
        }

        // The canonical image, so the thumbnail is generated from the same
        // flavor the entry's identity is keyed on — PNG in preference to JPEG.
        let source = dedup::canonical_flavor(EntryKind::Image, flavors.iter().copied())?;
        match images::thumbnail(&source.data) {
            Ok(png) => Some(Flavor::new(images::THUMBNAIL_MIME, png)),
            Err(error) => {
                // `?` rather than `%`: the `Display` of both variants is a fixed
                // string, so a decompression bomb refused by the decode
                // allocation limit and a genuinely corrupt image log
                // identically. What tells them apart is the `image` error in
                // `#[source]`, which only the `Debug` form carries.
                tracing::warn!(
                    mime = %source.mime,
                    bytes = source.data.len(),
                    error = ?error,
                    "clippo stored a copied image without a thumbnail"
                );
                None
            }
        }
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

    /// One entry's row, without any of its flavors.
    ///
    /// The cheap half of [`get`](Self::get), for callers that need the kind or
    /// the flags and not the content. Everything a blob costs — reading the
    /// overflow pages, decrypting them, copying them into a `Vec` — is skipped,
    /// which for a screenshot is the difference between a few hundred bytes and
    /// a few megabytes.
    pub fn entry(&self, id: EntryId) -> Result<Option<Entry>, StoreError> {
        match self.conn.query_row(
            &format!("SELECT {ENTRY_COLUMNS} FROM entries WHERE id = ?1"),
            [id.get()],
            entry_from_row,
        ) {
            Ok(entry) => Ok(Some(entry)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// The stored thumbnail for one entry, reading no other flavor.
    ///
    /// `None` when the entry has no thumbnail — it is not an image, or it was
    /// stored without one because it was too large or could not be decoded.
    ///
    /// Two statements rather than one, and neither of them selects `data` for a
    /// flavor it is not going to return. That is the whole point: a thumbnail
    /// sits in the same table as the full-size PNG it was derived from, so
    /// reaching it through [`get`](Self::get) would read and decrypt the
    /// megabytes beside it in order to hand back the kilobytes — quietly
    /// undoing what the derived thumbnail exists to save. The first statement
    /// asks only for MIME types, which are small and stored ahead of `data` in
    /// the row, so it never touches the blob's overflow pages.
    ///
    /// The MIME is matched with [`is_thumbnail`][crate::is_thumbnail] rather
    /// than by SQL equality so that a thumbnail carried in with a different
    /// spelling of the same type is still found; the second statement then asks
    /// for that exact stored spelling.
    pub fn thumbnail(&self, id: EntryId) -> Result<Option<Vec<u8>>, StoreError> {
        let mut mimes = self
            .conn
            .prepare("SELECT mime FROM flavors WHERE entry_id = ?1 ORDER BY rowid")?;
        let stored_as = mimes
            .query_map([id.get()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find(|mime| images::is_thumbnail(mime));

        let Some(stored_as) = stored_as else {
            return Ok(None);
        };

        match self.conn.query_row(
            "SELECT data FROM flavors WHERE entry_id = ?1 AND mime = ?2",
            params![id.get(), stored_as],
            |row| row.get(0),
        ) {
            Ok(data) => Ok(Some(data)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// One entry with all of its flavors, or `None` if there is no such id.
    pub fn get(&self, id: EntryId) -> Result<Option<StoredEntry>, StoreError> {
        let Some(entry) = self.entry(id)? else {
            return Ok(None);
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
    /// Returns whether there was anything to delete. Pinning does not protect
    /// an entry here: this is `Delete(id)`, a user pointing at one row.
    pub fn delete(&mut self, id: EntryId) -> Result<bool, StoreError> {
        let deleted = self
            .conn
            .execute("DELETE FROM entries WHERE id = ?1", [id.get()])?;
        if deleted > 0 {
            self.reclaim_after("delete");
        }
        Ok(deleted > 0)
    }

    /// Empty the history, returning how many entries went.
    ///
    /// `Clear()` over D-Bus. **Pinned entries survive unless `include_pinned`**
    /// — DESIGN.md, `clippo-store` → "Retention". The flag is a parameter
    /// rather than a second method so that a caller has to say which one it
    /// means; a `clear()` that took no argument would be a data-loss bug
    /// waiting for someone to assume the other default.
    pub fn clear(&mut self, include_pinned: bool) -> Result<usize, StoreError> {
        let deleted = if include_pinned {
            self.conn.execute("DELETE FROM entries", [])?
        } else {
            self.conn
                .execute("DELETE FROM entries WHERE pinned = 0", [])?
        };
        if deleted > 0 {
            tracing::info!(
                deleted,
                include_pinned,
                "clippo cleared the clipboard history"
            );
            self.reclaim_after("clear");
        }
        Ok(deleted)
    }

    /// Apply the retention limits now, against the caller's clock.
    ///
    /// [`Store::insert`] already does this after every copy, so this is for the
    /// gap that leaves: a daemon nobody has copied anything into all week has
    /// not swept, and entries can pass the age limit while it idles. `clippod`
    /// runs it at startup. See [`crate::retention`].
    pub fn enforce_retention(&mut self, now: Timestamp) -> Result<Sweep, StoreError> {
        let swept = retention::sweep(&self.conn, &self.retention, now)?;
        if !swept.is_empty() {
            self.reclaim_after("enforce_retention");
        }
        Ok(swept)
    }

    /// Give the pages a delete freed back to the operating system.
    ///
    /// The database is set to incremental auto-vacuum at open, which moves
    /// freed pages onto a free list; this is what actually truncates the file.
    /// Run after anything that deletes rows, and only then — the whole point of
    /// *incremental* is that the work is proportional to what was freed rather
    /// than to the size of the database, so a no-op call is cheap but a
    /// pointless one still costs a statement.
    ///
    /// It matters here more than it would elsewhere. A history of 8 MB
    /// screenshots that retention keeps dropping and replacing would otherwise
    /// grow to the high-water mark of everything it ever held and stay there,
    /// which is a file size nothing in the user's configuration explains. The
    /// *contents* of those pages are already gone — SQLCipher forces
    /// `PRAGMA secure_delete` on, so a freed page is zeroed in place before it
    /// reaches the free list — so this is about the size of the file, not about
    /// blobs lingering inside it.
    ///
    /// Stepped to completion by hand rather than run through `execute_batch`.
    /// `PRAGMA incremental_vacuum` frees **one page per step**, so the obvious
    /// one-line spelling silently reclaims a single 4 KB page and leaves the
    /// rest of an 8 MB screenshot on the free list — a bug that looks exactly
    /// like working code.
    ///
    /// Callers go through [`Store::reclaim_after`]; see there for why a failure
    /// is not the caller's failure.
    fn reclaim(&self) -> Result<(), StoreError> {
        let mut statement = self.conn.prepare("PRAGMA incremental_vacuum")?;
        let mut rows = statement.query([])?;
        while rows.next()?.is_some() {}
        Ok(())
    }

    /// [`Store::reclaim`] as housekeeping: log a failure, never report one.
    ///
    /// Every caller runs this *after* its own write is committed and durable.
    /// Propagating a vacuum error from there would tell the caller that the
    /// operation it asked for failed, when it did not: `clear` would report
    /// failure with the history already gone — and running it again returns
    /// `Ok(0)`, so the user is left with no way to make the reported failure
    /// come true — and `insert` would report a lost copy that is in fact stored,
    /// which at M3 means no `HistoryChanged` for a row the applet then does not
    /// know about.
    ///
    /// Failing to *shrink* a file is not failing to delete rows, and it is
    /// recoverable in a way a swallowed result is not: the pages stay on the
    /// free list and the next successful call picks them up. Deferring is
    /// exactly what incremental vacuum is for. `warn!` rather than `error!` for
    /// the same reason — nothing is wrong with the history, only with the file's
    /// size, and only until the next delete.
    fn reclaim_after(&self, operation: &'static str) {
        if let Err(error) = self.reclaim() {
            tracing::warn!(
                operation,
                error = ?error,
                "clippo could not reclaim the space a delete freed. The database file \
                 stays its current size until a later delete vacuums it; the deleted \
                 entries are gone either way"
            );
        }
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

/// The mode the history database is kept at: its owner, and nobody else.
const DB_FILE_MODE: u32 = 0o600;

/// Mode bits that must not be set on the database file: anything at all for
/// group or other. The same rule [`crate::key`] applies to the key file.
const DB_FORBIDDEN_MODE_BITS: u32 = 0o077;

/// Narrow the database file to [`DB_FILE_MODE`].
///
/// **Why this is two steps rather than one.** SQLite creates the file itself,
/// inside its VFS, and neither `rusqlite` nor SQLCipher exposes the mode to
/// create it with: the unix VFS uses `0644` masked by the process umask, so
/// the usual `022` leaves `history.db` world-readable. There is no flag to
/// pass and no hook to take, so the only place to set the mode is after the
/// file exists — which is the instant [`Connection::open_with_flags`] returns.
///
/// Two things make that gap uninteresting. The file is inside a `0700`
/// directory, which `create_data_dir` checks on the same startup, so nothing
/// can reach it during the gap anyway. And the chmod happens before the first
/// statement, so it is also before the first write: SQLite copies the database
/// file's own mode onto the rollback journal and any WAL file it creates, so
/// narrowing here narrows those too rather than leaving a `0644` journal
/// holding the same pages.
///
/// The contents are ciphertext either way. What a world-readable file leaks is
/// its size and the times it changes — how much clipboard history there is and
/// when the user copied something — to any account on the machine.
fn restrict_db_file(path: &Path) -> Result<(), StoreError> {
    let metadata = std::fs::metadata(path).map_err(|source| StoreError::FilePermissions {
        path: path.to_path_buf(),
        source,
    })?;

    // Narrower than 0600 is not "wider than 0600": a database the owner has
    // deliberately made read-only is their business, and SQLite has already
    // had its say about whether it can be opened read-write.
    let mode = metadata.permissions().mode() & 0o777;
    if mode & DB_FORBIDDEN_MODE_BITS == 0 {
        return Ok(());
    }

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(DB_FILE_MODE)).map_err(
        |source| StoreError::FilePermissions {
            path: path.to_path_buf(),
            source,
        },
    )
}

/// SQLite's auto-vacuum mode, as `PRAGMA auto_vacuum` reports it.
const AUTO_VACUUM_INCREMENTAL: i64 = 2;

/// Bring an existing database into incremental auto-vacuum mode, so deleted
/// blobs can be given back rather than growing the file for ever.
///
/// Incremental rather than `FULL`: full auto-vacuum reorganises pages inside
/// every commit that frees one, on the daemon's write path. Incremental puts
/// freed pages on a free list and leaves the truncation to [`Store::reclaim`],
/// which the store calls after a delete and never during an ordinary copy.
///
/// The ordering is the fiddly part. SQLite only lets this mode change on a
/// database that has no tables yet, or through a full `VACUUM`, so the two
/// cases are split: [`Store::open`] sets the pragma before the schema exists,
/// which is all a new file needs, and this converts one written by a clippo
/// from before the setting. That rewrite is a one-off — the mode lives in the
/// file header, so the next open finds it already there — and it is safe with
/// SQLCipher, which encrypts the temporary file `VACUUM` writes with the same
/// key as the database.
fn convert_to_incremental_vacuum(conn: &Connection) -> Result<(), StoreError> {
    let mode: i64 = conn.pragma_query_value(None, "auto_vacuum", |row| row.get(0))?;
    if mode != AUTO_VACUUM_INCREMENTAL {
        tracing::info!(
            mode,
            "converting the clippo history database to incremental auto-vacuum"
        );
        conn.execute_batch("VACUUM;")?;
    }
    Ok(())
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
    use crate::{Key, StoreError};

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

    /// A screenshot: one `image/png` flavor of a real, decodable PNG.
    fn screenshot(at: i64, width: u32, height: u32) -> NewEntry {
        selection(
            at,
            vec![Flavor::new(
                "image/png",
                images::testing::png(width, height),
            )],
        )
    }

    /// The stored flavor with this MIME type, if the entry has one.
    fn stored_flavor<'a>(stored: &'a StoredEntry, mime: &str) -> Option<&'a Flavor> {
        stored.flavors.iter().find(|flavor| flavor.mime == mime)
    }

    /// The targeted read `Thumbnail` uses. It has to give back exactly what
    /// `get` would have found, without the read and decrypt of the full-size
    /// blob that makes `get` the wrong tool for drawing a list.
    #[test]
    fn the_thumbnail_read_returns_the_derived_png_and_not_the_full_size_one() {
        let temp = Temp::new();
        let mut store = temp.open();

        let new = screenshot(1_000, 800, 400);
        let original = new.flavors[0].data.clone();
        let id = store.insert(&new).unwrap().id();

        let thumb = store
            .thumbnail(id)
            .unwrap()
            .expect("an image entry carries a thumbnail flavor");

        let stored = store.get(id).unwrap().unwrap();
        assert_eq!(
            thumb,
            stored_flavor(&stored, images::THUMBNAIL_MIME).unwrap().data,
            "the same bytes `get` would have found"
        );
        assert_ne!(thumb, original, "and not the full-size image beside it");
        assert!(thumb.len() < original.len());
    }

    #[test]
    fn there_is_no_thumbnail_for_text_or_for_an_entry_that_is_not_there() {
        let temp = Temp::new();
        let mut store = temp.open();

        let id = store.insert(&text(1_000, "hello")).unwrap().id();

        assert_eq!(store.thumbnail(id).unwrap(), None);
        assert_eq!(store.thumbnail(EntryId::new(9_999)).unwrap(), None);
    }

    /// The cheap half of `get`, for callers that want the kind or the flags.
    #[test]
    fn reading_one_entrys_row_gives_the_same_row_get_would() {
        let temp = Temp::new();
        let mut store = temp.open();

        let id = store.insert(&screenshot(1_000, 40, 40)).unwrap().id();

        let entry = store.entry(id).unwrap().expect("the entry is there");
        assert_eq!(entry, store.get(id).unwrap().unwrap().entry);
        assert_eq!(entry.kind, EntryKind::Image);

        assert_eq!(store.entry(EntryId::new(9_999)).unwrap(), None);
    }

    /// Not a property of this module so much as of SQLite, and it is pinned
    /// here because a frontend caching anything against an id depends on
    /// knowing it: `entries.id` is an `INTEGER PRIMARY KEY` *without*
    /// `AUTOINCREMENT`, so a freed id is handed straight back out. The applet's
    /// thumbnail cache is keyed on `(id, created_at)` for exactly this, and if
    /// this test ever starts failing that key can be simplified.
    #[test]
    fn a_deleted_entrys_id_is_reissued_to_the_next_insert() {
        let temp = Temp::new();
        let mut store = temp.open();

        let first = store.insert(&text(1_000, "first")).unwrap().id();
        let second = store.insert(&text(2_000, "second")).unwrap().id();
        assert_ne!(first, second);

        assert!(store.delete(second).unwrap());
        let reissued = store.insert(&text(3_000, "third")).unwrap().id();
        assert_eq!(
            reissued, second,
            "the newest id came straight back for a different entry"
        );

        store.clear(true).unwrap();
        let after_clear = store.insert(&text(4_000, "fourth")).unwrap().id();
        assert_eq!(after_clear, first, "and a clear restarts at the beginning");
    }

    #[test]
    fn an_image_round_trips_with_a_thumbnail_generated_beside_it() {
        let temp = Temp::new();
        let mut store = temp.open();

        let new = screenshot(1_000, 800, 400);
        let original = new.flavors[0].data.clone();
        let id = store.insert(&new).unwrap().id();

        let stored = store.get(id).unwrap().unwrap();
        assert_eq!(stored.entry.kind, EntryKind::Image);
        assert_eq!(
            stored.flavors.len(),
            2,
            "the captured image and the thumbnail clippo derived from it"
        );

        // The full-size image is byte-for-byte what was copied.
        assert_eq!(
            stored_flavor(&stored, "image/png").unwrap().data,
            original,
            "the stored image must paste as the image that was copied"
        );

        // The thumbnail is a real, smaller PNG under its own MIME type.
        let thumb = stored_flavor(&stored, images::THUMBNAIL_MIME)
            .expect("an image entry carries a thumbnail flavor");
        let decoded = image::load_from_memory(&thumb.data).expect("the thumbnail should decode");
        assert_eq!(
            (decoded.width(), decoded.height()),
            (images::THUMBNAIL_MAX_EDGE, images::THUMBNAIL_MAX_EDGE / 2),
            "scaled into the box with its aspect ratio kept"
        );
        assert!(
            thumb.data.len() < original.len(),
            "a list must be renderable without touching the full-size blob"
        );

        // And the thumbnail is not part of the entry's identity: the same
        // screenshot copied again is still one entry.
        assert!(store
            .insert(&screenshot(2_000, 800, 400))
            .unwrap()
            .was_deduplicated());
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn the_thumbnail_is_the_one_flavor_a_paste_must_not_offer_back() {
        // M3c reads this filter rather than reinventing it; pasting a 256-pixel
        // thumbnail in place of a screenshot is a silent wrong answer.
        let temp = Temp::new();
        let mut store = temp.open();
        let id = store.insert(&screenshot(1_000, 500, 500)).unwrap().id();

        let stored = store.get(id).unwrap().unwrap();
        let offerable: Vec<&str> = stored
            .flavors
            .iter()
            .filter(|flavor| images::is_offerable(&flavor.mime))
            .map(|flavor| flavor.mime.as_str())
            .collect();
        assert_eq!(offerable, vec!["image/png"]);
    }

    #[test]
    fn an_image_at_the_cap_is_stored_and_one_over_it_is_refused() {
        let temp = Temp::new();
        let picture = images::testing::png(200, 200);
        let mut store = temp.open().with_config(&Config {
            // Exactly the size of the picture: at the cap, not over it.
            max_image_bytes: picture.len() as u64,
            ..Config::default()
        });

        let at_the_cap = selection(1_000, vec![Flavor::new("image/png", picture.clone())]);
        assert!(store.insert(&at_the_cap).is_ok(), "at the cap is under it");

        let over_the_cap = screenshot(2_000, 400, 400);
        let bytes = over_the_cap.flavors[0].data.len();
        assert!(bytes > picture.len(), "the fixture must actually be bigger");

        let error = store.insert(&over_the_cap).unwrap_err();
        assert!(
            matches!(&error, StoreError::ImageTooLarge { bytes: reported, .. }
                if *reported == bytes as u64),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(message.contains("max_image_bytes"), "{message}");
        assert!(message.contains("image/png"), "{message}");

        // Refused rather than truncated: no half-written entry, no blob.
        assert_eq!(store.count().unwrap(), 1);
        assert_eq!(
            flavor_rows(&store),
            2,
            "only the first image and its thumbnail"
        );
    }

    #[test]
    fn an_over_cap_image_beside_a_smaller_one_is_dropped_rather_than_refused() {
        // The entry's identity rests on the canonical `image/png`, which fits.
        // The redundant JPEG encoding of the same picture does not, and losing
        // it costs the user nothing.
        let temp = Temp::new();
        let small = images::testing::png(8, 8);
        let large = images::testing::jpeg(600, 600);
        assert!(large.len() > small.len());

        let mut store = temp.open().with_config(&Config {
            max_image_bytes: small.len() as u64,
            ..Config::default()
        });

        let id = store
            .insert(&selection(
                1_000,
                vec![
                    Flavor::new("image/png", small),
                    Flavor::new("image/jpeg", large),
                ],
            ))
            .unwrap()
            .id();

        let stored = store.get(id).unwrap().unwrap();
        assert!(stored_flavor(&stored, "image/png").is_some());
        assert!(
            stored_flavor(&stored, "image/jpeg").is_none(),
            "the over-cap flavor was dropped, not truncated"
        );
        assert!(stored_flavor(&stored, images::THUMBNAIL_MIME).is_some());
    }

    #[test]
    fn a_cap_of_zero_means_no_image_is_ever_small_enough() {
        // config.rs documents that reading; this is where it takes effect.
        let temp = Temp::new();
        let mut store = temp.open().with_config(&Config {
            max_image_bytes: 0,
            ..Config::default()
        });
        assert!(matches!(
            store.insert(&screenshot(1_000, 4, 4)).unwrap_err(),
            StoreError::ImageTooLarge { .. }
        ));
        // Text is unaffected — only images are capped.
        assert!(store.insert(&text(2_000, "hello")).is_ok());
    }

    #[test]
    fn an_image_that_will_not_decode_is_stored_without_a_thumbnail() {
        // A truncated or unsupported image must not cost the user the copy: a
        // list row without a picture beats an entry that vanished.
        let temp = Temp::new();
        let mut store = temp.open();
        let id = store
            .insert(&selection(
                1_000,
                vec![Flavor::new(
                    "image/png",
                    b"\x89PNG\r\n\x1a\n truncated".to_vec(),
                )],
            ))
            .unwrap()
            .id();

        let stored = store.get(id).unwrap().unwrap();
        assert_eq!(stored.entry.kind, EntryKind::Image);
        assert_eq!(stored.flavors.len(), 1, "the entry, without a thumbnail");
        assert!(stored_flavor(&stored, images::THUMBNAIL_MIME).is_none());
        assert_eq!(
            stored_flavor(&stored, "image/png").unwrap().data,
            b"\x89PNG\r\n\x1a\n truncated".to_vec(),
            "the bytes are kept as captured, in case something else can read them"
        );
    }

    #[test]
    fn a_capture_that_already_carried_a_thumbnail_does_not_get_a_second_one() {
        let temp = Temp::new();
        let mut store = temp.open();
        let id = store
            .insert(&selection(
                1_000,
                vec![
                    Flavor::new("image/png", images::testing::png(64, 64)),
                    Flavor::new(images::THUMBNAIL_MIME, b"whatever was carried".to_vec()),
                ],
            ))
            .unwrap()
            .id();

        let stored = store.get(id).unwrap().unwrap();
        assert_eq!(stored.flavors.len(), 2);
        assert_eq!(
            stored_flavor(&stored, images::THUMBNAIL_MIME).unwrap().data,
            b"whatever was carried".to_vec()
        );
    }

    #[test]
    fn text_entries_get_no_thumbnail() {
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
        assert_eq!(store.get(id).unwrap().unwrap().flavors.len(), 2);
    }

    #[test]
    fn the_space_a_deleted_blob_used_is_given_back_rather_than_kept_for_ever() {
        // The written decision the criterion asks for, as a test: incremental
        // auto-vacuum, triggered by anything that deletes rows. Without it an
        // 8 MB screenshot that retention dropped stays resident inside an
        // encrypted file nobody can inspect to check.
        let temp = Temp::new();
        let mut store = temp.open().with_retention(crate::Retention {
            max_entries: 2,
            max_age: None,
        });

        let mode: i64 = store
            .conn
            .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
            .unwrap();
        assert_eq!(
            mode, AUTO_VACUUM_INCREMENTAL,
            "set before the schema exists"
        );

        // Twenty copies of a quarter-megabyte of incompressible bytes, keeping
        // two: a file that grew with the churn would be some 5 MB.
        for n in 0..20u32 {
            let blob: Vec<u8> = (0..250_000u32)
                .map(|byte| (byte.wrapping_mul(2_654_435_761).wrapping_add(n)) as u8)
                .collect();
            store
                .insert(&selection(
                    1_000 + i64::from(n),
                    vec![Flavor::new("image/png", blob)],
                ))
                .unwrap();
        }

        assert_eq!(store.count().unwrap(), 2);
        let free: i64 = store
            .conn
            .query_row("PRAGMA freelist_count", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            free, 0,
            "freed pages were handed back, not left on the list"
        );

        let size = std::fs::metadata(temp.db()).unwrap().len();
        assert!(
            size < 2 * 1024 * 1024,
            "the file grew to {size} bytes holding two 250 KB entries"
        );
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
    fn a_repeat_copy_can_only_ever_make_an_entry_more_sensitive() {
        // The same bytes out of a text editor and then out of a password
        // manager: identical hash, so the second is a bump, but only the second
        // carries the marker. A bump that dropped it would leave a password the
        // applet renders in full.
        let temp = Temp::new();
        let mut store = temp.open();

        let id = store.insert(&text(1_000, "hunter2")).unwrap().id();
        let stored = store.get(id).unwrap().unwrap();
        assert!(!stored.entry.sensitive);
        assert_eq!(stored.entry.preview, "hunter2");

        // The daemon masks before it stores, so a sensitive capture arrives
        // with a masked preview. The flag and the preview have to move
        // together: a row flagged sensitive whose preview is still the
        // password is the worst of both, because the applet draws its lock
        // badge next to the plaintext.
        let mut from_password_manager = text(2_000, "hunter2");
        from_password_manager.sensitive = true;
        from_password_manager.preview = "hu••••••••r2".to_owned();
        assert_eq!(
            store.insert(&from_password_manager).unwrap(),
            Insertion::Bumped(id)
        );
        let stored = store.get(id).unwrap().unwrap();
        assert!(stored.entry.sensitive);
        assert_eq!(
            stored.entry.preview, "hu••••••••r2",
            "the better-informed capture's preview replaces the one in the clear"
        );

        // And not back again: an entry that was ever sensitive stays sensitive
        // until something with a user behind it says otherwise — and so does
        // its mask, or an unmarked third copy would put the password back on
        // screen with the flag still set.
        assert_eq!(
            store.insert(&text(3_000, "hunter2")).unwrap(),
            Insertion::Bumped(id)
        );
        let stored = store.get(id).unwrap().unwrap();
        assert!(stored.entry.sensitive);
        assert_eq!(stored.entry.preview, "hu••••••••r2", "the mask stays too");
        assert_eq!(
            stored.entry.last_used_at,
            Timestamp::from_unix_millis(3_000)
        );

        // The value itself is untouched by any of this — the flavors are what
        // `Reveal` and the copy-back path read, and a bump never rewrites them.
        assert_eq!(
            stored.flavors[0].as_str().unwrap(),
            "hunter2",
            "masking is display-only, even across a bump"
        );
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

    #[test]
    #[cfg(unix)]
    fn the_database_is_not_readable_by_anyone_but_its_owner() {
        // The contents are ciphertext, so this is not about the copied bytes;
        // it is about the file's size and mtime, which say how much history
        // there is and when the user last copied something.
        let temp = Temp::new();
        let mut store = temp.open();
        store
            .insert(&text(1_000, "something to make it grow"))
            .unwrap();

        // Every file the store left behind, for the reason
        // `what_was_copied_is_not_in_the_file` checks them all: a journal or a
        // WAL holds the same pages as the database.
        let mut checked = 0;
        for entry in std::fs::read_dir(temp.dir.path()).unwrap() {
            let path = entry.unwrap().path();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode & DB_FORBIDDEN_MODE_BITS,
                0,
                "{} is mode {mode:04o}",
                path.display()
            );
            checked += 1;
        }
        assert!(checked > 0, "the database file should exist");
    }

    #[test]
    #[cfg(unix)]
    fn a_database_left_wide_by_an_earlier_run_is_narrowed_when_it_is_opened() {
        // Not only fresh files: a history.db created before clippo set the
        // mode, or by a user's `cp`, is fixed on the next open rather than
        // left as it was found.
        let temp = Temp::new();
        drop(temp.open());
        std::fs::set_permissions(temp.db(), std::fs::Permissions::from_mode(0o644)).unwrap();

        drop(temp.open());

        let mode = std::fs::metadata(temp.db()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:04o}");
    }

    #[test]
    fn a_database_written_before_auto_vacuum_existed_is_converted_on_open() {
        // The mode lives in the file header and SQLite only lets it change on a
        // database with no tables, or through a full VACUUM. A history written
        // by an earlier clippo would otherwise never reclaim anything, silently.
        let temp = Temp::new();
        let hash = {
            // The database as M2b made it: keyed, schema'd, no auto-vacuum.
            let conn = Connection::open(temp.db()).unwrap();
            conn.execute_batch(&temp.key.pragma()).unwrap();
            schema::ensure(&conn, &temp.db()).unwrap();
            let mode: i64 = conn
                .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
                .unwrap();
            assert_eq!(mode, 0, "the fixture has to actually predate the setting");

            let new = text(1_000, "written by the old clippo");
            conn.execute(
                "INSERT INTO entries (created_at, last_used_at, kind, preview, hash)
                 VALUES (1000, 1000, 'text', 'written by the old clippo', ?1)",
                [&new.hash],
            )
            .unwrap();
            new.hash
        };

        let store = temp.open();
        let mode: i64 = store
            .conn
            .pragma_query_value(None, "auto_vacuum", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, AUTO_VACUUM_INCREMENTAL, "converted by the VACUUM");

        // And the rewrite kept the history rather than starting it again.
        assert_eq!(store.count().unwrap(), 1);
        assert_eq!(store.list(1, 0).unwrap()[0].hash, hash);

        // Once converted, it stays converted — the next open does no VACUUM.
        drop(store);
        let store = temp.open();
        assert_eq!(store.count().unwrap(), 1);
    }

    #[test]
    fn a_reclaim_that_fails_is_logged_rather_than_reported_as_a_failed_delete() {
        // Reclaiming runs after the delete is already committed, so an error
        // from it must not become the caller's error: a `clear` that reports
        // failure with the history already gone leaves the user with nothing to
        // retry — the second run returns Ok(0) — and an `insert` that reports
        // failure with the row stored is a history M3's applet never hears about.
        let temp = Temp::new();
        let mut store = temp.open();
        store.insert(&text(1_000, "something to free")).unwrap();

        // The delete itself succeeds, and reports what it did.
        assert_eq!(store.clear(false).unwrap(), 1);
        assert_eq!(store.count().unwrap(), 0);

        // Now make the vacuum genuinely fail — this is the probe that keeps the
        // rest of the test from passing vacuously, because `reclaim_after`
        // swallowing an error only means anything if there is one to swallow.
        store.conn.execute_batch("PRAGMA query_only = ON;").unwrap();
        assert!(
            store.reclaim().is_err(),
            "the probe has to actually break the vacuum"
        );

        // Swallowed: no panic, nothing to propagate, and the history the delete
        // emptied is still empty.
        store.reclaim_after("test");
        assert_eq!(store.count().unwrap(), 0);
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
