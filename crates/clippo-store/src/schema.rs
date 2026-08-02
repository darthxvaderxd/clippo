//! The two tables, and the version stamp that says which shape they are in.
//!
//! The schema is DESIGN.md's, `clippo-store` → "Schema", column for column:
//!
//! ```text
//! entries(id, created_at, last_used_at, kind, preview, hash UNIQUE, pinned, sensitive)
//! flavors(entry_id, mime, data BLOB)
//! ```
//!
//! Two constraints in there do real work rather than documenting intent:
//!
//! - **`entries.hash UNIQUE`** is what makes dedup true. The store tries the
//!   insert and treats the constraint violation as "already have this", so a
//!   second writer racing on the same copy cannot produce two rows however the
//!   two `SELECT`s interleave. See [`crate::Store::insert`].
//! - **`flavors.entry_id REFERENCES entries(id) ON DELETE CASCADE`** is what
//!   makes deletion true. Blobs are the part of the database that actually
//!   holds clipboard contents; an orphaned `flavors` row is a password that
//!   survived the user deleting it. Note that this only bites with
//!   `PRAGMA foreign_keys = ON`, which SQLite does *not* default to and which
//!   [`crate::Store::open`] therefore sets on every connection.
//!
//! # Versioning
//!
//! `PRAGMA user_version` carries [`SCHEMA_VERSION`], written in the same
//! transaction that creates the tables so a half-created database cannot claim
//! to be a whole one. On open:
//!
//! | `user_version` | What happens |
//! |---|---|
//! | 0, no tables | fresh database — create the schema, stamp the version |
//! | 0, tables present | refused: something else's database, or one clippo never finished writing |
//! | [`SCHEMA_VERSION`] | opened |
//! | above | refused, naming both versions — a newer clippo wrote it |
//! | below | refused — no migration exists yet |
//!
//! The "above" row is the one that matters. Letting an old build write to a
//! schema it does not understand is how a history gets corrupted rather than
//! merely unreadable, and the encryption means there is no `sqlite3` shell to
//! repair it with afterwards.

use std::path::Path;

use rusqlite::Connection;

use crate::StoreError;

/// The schema version this build reads and writes.
///
/// Bump when the DDL below changes, and add the migration that gets a database
/// from the previous version to this one — [`ensure`] refuses anything it has
/// no path from, rather than guessing.
pub const SCHEMA_VERSION: i64 = 1;

/// Version 1 of the schema.
///
/// `STRICT` is deliberate: without it SQLite would happily store the string
/// `"yes"` in `pinned`, and the first anyone would know is a row that fails to
/// map back into an [`Entry`](clippo_core::Entry) months later.
const SCHEMA_SQL: &str = "\
CREATE TABLE entries (
    id           INTEGER PRIMARY KEY,
    created_at   INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL,
    kind         TEXT    NOT NULL,
    preview      TEXT    NOT NULL,
    hash         TEXT    NOT NULL UNIQUE,
    pinned       INTEGER NOT NULL DEFAULT 0,
    sensitive    INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE INDEX entries_by_recency ON entries (last_used_at DESC, id DESC);

CREATE TABLE flavors (
    entry_id INTEGER NOT NULL REFERENCES entries (id) ON DELETE CASCADE,
    mime     TEXT    NOT NULL,
    data     BLOB    NOT NULL,
    PRIMARY KEY (entry_id, mime)
) STRICT;
";

/// Bring a freshly keyed connection up to [`SCHEMA_VERSION`], or refuse it.
///
/// `path` is carried only so the errors can name the file the user has to deal
/// with.
pub(crate) fn ensure(conn: &Connection, path: &Path) -> Result<(), StoreError> {
    let found: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    match found {
        // A database with no version stamp is either brand new or not ours.
        0 if is_empty(conn)? => create(conn),
        0 => Err(StoreError::SchemaUnversioned {
            path: path.to_path_buf(),
        }),
        found if found == SCHEMA_VERSION => Ok(()),
        found if found > SCHEMA_VERSION => Err(StoreError::SchemaTooNew {
            path: path.to_path_buf(),
            found,
            supported: SCHEMA_VERSION,
        }),
        found => Err(StoreError::SchemaTooOld {
            path: path.to_path_buf(),
            found,
            supported: SCHEMA_VERSION,
        }),
    }
}

/// Whether the database has no tables of its own yet.
fn is_empty(conn: &Connection) -> Result<bool, StoreError> {
    // `sqlite_master` rather than the newer `sqlite_schema` alias, so this does
    // not depend on which SQLite the vendored SQLCipher was cut from.
    let tables: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        [],
        |row| row.get(0),
    )?;
    Ok(tables == 0)
}

/// Create the schema and stamp its version, in one transaction.
fn create(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(&format!(
        "BEGIN;\n{SCHEMA_SQL}\nPRAGMA user_version = {SCHEMA_VERSION};\nCOMMIT;"
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Temp;

    /// Stamp a version onto an existing database and close it, so the next
    /// open has to decide what to do about it.
    fn restamp(temp: &Temp, version: i64) {
        let store = temp.open();
        store
            .connection()
            .execute_batch(&format!("PRAGMA user_version = {version};"))
            .unwrap();
    }

    #[test]
    fn a_fresh_database_gets_the_schema_and_the_stamp() {
        let temp = Temp::new();
        let store = temp.open();
        let conn = store.connection();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // The columns DESIGN.md names, in the tables it names them in.
        for (table, columns) in [
            (
                "entries",
                vec![
                    "id",
                    "created_at",
                    "last_used_at",
                    "kind",
                    "preview",
                    "hash",
                    "pinned",
                    "sensitive",
                ],
            ),
            ("flavors", vec!["entry_id", "mime", "data"]),
        ] {
            let found: Vec<String> = conn
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(found, columns, "{table}");
        }
    }

    #[test]
    fn the_hash_column_is_unique_in_the_schema_not_just_in_the_code() {
        let temp = Temp::new();
        let store = temp.open();
        store
            .connection()
            .execute(
                "INSERT INTO entries (created_at, last_used_at, kind, preview, hash)
                 VALUES (1, 1, 'text', 'hi', 'abc')",
                [],
            )
            .unwrap();
        let error = store
            .connection()
            .execute(
                "INSERT INTO entries (created_at, last_used_at, kind, preview, hash)
                 VALUES (2, 2, 'text', 'ho', 'abc')",
                [],
            )
            .unwrap_err();
        assert!(error.to_string().contains("UNIQUE"), "{error}");
    }

    #[test]
    fn foreign_keys_are_enforced_on_the_connection_not_merely_declared() {
        // SQLite defaults `foreign_keys` off, so a schema that declares the
        // reference and a connection that does not enable it look identical
        // until an orphan row appears.
        let temp = Temp::new();
        let store = temp.open();
        let enforced: i64 = store
            .connection()
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(enforced, 1);

        let error = store
            .connection()
            .execute(
                "INSERT INTO flavors (entry_id, mime, data) VALUES (99, 'text/plain', x'00')",
                [],
            )
            .unwrap_err();
        assert!(error.to_string().contains("FOREIGN KEY"), "{error}");
    }

    #[test]
    fn reopening_an_existing_database_keeps_its_version() {
        let temp = Temp::new();
        drop(temp.open());

        let reopened = temp.open();
        let version: i64 = reopened
            .connection()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn a_database_from_a_future_clippo_is_refused_rather_than_written_to() {
        let temp = Temp::new();
        restamp(&temp, SCHEMA_VERSION + 1);

        let error = temp.try_open().unwrap_err();
        assert!(
            matches!(error, StoreError::SchemaTooNew { found, .. } if found == SCHEMA_VERSION + 1),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains(&temp.db().display().to_string()),
            "{message}"
        );
        assert!(message.contains("newer version of clippo"), "{message}");
    }

    #[test]
    fn a_database_from_a_version_with_no_migration_is_refused_too() {
        // Only reachable in the wild once SCHEMA_VERSION has moved past 1;
        // forced here so the arm cannot rot into an untested one.
        let temp = Temp::new();
        restamp(&temp, -1);

        let error = temp.try_open().unwrap_err();
        assert!(
            matches!(error, StoreError::SchemaTooOld { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_database_with_tables_but_no_version_is_not_assumed_to_be_ours() {
        let temp = Temp::new();
        restamp(&temp, 0);

        let error = temp.try_open().unwrap_err();
        assert!(
            matches!(error, StoreError::SchemaUnversioned { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn the_schema_rejects_a_value_of_the_wrong_type() {
        // What `STRICT` buys: `pinned` cannot quietly become the text "yes".
        let temp = Temp::new();
        let store = temp.open();
        let error = store
            .connection()
            .execute(
                "INSERT INTO entries (created_at, last_used_at, kind, preview, hash, pinned)
                 VALUES (1, 1, 'text', 'hi', 'abc', 'yes')",
                [],
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot store TEXT value in INTEGER column"),
            "{error}"
        );
    }
}
