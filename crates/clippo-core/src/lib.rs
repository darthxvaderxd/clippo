//! The vocabulary every other clippo crate speaks.
//!
//! - [`entry`] — [`Entry`], [`Flavor`] and [`EntryKind`], mirroring the
//!   `entries` and `flavors` tables in DESIGN.md field for field so that
//!   `clippo-store` maps them rather than translating them.
//! - [`config`] — every knob DESIGN.md names, its documented default, and the
//!   TOML file it can be overridden from.
//! - [`paths`] — the one place that knows where clippo's config file and
//!   database live, so no other crate hardcodes either.
//!
//! Secret detection and masking, the other half of this crate's job, arrive at
//! M4; the config knobs they need ([`SecretsConfig`]) are already here.
//!
//! ```no_run
//! let config = clippo_core::Config::load()?;
//! println!("keeping {} entries in {}", config.max_entries, clippo_core::paths::db_path()?.display());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod config;
pub mod entry;
pub mod paths;

pub use config::{Config, ConfigError, SecretsConfig};
pub use entry::{Entry, EntryId, EntryKind, Flavor, NewEntry, ParseEntryKindError, Timestamp};
pub use paths::PathError;
