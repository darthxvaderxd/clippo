//! The clipboard vocabulary: what one copy is, and what it is made of.
//!
//! These types mirror the two tables in DESIGN.md, `clippo-store` → "Schema",
//! field for field:
//!
//! ```text
//! entries(id, created_at, last_used_at, kind, preview, hash UNIQUE, pinned, sensitive)
//! flavors(entry_id, mime, data BLOB)
//! ```
//!
//! [`Entry`] is one `entries` row and [`Flavor`] is one `flavors` row minus the
//! join key, which the store owns. Keeping the names identical is deliberate:
//! it makes `clippo-store` a mapping layer rather than a translation layer.

use std::fmt;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The primary key of an [`Entry`], as handed out by the store.
///
/// A newtype rather than a bare `i64` so an entry id can never be passed where
/// a timestamp or a count is expected — both are `i64` in the schema too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntryId(i64);

impl EntryId {
    /// Wrap the id the database assigned.
    pub const fn new(id: i64) -> Self {
        Self(id)
    }

    /// The underlying value, for binding back into SQL or D-Bus.
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for EntryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for EntryId {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.trim().parse::<i64>().map(Self)
    }
}

impl From<i64> for EntryId {
    fn from(id: i64) -> Self {
        Self(id)
    }
}

impl From<EntryId> for i64 {
    fn from(id: EntryId) -> Self {
        id.0
    }
}

/// A point in time, stored as milliseconds since the Unix epoch.
///
/// Milliseconds rather than seconds so that two distinct copies made in the
/// same second still order correctly in the history; `i64` rather than
/// `SystemTime` so the value maps straight onto a SQLite `INTEGER` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    /// The current wall-clock time.
    ///
    /// A clock set before 1970 yields a negative value rather than panicking;
    /// retention arithmetic saturates, so a nonsense clock cannot delete
    /// history it should not.
    pub fn now() -> Self {
        Self(match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(since) => millis_of(since),
            Err(before) => -millis_of(before.duration()),
        })
    }

    /// Wrap a raw column value.
    pub const fn from_unix_millis(millis: i64) -> Self {
        Self(millis)
    }

    /// The raw value, for binding back into SQL.
    pub const fn as_unix_millis(self) -> i64 {
        self.0
    }

    /// How much later `self` is than `earlier`, or zero if it is not later.
    pub fn since(self, earlier: Self) -> Duration {
        let delta = self.0.saturating_sub(earlier.0);
        Duration::from_millis(u64::try_from(delta).unwrap_or(0))
    }

    /// This timestamp moved forward by `age`, saturating at [`i64::MAX`].
    ///
    /// Retention uses it to turn "30 days" into a cutoff.
    pub fn saturating_add(self, age: Duration) -> Self {
        Self(self.0.saturating_add(millis_of(age)))
    }

    /// This timestamp moved back by `age`, saturating at [`i64::MIN`].
    pub fn saturating_sub(self, age: Duration) -> Self {
        Self(self.0.saturating_sub(millis_of(age)))
    }
}

/// A duration in whole milliseconds, clamped into `i64` range.
fn millis_of(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// What sort of thing a copy is, which decides how the applet renders it.
///
/// Stored in the `entries.kind` column as the lowercase word from
/// [`EntryKind::as_str`], so the column stays readable in a debug dump and new
/// variants do not renumber the old ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EntryKind {
    /// Plain text — the overwhelmingly common case.
    Text,
    /// Rich text copied from a browser or word processor.
    Html,
    /// One or more file or web URIs, as `text/uri-list`.
    Uris,
    /// A raster image, stored as a blob with a thumbnail alongside it.
    Image,
}

impl EntryKind {
    /// Every kind, in the order they are ranked when a selection carries
    /// several flavors.
    pub const ALL: &'static [EntryKind] = &[
        EntryKind::Image,
        EntryKind::Uris,
        EntryKind::Html,
        EntryKind::Text,
    ];

    /// The word written to the `kind` column.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Html => "html",
            Self::Uris => "uris",
            Self::Image => "image",
        }
    }

    /// The kind implied by a single MIME type, if it implies one.
    ///
    /// Parameters and case are ignored, so `text/plain; charset=UTF-8` is text.
    /// Flavors that carry no content of their own — the
    /// `x-kde-passwordManagerHint` marker, say — return `None`.
    pub fn from_mime(mime: &str) -> Option<Self> {
        let mime = mime
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match mime.as_str() {
            "text/html" => Some(Self::Html),
            "text/uri-list" => Some(Self::Uris),
            _ if mime.starts_with("image/") => Some(Self::Image),
            _ if mime.starts_with("text/") => Some(Self::Text),
            _ => None,
        }
    }

    /// The kind of a whole selection.
    ///
    /// Real copies advertise several flavors at once — a browser offers
    /// `text/html` *and* `text/plain`, a file manager offers `text/uri-list`
    /// *and* `text/plain` — so the richest flavor present wins, in the order of
    /// [`EntryKind::ALL`]. Returns `None` when nothing carries content.
    pub fn for_flavors(flavors: &[Flavor]) -> Option<Self> {
        let found: Vec<Self> = flavors
            .iter()
            .filter_map(|flavor| Self::from_mime(&flavor.mime))
            .collect();
        Self::ALL.iter().copied().find(|kind| found.contains(kind))
    }
}

impl fmt::Display for EntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A `kind` column value that is not one clippo writes.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{value:?} is not a clippo entry kind (expected one of text, html, uris, image)")]
pub struct ParseEntryKindError {
    /// The value that was read.
    pub value: String,
}

impl FromStr for EntryKind {
    type Err = ParseEntryKindError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "text" => Ok(Self::Text),
            "html" => Ok(Self::Html),
            "uris" => Ok(Self::Uris),
            "image" => Ok(Self::Image),
            other => Err(ParseEntryKindError {
                value: other.to_owned(),
            }),
        }
    }
}

/// One MIME flavor of one copy: a `flavors` row without its `entry_id`.
///
/// `Debug` prints the byte count rather than the bytes. Clipboard contents
/// routinely include passwords, and a stray `{flavor:?}` in a log line would
/// put one in the journal — the same reasoning as `clippo_wayland::Flavor`,
/// which this type mirrors on the storage side.
#[derive(Clone, PartialEq, Eq)]
pub struct Flavor {
    /// The MIME type, as the source advertised it.
    pub mime: String,
    /// The bytes for this flavor.
    pub data: Vec<u8>,
}

impl Flavor {
    /// Build a flavor from anything string-like and any bytes.
    pub fn new(mime: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        Self {
            mime: mime.into(),
            data: data.into(),
        }
    }

    /// The data as UTF-8, or `None` if it is not valid UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.data).ok()
    }
}

impl fmt::Debug for Flavor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Flavor")
            .field("mime", &self.mime)
            .field("bytes", &self.data.len())
            .finish()
    }
}

/// One stored copy: exactly the `entries` row.
///
/// The flavors live in their own table, so they are not a field here; a copy on
/// its way *into* the store, which has both and no id yet, is a [`NewEntry`].
///
/// `Debug` prints the preview's length rather than its text, for the reason
/// given on [`Flavor`].
#[derive(Clone, PartialEq, Eq)]
pub struct Entry {
    /// Primary key.
    pub id: EntryId,
    /// When the copy was first captured.
    pub created_at: Timestamp,
    /// When it was last copied or pasted; a repeat copy bumps this instead of
    /// inserting a duplicate row.
    pub last_used_at: Timestamp,
    /// What sort of content it is.
    pub kind: EntryKind,
    /// A short rendering for the list, already masked if `sensitive`.
    pub preview: String,
    /// BLAKE3 of the canonical flavor, lowercase hex. `UNIQUE`: this is what
    /// dedup keys on.
    pub hash: String,
    /// Pinned entries are exempt from both retention limits, and from `Clear()`
    /// unless explicitly included.
    pub pinned: bool,
    /// Whether secret detection flagged this at capture time. Drives masking in
    /// every frontend.
    pub sensitive: bool,
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry")
            .field("id", &self.id)
            .field("created_at", &self.created_at)
            .field("last_used_at", &self.last_used_at)
            .field("kind", &self.kind)
            .field("preview_chars", &self.preview.chars().count())
            .field("hash", &self.hash)
            .field("pinned", &self.pinned)
            .field("sensitive", &self.sensitive)
            .finish()
    }
}

/// A copy on its way into the store: everything an [`Entry`] has except the id
/// the database has not assigned yet, plus the flavors it is made of.
///
/// `created_at` and `last_used_at` both start at [`NewEntry::created_at`]; the
/// store sets them when it inserts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEntry {
    /// When the selection was captured.
    pub created_at: Timestamp,
    /// What sort of content it is.
    pub kind: EntryKind,
    /// A short rendering for the list, already masked if `sensitive`.
    pub preview: String,
    /// BLAKE3 of the canonical flavor, lowercase hex.
    pub hash: String,
    /// Whether secret detection flagged it.
    pub sensitive: bool,
    /// Every captured flavor, one `flavors` row each.
    pub flavors: Vec<Flavor>,
}

impl NewEntry {
    /// The flavor with this exact MIME type, if it was captured.
    pub fn flavor(&self, mime: &str) -> Option<&Flavor> {
        self.flavors.iter().find(|flavor| flavor.mime == mime)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flavors(mimes: &[&str]) -> Vec<Flavor> {
        mimes.iter().map(|mime| Flavor::new(*mime, "x")).collect()
    }

    #[test]
    fn entry_kind_round_trips_through_the_kind_column() {
        for kind in EntryKind::ALL {
            assert_eq!(kind.as_str().parse::<EntryKind>().unwrap(), *kind);
        }
    }

    #[test]
    fn an_unknown_kind_column_is_an_error_not_a_guess() {
        let error = "rtf".parse::<EntryKind>().unwrap_err();
        assert_eq!(error.value, "rtf");
        assert!(error.to_string().contains("text, html, uris, image"));
    }

    #[test]
    fn mime_parameters_and_case_do_not_change_the_kind() {
        assert_eq!(
            EntryKind::from_mime("text/plain; charset=UTF-8"),
            Some(EntryKind::Text)
        );
        assert_eq!(EntryKind::from_mime("TEXT/HTML"), Some(EntryKind::Html));
        assert_eq!(EntryKind::from_mime("image/jpeg"), Some(EntryKind::Image));
        assert_eq!(
            EntryKind::from_mime("image/png;clippo-thumb"),
            Some(EntryKind::Image)
        );
    }

    #[test]
    fn a_flavor_carrying_no_content_implies_no_kind() {
        assert_eq!(EntryKind::from_mime("x-kde-passwordManagerHint"), None);
        assert_eq!(EntryKind::from_mime(""), None);
    }

    #[test]
    fn the_richest_flavor_of_a_selection_decides_its_kind() {
        // A browser copy, a file-manager copy, a screenshot, a password.
        assert_eq!(
            EntryKind::for_flavors(&flavors(&["text/html", "text/plain"])),
            Some(EntryKind::Html)
        );
        assert_eq!(
            EntryKind::for_flavors(&flavors(&["text/uri-list", "text/plain"])),
            Some(EntryKind::Uris)
        );
        assert_eq!(
            EntryKind::for_flavors(&flavors(&["image/png", "text/plain"])),
            Some(EntryKind::Image)
        );
        assert_eq!(
            EntryKind::for_flavors(&flavors(&["text/plain", "x-kde-passwordManagerHint"])),
            Some(EntryKind::Text)
        );
        assert_eq!(
            EntryKind::for_flavors(&flavors(&["x-kde-passwordManagerHint"])),
            None
        );
    }

    #[test]
    fn timestamps_map_straight_onto_an_integer_column() {
        let stored = Timestamp::from_unix_millis(1_700_000_000_000);
        assert_eq!(stored.as_unix_millis(), 1_700_000_000_000);
        assert!(Timestamp::now() > Timestamp::from_unix_millis(0));
    }

    #[test]
    fn timestamp_arithmetic_saturates_rather_than_wrapping() {
        let now = Timestamp::from_unix_millis(10_000);
        assert_eq!(
            now.since(Timestamp::from_unix_millis(4_000)).as_millis(),
            6_000
        );
        // Going backwards is not a negative duration, it is no duration.
        assert_eq!(Timestamp::from_unix_millis(4_000).since(now).as_millis(), 0);
        assert_eq!(
            Timestamp::from_unix_millis(i64::MAX).saturating_add(Duration::from_secs(1)),
            Timestamp::from_unix_millis(i64::MAX)
        );
        assert_eq!(
            Timestamp::from_unix_millis(i64::MIN).saturating_sub(Duration::from_secs(1)),
            Timestamp::from_unix_millis(i64::MIN)
        );
    }

    #[test]
    fn entry_ids_survive_the_cli_and_the_database() {
        assert_eq!(" 42 ".parse::<EntryId>().unwrap(), EntryId::new(42));
        assert_eq!(EntryId::new(42).to_string(), "42");
        assert_eq!(i64::from(EntryId::from(7_i64)), 7);
        assert!("two".parse::<EntryId>().is_err());
    }

    #[test]
    fn debug_never_prints_clipboard_contents() {
        let flavor = Flavor::new("text/plain", "hunter2");
        let rendered = format!("{flavor:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("text/plain"), "{rendered}");

        let entry = Entry {
            id: EntryId::new(1),
            created_at: Timestamp::from_unix_millis(0),
            last_used_at: Timestamp::from_unix_millis(0),
            kind: EntryKind::Text,
            preview: "hunter2".to_owned(),
            hash: "abc123".to_owned(),
            pinned: false,
            sensitive: true,
        };
        let rendered = format!("{entry:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("preview_chars: 7"), "{rendered}");
    }

    #[test]
    fn a_new_entry_exposes_its_flavors_by_mime() {
        let new = NewEntry {
            created_at: Timestamp::from_unix_millis(1),
            kind: EntryKind::Text,
            preview: "hi".to_owned(),
            hash: "abc".to_owned(),
            sensitive: false,
            flavors: vec![Flavor::new("text/plain", "hi")],
        };
        assert_eq!(new.flavor("text/plain").unwrap().as_str(), Some("hi"));
        assert!(new.flavor("text/html").is_none());
    }
}
