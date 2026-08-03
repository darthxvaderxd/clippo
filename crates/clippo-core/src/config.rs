//! clippo's configuration: the knobs, their documented defaults, and the TOML
//! file they can be overridden from.
//!
//! ```toml
//! # ~/.config/clippo/config.toml — every key is optional.
//! # Read once when clippod starts: run `systemctl --user restart clippod`
//! # after editing this file.
//! max_entries = 500
//! max_age_days = 30
//! max_image_bytes = 8388608
//! capture_primary = false
//! auto_paste = true
//! paste_shortcut = "Ctrl+V"
//! allow_privileged_members = true
//!
//! [secrets]
//! entropy_rule = true
//! mask_prefix = 2
//! mask_suffix = 2
//! ```
//!
//! Four rules the loader keeps, in order of how much trouble getting them wrong
//! would cause:
//!
//! 1. **A missing file is not a problem.** No file, no warning, defaults.
//! 2. **A file that is there but wrong is a hard error**, naming the path and
//!    what is wrong with it. Unknown keys count: a typo'd `max_entires` is a
//!    setting that silently does nothing, which is exactly the failure this
//!    guards against.
//! 3. **An explicit `0` is never quietly replaced by a default.** Absent and
//!    zero stay distinguishable all the way from the file to [`Config`]: every
//!    key parses as an `Option`, and each one either documents what its zero
//!    means or rejects it by name. See the table on each field below.
//! 4. **The file is read once, at startup.** There is no hot-reload and no file
//!    watching in v1, so editing `config.toml` while `clippod` is running has
//!    no effect until the daemon is restarted:
//!    `systemctl --user restart clippod`. That is a deliberate v1 scope
//!    boundary rather than an oversight — re-reading config mid-run would mean
//!    re-deriving retention, the watcher's primary-selection binding and the
//!    masking rules against a history that was captured under the old ones.

use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::chord::Chord;
use crate::paths;

/// Entries kept before the oldest unpinned ones are dropped. DESIGN.md,
/// `clippo-store` → "Retention".
pub const DEFAULT_MAX_ENTRIES: usize = 500;

/// Days an unpinned entry survives. DESIGN.md, `clippo-store` → "Retention".
pub const DEFAULT_MAX_AGE_DAYS: u32 = 30;

/// Largest image blob stored, 8 MB. DESIGN.md, `clippo-store` → "Images".
pub const DEFAULT_MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

/// Whether the middle-click primary selection is captured. DESIGN.md,
/// `clippo-wayland`: **off**.
pub const DEFAULT_CAPTURE_PRIMARY: bool = false;

/// The shortcut `Paste` synthesises once the entry is on the clipboard.
///
/// `Ctrl+V` because it is what almost everything pastes on. It is a config key
/// rather than a constant because the exception is a large one: most terminals
/// paste on `Ctrl+Shift+V` and treat `Ctrl+V` as something else entirely, so a
/// user who mostly pastes into a terminal needs the other one. There is one
/// shortcut for every application — see [`Config::paste_shortcut`].
pub const DEFAULT_PASTE_SHORTCUT: &str = "Ctrl+V";

/// Whether `Paste` presses the shortcut at all. **On.**
///
/// On because a picker that leaves you to press the key yourself is doing half
/// the job, and that half is the one the user is there for. Off is for people
/// who would rather clippo never synthesised input — see [`Config::auto_paste`],
/// which is a capability switch and not a preference about the applet.
pub const DEFAULT_AUTO_PASTE: bool = true;

/// Whether `Reveal` and `Paste` may be called over D-Bus at all. **On.**
///
/// On because turning it off costs the applet its `Ctrl+R` and its `Enter`, and
/// most users are not running the sandboxed applications this is about. See
/// [`Config::allow_privileged_members`] for what "privileged" means here and
/// why the two members are one switch.
pub const DEFAULT_ALLOW_PRIVILEGED_MEMBERS: bool = true;

/// Whether the entropy heuristic runs. DESIGN.md, "Known risks": on, with an
/// escape hatch.
pub const DEFAULT_ENTROPY_RULE: bool = true;

/// Leading characters left visible by `mask()`. DESIGN.md, `clippo-core` →
/// "Masking".
pub const DEFAULT_MASK_PREFIX: usize = 2;

/// Trailing characters left visible by `mask()`. DESIGN.md, `clippo-core` →
/// "Masking".
pub const DEFAULT_MASK_SUFFIX: usize = 2;

/// Ceiling on `max_entries`. Previews are held in memory for fuzzy search, so
/// this is a real resource bound and not just typo protection.
pub const MAX_ENTRIES_LIMIT: usize = 100_000;

/// Ceiling on `max_age_days`, a century. Anything longer is better expressed as
/// `0`, which means no age limit at all.
pub const MAX_AGE_DAYS_LIMIT: u32 = 36_500;

/// Ceiling on `max_image_bytes`, 1 GiB. Blobs are read into memory whole.
pub const MAX_IMAGE_BYTES_LIMIT: u64 = 1024 * 1024 * 1024;

/// Ceiling on `mask_prefix + mask_suffix`. Masking exists to keep a secret off
/// the screen; showing more than 16 characters of one would defeat it.
pub const MAX_MASK_CONTEXT: usize = 16;

/// Everything clippo can be configured to do.
///
/// [`Config::default`] is the documented default for every knob, and is exactly
/// what an empty or missing config file produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// How many entries to keep. Pinned entries are exempt.
    ///
    /// Must be at least 1: a zero-entry history is not a configuration, it is a
    /// stopped daemon, and `SetPaused` already expresses that. Rejected by
    /// name rather than silently turned back into 500.
    pub max_entries: usize,

    /// How many days an unpinned entry survives.
    ///
    /// **`0` means no age limit** — entries are then dropped only by
    /// `max_entries`. Use [`Config::max_age`] rather than reading this field,
    /// so the zero case cannot be forgotten at a call site.
    pub max_age_days: u32,

    /// Largest image blob stored, in bytes.
    ///
    /// **`0` means no image is ever small enough**, i.e. images are dropped.
    /// That falls out of the comparison naturally — a cap of zero admits
    /// nothing — so it needs no special case downstream.
    pub max_image_bytes: u64,

    /// Capture the middle-click primary selection as well as the clipboard.
    ///
    /// Off by default; feeds `clippo_wayland::WatchConfig::primary`.
    pub capture_primary: bool,

    /// Whether clippo may press keys for you at all.
    ///
    /// With this off, `Paste` copies and stops there: `Enter` in the picker
    /// puts the entry on the clipboard and you press your own paste key, which
    /// is what clippo did before it could do this. `clippo paste` is affected
    /// too, and deliberately — this is not "what `Enter` does", it is whether
    /// clippo is allowed to synthesise input at all, and a switch for that
    /// should not have an exception that types into a window anyway.
    ///
    /// Nothing else changes. The entry still reaches the clipboard, so every
    /// application's own paste key still works.
    pub auto_paste: bool,

    /// The shortcut `Paste` synthesises into the focused window.
    ///
    /// `Paste(id)` is `Copy(id)` plus this combination pressed for the user, so
    /// choosing an entry from the picker puts it where the cursor is instead of
    /// leaving them to press it themselves.
    ///
    /// **It is one shortcut for every application**, and applications disagree:
    /// the default `Ctrl+V` is wrong in most terminals, which want
    /// `Ctrl+Shift+V`. Set it to whichever you paste into most; the other one
    /// still works by hand, because `Paste` really does put the entry on the
    /// clipboard first. A shortcut clippo cannot read stops the daemon at
    /// startup naming the key, rather than silently pasting nothing.
    pub paste_shortcut: Chord,

    /// Whether `Reveal` and `Paste` may be called over D-Bus at all.
    ///
    /// Those two are the members that do more than list your own previews:
    /// `Reveal` returns a whole stored value, mask or no mask, and `Paste`
    /// types into whatever window has keyboard focus. **A session bus
    /// authenticates nobody**, so every process running as you can call both —
    /// including a sandboxed application that was denied the clipboard and
    /// keystroke-synthesis Wayland protocols directly, and which gets them back
    /// through clippo.
    ///
    /// With this off, both are refused with `AccessDenied` and nothing else
    /// changes: the history is intact, `List` and `Search` still answer, and
    /// `Copy` still puts an entry on the clipboard for you to paste yourself.
    /// The visible cost is that the picker's `Ctrl+R` and `clippo reveal` stop
    /// working, and `Enter` in the picker becomes a copy.
    ///
    /// **The daemon enforces it**, not the frontends — a knob a hostile caller
    /// could skip by not being the applet would not be one.
    ///
    /// This is a *different* switch from [`auto_paste`][Self::auto_paste],
    /// which is about whether clippo may synthesise input at all and leaves
    /// `Paste` succeeding as a copy. This one is about whether an unauthenticated
    /// peer may ask, and refuses the call outright.
    pub allow_privileged_members: bool,

    /// Secret detection and masking.
    pub secrets: SecretsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_entries: DEFAULT_MAX_ENTRIES,
            max_age_days: DEFAULT_MAX_AGE_DAYS,
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
            capture_primary: DEFAULT_CAPTURE_PRIMARY,
            auto_paste: DEFAULT_AUTO_PASTE,
            // Parsed rather than built by hand so that the default is held to
            // the same rule as a user's value: if `DEFAULT_PASTE_SHORTCUT` ever
            // stops being readable, every test fails rather than the daemon
            // quietly shipping a shortcut nobody wrote.
            paste_shortcut: DEFAULT_PASTE_SHORTCUT
                .parse()
                .expect("the default paste shortcut parses"),
            allow_privileged_members: DEFAULT_ALLOW_PRIVILEGED_MEMBERS,
            secrets: SecretsConfig::default(),
        }
    }
}

impl Config {
    /// Load from `$XDG_CONFIG_HOME/clippo/config.toml`, falling back to
    /// `~/.config/clippo/config.toml`.
    ///
    /// No file means defaults, silently. Any other problem — unreadable,
    /// unparseable, out of range — is an error naming the path.
    ///
    /// Call this once, at startup. The file is not watched and nothing
    /// re-reads it, so a running daemon must be restarted to pick up an edit;
    /// see the module docs.
    pub fn load() -> Result<Self, ConfigError> {
        Self::load_from(paths::config_file()?)
    }

    /// Load from a specific file, with the same rules as [`Config::load`].
    pub fn load_from(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text, path),
            // The one non-error failure: nothing configured, so defaults.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Parse config text. `path` is used only to name the file in errors.
    pub fn from_toml(text: &str, path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let raw: RawConfig = toml::from_str(text).map_err(|problem| ConfigError::Parse {
            path: path.to_path_buf(),
            problem,
        })?;
        raw.resolve().map_err(|message| ConfigError::Invalid {
            path: path.to_path_buf(),
            message,
        })
    }

    /// How old an unpinned entry may get, or `None` when there is no age limit.
    ///
    /// This is where `max_age_days = 0` is given its meaning, once, instead of
    /// at every retention call site.
    pub fn max_age(&self) -> Option<Duration> {
        match self.max_age_days {
            0 => None,
            days => Some(Duration::from_secs(u64::from(days) * 24 * 60 * 60)),
        }
    }
}

/// Secret detection and masking, DESIGN.md, `clippo-core`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretsConfig {
    /// Run the entropy heuristic.
    ///
    /// The escape hatch from DESIGN.md's risk table: turning this off keeps the
    /// MIME-hint and shape-regex rules, which have no false positives worth
    /// worrying about, and drops only the heuristic one.
    pub entropy_rule: bool,

    /// Leading characters `mask()` leaves visible.
    ///
    /// **`0` is a legitimate setting** and means show nothing at the front.
    /// `mask_prefix = 0` with `mask_suffix = 0` masks the value completely.
    pub mask_prefix: usize,

    /// Trailing characters `mask()` leaves visible. `0` behaves as for
    /// [`SecretsConfig::mask_prefix`].
    ///
    /// `mask_prefix + mask_suffix` may not exceed [`MAX_MASK_CONTEXT`]. A value
    /// shorter than the two together is masked entirely at render time — the
    /// mask never reveals a whole short secret just because the config asked
    /// for more characters than it has.
    pub mask_suffix: usize,
}

impl Default for SecretsConfig {
    fn default() -> Self {
        Self {
            entropy_rule: DEFAULT_ENTROPY_RULE,
            mask_prefix: DEFAULT_MASK_PREFIX,
            mask_suffix: DEFAULT_MASK_SUFFIX,
        }
    }
}

/// Why a config file could not be turned into a [`Config`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file is there but could not be read.
    #[error("could not read the clippo config file at {path}")]
    Read {
        /// The file clippo tried to read.
        path: PathBuf,
        /// Why it failed.
        #[source]
        source: std::io::Error,
    },

    /// The file is not valid TOML, or carries a key clippo does not know.
    ///
    /// The parse problem is part of this message rather than a `source`: the
    /// line and column are the whole point of the error, and a caller that
    /// prints only the top-level message must still get them.
    #[error("could not parse the clippo config file at {path}:\n{problem}")]
    Parse {
        /// The file that failed to parse.
        path: PathBuf,
        /// What `toml` made of it, including the offending line.
        problem: toml::de::Error,
    },

    /// The file parsed, but a value is out of range.
    #[error("invalid setting in the clippo config file at {path}: {message}")]
    Invalid {
        /// The file the setting came from.
        path: PathBuf,
        /// Which setting, what it was, and what was expected.
        message: String,
    },

    /// There is nowhere to look for a config file.
    #[error(transparent)]
    Path(#[from] paths::PathError),
}

/// The file exactly as written: every key optional, and every number kept as
/// the `i64` TOML gave us.
///
/// Nothing here has a default. That is the point — `Option::None` means "the
/// user did not say", which is a different thing from "the user said 0", and
/// the two must not be collapsed before [`RawConfig::resolve`] has had a look
/// at them. Negative numbers survive this far too, so they can be rejected with
/// a message about the key rather than a serde message about `usize`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    max_entries: Option<i64>,
    max_age_days: Option<i64>,
    max_image_bytes: Option<i64>,
    capture_primary: Option<bool>,
    auto_paste: Option<bool>,
    paste_shortcut: Option<String>,
    allow_privileged_members: Option<bool>,
    secrets: Option<Secrets>,
}

/// The `[secrets]` table as written.
///
/// Named for the table rather than `RawSecrets`, because serde puts the Rust
/// type name into its type-mismatch messages and `#[serde(rename)]` does not
/// change it: `secrets = 5` should read "expected struct Secrets", which is a
/// thing the user's file has, rather than naming an internal type it does not.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Secrets {
    entropy_rule: Option<bool>,
    mask_prefix: Option<i64>,
    mask_suffix: Option<i64>,
}

impl RawConfig {
    /// Fill in the defaults for what was absent and range-check what was not.
    fn resolve(self) -> Result<Config, String> {
        let secrets = self.secrets.unwrap_or_default();

        // One arm per knob, deliberately repetitive: the only way an explicit
        // value can be swallowed is if a `Some` arm forgets to use it, and that
        // is visible here.
        let max_entries = match self.max_entries {
            None => DEFAULT_MAX_ENTRIES,
            Some(value) => checked(
                "max_entries",
                value,
                1,
                MAX_ENTRIES_LIMIT as i64,
                "0 would keep no history at all — pause capture instead",
            )? as usize,
        };

        let max_age_days = match self.max_age_days {
            None => DEFAULT_MAX_AGE_DAYS,
            Some(value) => checked(
                "max_age_days",
                value,
                0,
                i64::from(MAX_AGE_DAYS_LIMIT),
                "0 means no age limit",
            )? as u32,
        };

        let max_image_bytes = match self.max_image_bytes {
            None => DEFAULT_MAX_IMAGE_BYTES,
            Some(value) => checked(
                "max_image_bytes",
                value,
                0,
                MAX_IMAGE_BYTES_LIMIT as i64,
                "0 means images are never stored",
            )? as u64,
        };

        let capture_primary = match self.capture_primary {
            None => DEFAULT_CAPTURE_PRIMARY,
            Some(value) => value,
        };

        let auto_paste = match self.auto_paste {
            None => DEFAULT_AUTO_PASTE,
            Some(value) => value,
        };

        let paste_shortcut = match self.paste_shortcut.as_deref() {
            None => DEFAULT_PASTE_SHORTCUT
                .parse()
                .expect("the default paste shortcut parses"),
            Some(value) => value.parse().map_err(|error| {
                format!("paste_shortcut is not a shortcut clippo can press: {error}")
            })?,
        };

        let allow_privileged_members = match self.allow_privileged_members {
            None => DEFAULT_ALLOW_PRIVILEGED_MEMBERS,
            Some(value) => value,
        };

        let entropy_rule = match secrets.entropy_rule {
            None => DEFAULT_ENTROPY_RULE,
            Some(value) => value,
        };

        let mask_prefix = match secrets.mask_prefix {
            None => DEFAULT_MASK_PREFIX,
            Some(value) => checked(
                "secrets.mask_prefix",
                value,
                0,
                MAX_MASK_CONTEXT as i64,
                "0 shows nothing at the front, which is allowed",
            )? as usize,
        };

        let mask_suffix = match secrets.mask_suffix {
            None => DEFAULT_MASK_SUFFIX,
            Some(value) => checked(
                "secrets.mask_suffix",
                value,
                0,
                MAX_MASK_CONTEXT as i64,
                "0 shows nothing at the end, which is allowed",
            )? as usize,
        };

        // The two halves are individually sane but only meaningful together:
        // enough visible characters and there is no secret left to hide.
        if mask_prefix + mask_suffix > MAX_MASK_CONTEXT {
            return Err(format!(
                "secrets.mask_prefix + secrets.mask_suffix = {} leaves too much of a secret \
                 visible (at most {MAX_MASK_CONTEXT} characters may be shown)",
                mask_prefix + mask_suffix
            ));
        }

        Ok(Config {
            max_entries,
            max_age_days,
            max_image_bytes,
            capture_primary,
            auto_paste,
            paste_shortcut,
            allow_privileged_members,
            secrets: SecretsConfig {
                entropy_rule,
                mask_prefix,
                mask_suffix,
            },
        })
    }
}

/// Range-check one value, naming the key and saying what was expected.
///
/// `hint` is appended only on failure, which is the only time it helps: it is
/// where each knob's zero either gets its meaning or is refused.
fn checked(key: &str, value: i64, min: i64, max: i64, hint: &str) -> Result<i64, String> {
    if value < min || value > max {
        return Err(format!(
            "{key} = {value} is out of range (expected {min} to {max}); {hint}"
        ));
    }
    Ok(value)
}

impl fmt::Display for Config {
    /// A one-line summary for the daemon's startup log.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "max_entries={}, max_age_days={} ({}), max_image_bytes={}, capture_primary={}, \
             auto_paste={}, paste_shortcut={}, allow_privileged_members={}, entropy_rule={}, \
             mask={}/{}",
            self.max_entries,
            self.max_age_days,
            if self.max_age().is_some() {
                "expiring"
            } else {
                "no age limit"
            },
            self.max_image_bytes,
            self.capture_primary,
            self.auto_paste,
            self.paste_shortcut,
            self.allow_privileged_members,
            self.secrets.entropy_rule,
            self.secrets.mask_prefix,
            self.secrets.mask_suffix,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file from the module docs, so the documented example stays valid.
    const DOCUMENTED_EXAMPLE: &str = "\
max_entries = 500
max_age_days = 30
max_image_bytes = 8388608
capture_primary = false

[secrets]
entropy_rule = true
mask_prefix = 2
mask_suffix = 2
";

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::from_toml(text, "/tmp/clippo-test/config.toml")
    }

    fn invalid_message(text: &str) -> String {
        match parse(text) {
            Err(error @ ConfigError::Invalid { .. }) => error.to_string(),
            other => panic!("expected an out-of-range error, got {other:?}"),
        }
    }

    #[test]
    fn the_defaults_are_the_ones_design_md_documents() {
        let config = Config::default();
        assert_eq!(config.max_entries, 500);
        assert_eq!(config.max_age_days, 30);
        assert_eq!(
            config.max_age(),
            Some(Duration::from_secs(30 * 24 * 60 * 60))
        );
        assert_eq!(config.max_image_bytes, 8 * 1024 * 1024);
        assert!(!config.capture_primary);
        assert!(config.secrets.entropy_rule);
        assert_eq!(config.secrets.mask_prefix, 2);
        assert_eq!(config.secrets.mask_suffix, 2);
    }

    #[test]
    fn a_missing_file_yields_defaults_and_is_not_an_error() {
        // An empty private directory, so "missing" is a fact about the path
        // rather than a bet that nobody else in /tmp picked the same name.
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join(paths::CONFIG_FILE_NAME);
        assert!(!missing.exists());
        assert_eq!(Config::load_from(&missing).unwrap(), Config::default());
    }

    #[test]
    fn an_empty_file_is_the_same_as_no_file() {
        assert_eq!(parse("").unwrap(), Config::default());
        assert_eq!(parse("[secrets]\n").unwrap(), Config::default());
    }

    #[test]
    fn the_documented_example_parses_to_the_defaults() {
        assert_eq!(parse(DOCUMENTED_EXAMPLE).unwrap(), Config::default());
    }

    #[test]
    fn setting_one_knob_does_not_require_restating_the_rest() {
        let config = parse("max_entries = 42\n").unwrap();
        assert_eq!(config.max_entries, 42);
        assert_eq!(config.max_age_days, DEFAULT_MAX_AGE_DAYS);
        assert_eq!(config.max_image_bytes, DEFAULT_MAX_IMAGE_BYTES);
        assert_eq!(config.secrets, SecretsConfig::default());

        // ... including a single key inside the nested table.
        let config = parse("[secrets]\nmask_prefix = 1\n").unwrap();
        assert_eq!(config.secrets.mask_prefix, 1);
        assert_eq!(config.secrets.mask_suffix, DEFAULT_MASK_SUFFIX);
        assert!(config.secrets.entropy_rule);
        assert_eq!(config.max_entries, DEFAULT_MAX_ENTRIES);
    }

    #[test]
    fn the_entropy_rule_can_be_turned_off_on_its_own() {
        let config = parse("[secrets]\nentropy_rule = false\n").unwrap();
        assert!(!config.secrets.entropy_rule);
        assert_eq!(config.secrets.mask_prefix, DEFAULT_MASK_PREFIX);
        assert_eq!(config.secrets.mask_suffix, DEFAULT_MASK_SUFFIX);
    }

    #[test]
    fn primary_capture_can_be_turned_on() {
        assert!(parse("capture_primary = true\n").unwrap().capture_primary);
        assert!(!parse("capture_primary = false\n").unwrap().capture_primary);
    }

    #[test]
    fn auto_paste_is_on_unless_it_is_turned_off() {
        assert!(parse("").unwrap().auto_paste);
        assert!(parse("auto_paste = true\n").unwrap().auto_paste);
        assert!(!parse("auto_paste = false\n").unwrap().auto_paste);
    }

    /// Turning the keystroke off leaves the shortcut readable rather than
    /// making it meaningless: the two keys are independent, and turning
    /// `auto_paste` back on should not require re-reading the other one.
    #[test]
    fn auto_paste_off_still_parses_the_shortcut() {
        let config = parse("auto_paste = false\npaste_shortcut = \"Ctrl+Shift+V\"\n").unwrap();
        assert!(!config.auto_paste);
        assert_eq!(config.paste_shortcut.to_string(), "Ctrl+Shift+V");
    }

    /// And an unreadable one is still an error when the keystroke is off. A
    /// config that would break on being switched back on is a config that is
    /// wrong now.
    #[test]
    fn auto_paste_off_does_not_excuse_an_unpressable_shortcut() {
        assert!(parse("auto_paste = false\npaste_shortcut = \"Ctrl+F13\"\n").is_err());
    }

    /// The knob a user who runs sandboxed applications reaches for. On unless
    /// they say otherwise, because turning it off costs the picker its reveal
    /// and its Enter.
    #[test]
    fn the_privileged_members_are_available_unless_they_are_turned_off() {
        assert!(parse("").unwrap().allow_privileged_members);
        assert!(
            parse("allow_privileged_members = true\n")
                .unwrap()
                .allow_privileged_members
        );
        assert!(
            !parse("allow_privileged_members = false\n")
                .unwrap()
                .allow_privileged_members
        );
    }

    /// The two switches are independent, and the daemon reads both: one is
    /// about whether clippo may synthesise input, the other about whether an
    /// unauthenticated peer may ask it to.
    #[test]
    fn the_two_paste_switches_do_not_shadow_each_other() {
        let config = parse("auto_paste = true\nallow_privileged_members = false\n").unwrap();
        assert!(config.auto_paste);
        assert!(!config.allow_privileged_members);

        let config = parse("auto_paste = false\nallow_privileged_members = true\n").unwrap();
        assert!(!config.auto_paste);
        assert!(config.allow_privileged_members);
    }

    #[test]
    fn the_paste_shortcut_defaults_to_the_one_almost_everything_uses() {
        assert_eq!(parse("").unwrap().paste_shortcut.to_string(), "Ctrl+V");
    }

    /// The case the key exists for: a user who mostly pastes into a terminal.
    #[test]
    fn the_paste_shortcut_can_be_a_terminals() {
        let config = parse("paste_shortcut = \"Ctrl+Shift+V\"\n").unwrap();
        assert_eq!(config.paste_shortcut.to_string(), "Ctrl+Shift+V");
    }

    /// Rule 2 of the module docs, for the one key whose value is not a number:
    /// a shortcut clippo cannot press must stop the daemon rather than become a
    /// `Paste` that copies and then does nothing visible.
    #[test]
    fn an_unpressable_paste_shortcut_is_an_error_naming_the_key() {
        let error = parse("paste_shortcut = \"Ctrl+F13\"\n").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("paste_shortcut"), "{message}");
        assert!(message.contains("F13"), "{message}");
    }

    #[test]
    fn a_paste_shortcut_with_no_key_is_an_error() {
        assert!(parse("paste_shortcut = \"Ctrl\"\n").is_err());
        assert!(parse("paste_shortcut = \"\"\n").is_err());
    }

    #[test]
    fn a_malformed_file_names_the_path_and_the_problem() {
        let error = match parse("max_entries = \n") {
            Err(error @ ConfigError::Parse { .. }) => error.to_string(),
            other => panic!("expected a parse error, got {other:?}"),
        };
        assert!(error.contains("/tmp/clippo-test/config.toml"), "{error}");
        // toml's own message points at the offending line.
        assert!(error.contains("max_entries"), "{error}");
    }

    #[test]
    fn a_wrongly_typed_value_is_a_parse_error_not_a_default() {
        let error = match parse("max_entries = \"lots\"\n") {
            Err(error @ ConfigError::Parse { .. }) => error.to_string(),
            other => panic!("expected a parse error, got {other:?}"),
        };
        assert!(error.contains("config.toml"), "{error}");
    }

    #[test]
    fn a_typoed_key_is_an_error_rather_than_a_setting_that_does_nothing() {
        let error = match parse("max_entires = 10\n") {
            Err(error @ ConfigError::Parse { .. }) => error.to_string(),
            other => panic!("expected a parse error, got {other:?}"),
        };
        assert!(error.contains("max_entires"), "{error}");
        assert!(error.contains("unknown field"), "{error}");
    }

    #[test]
    fn a_malformed_table_does_not_name_an_internal_type() {
        // The user wrote `secrets`, so that is what the error may talk about.
        let error = match parse("secrets = 5\n") {
            Err(error @ ConfigError::Parse { .. }) => error.to_string(),
            other => panic!("expected a parse error, got {other:?}"),
        };
        assert!(!error.to_lowercase().contains("raw"), "{error}");
        assert!(error.contains("Secrets"), "{error}");
        assert!(error.contains("config.toml"), "{error}");
    }

    #[test]
    fn out_of_range_values_are_rejected_by_name() {
        let error = invalid_message("max_entries = -1\n");
        assert!(error.contains("max_entries = -1"), "{error}");
        assert!(error.contains("config.toml"), "{error}");

        let error = invalid_message(&format!("max_entries = {}\n", MAX_ENTRIES_LIMIT + 1));
        assert!(error.contains("expected 1 to 100000"), "{error}");

        let error = invalid_message("max_age_days = -1\n");
        assert!(error.contains("max_age_days"), "{error}");

        let error = invalid_message(&format!("max_age_days = {}\n", MAX_AGE_DAYS_LIMIT + 1));
        assert!(error.contains("max_age_days"), "{error}");

        let error = invalid_message("max_image_bytes = -1\n");
        assert!(error.contains("max_image_bytes"), "{error}");

        let error = invalid_message(&format!(
            "max_image_bytes = {}\n",
            MAX_IMAGE_BYTES_LIMIT + 1
        ));
        assert!(error.contains("max_image_bytes"), "{error}");

        let error = invalid_message("[secrets]\nmask_prefix = -1\n");
        assert!(error.contains("secrets.mask_prefix"), "{error}");

        let error = invalid_message("[secrets]\nmask_suffix = 17\n");
        assert!(error.contains("secrets.mask_suffix"), "{error}");
    }

    #[test]
    fn a_mask_that_would_reveal_too_much_of_a_secret_is_rejected() {
        // Each half is in range; together they show more than a secret can spare.
        let error = invalid_message("[secrets]\nmask_prefix = 10\nmask_suffix = 10\n");
        assert!(
            error.contains("mask_prefix + secrets.mask_suffix = 20"),
            "{error}"
        );
        assert!(error.contains("at most 16"), "{error}");

        // Exactly at the limit is fine.
        let config = parse("[secrets]\nmask_prefix = 8\nmask_suffix = 8\n").unwrap();
        assert_eq!(config.secrets.mask_prefix, 8);
        assert_eq!(config.secrets.mask_suffix, 8);
    }

    #[test]
    fn an_explicit_zero_is_never_swallowed_by_a_default() {
        // max_age_days = 0: accepted, and means no age limit.
        let config = parse("max_age_days = 0\n").unwrap();
        assert_eq!(config.max_age_days, 0);
        assert_eq!(config.max_age(), None);

        // max_image_bytes = 0: accepted, and means no image ever fits.
        let config = parse("max_image_bytes = 0\n").unwrap();
        assert_eq!(config.max_image_bytes, 0);

        // mask lengths of 0: accepted, and mean mask the whole value.
        let config = parse("[secrets]\nmask_prefix = 0\nmask_suffix = 0\n").unwrap();
        assert_eq!(config.secrets.mask_prefix, 0);
        assert_eq!(config.secrets.mask_suffix, 0);

        // max_entries = 0: refused with a message, not turned back into 500.
        let error = invalid_message("max_entries = 0\n");
        assert!(error.contains("max_entries = 0"), "{error}");
        assert!(error.contains("no history at all"), "{error}");
    }

    #[test]
    fn a_zero_that_was_written_down_reads_differently_from_one_that_was_not() {
        // The distinction the loader exists to preserve, stated as a test:
        // absent means "use the default", 0 means "the user chose 0".
        assert_eq!(parse("").unwrap().max_age_days, DEFAULT_MAX_AGE_DAYS);
        assert_eq!(parse("max_age_days = 0\n").unwrap().max_age_days, 0);
        assert_eq!(parse("").unwrap().secrets.mask_prefix, DEFAULT_MASK_PREFIX);
        assert_eq!(
            parse("[secrets]\nmask_prefix = 0\n")
                .unwrap()
                .secrets
                .mask_prefix,
            0
        );
    }

    #[test]
    fn a_real_file_on_disk_loads_through_the_same_rules() {
        // The file path, not just the text: `load_from` is what `load` calls
        // once `paths` has told it where to look.
        // A fresh private directory, not a name another local account could
        // have pre-created as a symlink for us to write through. The guard
        // removes it on drop, including when an assertion below panics.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(paths::CONFIG_FILE_NAME);

        std::fs::write(&file, "max_entries = 7\n[secrets]\nentropy_rule = false\n").unwrap();
        let config = Config::load_from(&file).unwrap();

        assert_eq!(config.max_entries, 7);
        assert!(!config.secrets.entropy_rule);
        assert_eq!(config.max_age_days, DEFAULT_MAX_AGE_DAYS);
    }

    #[test]
    fn an_unreadable_file_is_an_error_naming_the_path() {
        // A directory where a file should be: readable path, unreadable file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let error = match Config::load_from(path) {
            Err(error @ ConfigError::Read { .. }) => error.to_string(),
            other => panic!("expected a read error, got {other:?}"),
        };
        assert!(error.contains(&path.display().to_string()), "{error}");
    }

    #[test]
    fn the_startup_summary_says_which_way_the_zeroes_went() {
        assert!(Config::default().to_string().contains("expiring"));
        let config = parse("max_age_days = 0\n").unwrap();
        assert!(config.to_string().contains("no age limit"), "{config}");
    }
}
