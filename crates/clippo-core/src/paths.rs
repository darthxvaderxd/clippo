//! Where clippo keeps its things.
//!
//! Every path clippo touches is named here once. No other crate builds
//! `~/.local/share/clippo/...` by hand — when the layout changes, or when a
//! test needs to redirect it, there is one place to change.
//!
//! Resolution follows the XDG Base Directory spec: `$XDG_CONFIG_HOME` and
//! `$XDG_DATA_HOME` win when they hold an absolute path, otherwise `$HOME`
//! supplies the usual `~/.config` and `~/.local/share`. A relative or empty XDG
//! value is ignored rather than honoured, which is what the spec requires and
//! what stops a stray `XDG_DATA_HOME=` from scattering databases in the working
//! directory.
//!
//! This module answers *where*, not *what is there*: nothing here creates a
//! directory or touches a file. Creating the data directory — and the `0700`
//! decision that goes with it, which exists for the fallback key file — belongs
//! with `clippo-store` at M2b, next to the key handling it protects.

use std::ffi::OsString;
use std::path::PathBuf;

/// The directory clippo owns inside each XDG base directory.
pub const APP_DIR: &str = "clippo";

/// The config file's name inside [`config_dir`].
pub const CONFIG_FILE_NAME: &str = "config.toml";

/// The history database's name inside [`data_dir`].
pub const DB_FILE_NAME: &str = "history.db";

/// Nothing in the environment says where the user's home is.
#[derive(Debug, thiserror::Error)]
pub enum PathError {
    /// Neither the XDG variable nor `$HOME` gave an absolute path.
    #[error(
        "neither ${xdg_var} nor $HOME is set to an absolute path, \
         so clippo cannot work out where to keep its {what}"
    )]
    NoHome {
        /// The XDG variable that was consulted first.
        xdg_var: &'static str,
        /// What clippo was trying to place, for the message.
        what: &'static str,
    },
}

/// `$XDG_CONFIG_HOME/clippo`, else `~/.config/clippo`.
pub fn config_dir() -> Result<PathBuf, PathError> {
    resolve("XDG_CONFIG_HOME", ".config", "configuration")
}

/// The config file: `<config_dir>/config.toml`.
pub fn config_file() -> Result<PathBuf, PathError> {
    Ok(config_dir()?.join(CONFIG_FILE_NAME))
}

/// `$XDG_DATA_HOME/clippo`, else `~/.local/share/clippo`.
pub fn data_dir() -> Result<PathBuf, PathError> {
    resolve("XDG_DATA_HOME", ".local/share", "history")
}

/// The encrypted history database: `<data_dir>/history.db`.
pub fn db_path() -> Result<PathBuf, PathError> {
    Ok(data_dir()?.join(DB_FILE_NAME))
}

fn resolve(
    xdg_var: &'static str,
    home_relative: &str,
    what: &'static str,
) -> Result<PathBuf, PathError> {
    resolve_from(
        std::env::var_os(xdg_var),
        std::env::var_os("HOME"),
        home_relative,
    )
    .ok_or(PathError::NoHome { xdg_var, what })
}

/// The resolution rule itself, with the environment passed in so it can be
/// tested without mutating the process's environment.
fn resolve_from(
    xdg: Option<OsString>,
    home: Option<OsString>,
    home_relative: &str,
) -> Option<PathBuf> {
    let base = xdg
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(home_relative))
        })?;
    Some(base.join(APP_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn xdg_wins_when_it_is_absolute() {
        assert_eq!(
            resolve_from(os("/xdg/data"), os("/home/ada"), ".local/share"),
            Some(PathBuf::from("/xdg/data/clippo"))
        );
    }

    #[test]
    fn home_supplies_the_default_layout() {
        assert_eq!(
            resolve_from(None, os("/home/ada"), ".local/share"),
            Some(PathBuf::from("/home/ada/.local/share/clippo"))
        );
        assert_eq!(
            resolve_from(None, os("/home/ada"), ".config"),
            Some(PathBuf::from("/home/ada/.config/clippo"))
        );
    }

    #[test]
    fn an_unset_or_relative_xdg_value_falls_back_to_home() {
        for xdg in [None, os(""), os("relative/path")] {
            assert_eq!(
                resolve_from(xdg, os("/home/ada"), ".config"),
                Some(PathBuf::from("/home/ada/.config/clippo"))
            );
        }
    }

    #[test]
    fn nothing_usable_in_the_environment_is_an_error() {
        assert_eq!(resolve_from(None, None, ".config"), None);
        assert_eq!(resolve_from(os(""), os("relative"), ".config"), None);

        let error = PathError::NoHome {
            xdg_var: "XDG_DATA_HOME",
            what: "history",
        }
        .to_string();
        assert!(error.contains("$XDG_DATA_HOME"), "{error}");
        assert!(error.contains("$HOME"), "{error}");
    }

    #[test]
    fn file_names_hang_off_their_directories() {
        // Exercised through the same rule the public helpers use, so this pins
        // the layout without depending on the test runner's environment.
        let data = resolve_from(os("/xdg/data"), None, ".local/share").unwrap();
        assert_eq!(
            data.join(DB_FILE_NAME),
            PathBuf::from("/xdg/data/clippo/history.db")
        );

        let config = resolve_from(os("/xdg/config"), None, ".config").unwrap();
        assert_eq!(
            config.join(CONFIG_FILE_NAME),
            PathBuf::from("/xdg/config/clippo/config.toml")
        );
    }
}
