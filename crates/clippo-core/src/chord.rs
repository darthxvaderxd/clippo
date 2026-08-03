//! A key combination, written the way a user writes it and stored the way the
//! kernel numbers it.
//!
//! One thing needs this: the shortcut `clippod` synthesises to make the focused
//! application paste, after `Paste` has put an entry on the clipboard. The
//! combination has to be configurable because there is no single right answer —
//! `Ctrl+V` almost everywhere, `Ctrl+Shift+V` in most terminals, `Shift+Insert`
//! in some older ones — and only the user knows which application they are
//! about to paste into.
//!
//! # Why the keycodes are here and not in `clippo-wayland`
//!
//! Because [`config`][crate::config] promises that a wrong key is an error
//! naming the key rather than a setting that silently does nothing, and a
//! shortcut naming a key that does not exist is exactly that failure. Checking
//! it needs the table, so the table lives where the checking happens. The
//! numbers are the kernel's own — `linux/input-event-codes.h`, the `KEY_*`
//! constants — which is what both the Wayland keyboard protocols and
//! `zwp_virtual_keyboard_v1` carry.
//!
//! # What is deliberately not here
//!
//! Every key on a keyboard. This is the set a paste shortcut is plausibly built
//! from — letters, digits and `Insert` — and nothing else, because every name
//! accepted here is a name that has to keep working. A user who needs `F13` can
//! ask for it; a table copied wholesale out of a header could not be taken back.

use std::fmt;

/// `KEY_LEFTCTRL`.
pub const KEY_LEFTCTRL: u32 = 29;
/// `KEY_LEFTSHIFT`.
pub const KEY_LEFTSHIFT: u32 = 42;
/// `KEY_LEFTALT`.
pub const KEY_LEFTALT: u32 = 56;
/// `KEY_LEFTMETA`, the Super key.
pub const KEY_LEFTMETA: u32 = 125;

/// The keys a shortcut may name, and the kernel code each one is.
///
/// Ordered as a keyboard is rather than alphabetically, so that a missing key is
/// visible as a gap in a row.
const KEYS: &[(&str, u32)] = &[
    ("1", 2),
    ("2", 3),
    ("3", 4),
    ("4", 5),
    ("5", 6),
    ("6", 7),
    ("7", 8),
    ("8", 9),
    ("9", 10),
    ("0", 11),
    ("q", 16),
    ("w", 17),
    ("e", 18),
    ("r", 19),
    ("t", 20),
    ("y", 21),
    ("u", 22),
    ("i", 23),
    ("o", 24),
    ("p", 25),
    ("a", 30),
    ("s", 31),
    ("d", 32),
    ("f", 33),
    ("g", 34),
    ("h", 35),
    ("j", 36),
    ("k", 37),
    ("l", 38),
    ("z", 44),
    ("x", 45),
    ("c", 46),
    ("v", 47),
    ("b", 48),
    ("n", 49),
    ("m", 50),
    ("insert", 110),
];

/// A key combination: some modifiers, and exactly one key.
///
/// Parse one with [`str::parse`]. [`Display`][fmt::Display] writes it back in
/// the canonical spelling, which is what the daemon logs, so a user who wrote
/// `control+shift+v` sees `Ctrl+Shift+V` and can tell it was understood.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    /// Hold Control.
    pub ctrl: bool,
    /// Hold Shift.
    pub shift: bool,
    /// Hold Alt.
    pub alt: bool,
    /// Hold Super.
    pub logo: bool,
    /// The key pressed while those are held, as a `KEY_*` code.
    pub key: u32,
    /// The key's name, kept for [`Display`][fmt::Display].
    name: &'static str,
}

impl Chord {
    /// The modifier keys to hold down, as `KEY_*` codes, in press order.
    ///
    /// Sending the modifier as a *key* is not the same as announcing it in a
    /// `modifiers` event, and it is the part that actually works: a compositor
    /// recomputes the modifier state from the keys it is told are down, so a
    /// bare announcement is overwritten by the very next keypress. Callers
    /// should do both — announce, then hold — and release in reverse.
    pub fn modifier_keys(&self) -> Vec<u32> {
        let mut keys = Vec::new();
        if self.ctrl {
            keys.push(KEY_LEFTCTRL);
        }
        if self.alt {
            keys.push(KEY_LEFTALT);
        }
        if self.logo {
            keys.push(KEY_LEFTMETA);
        }
        if self.shift {
            keys.push(KEY_LEFTSHIFT);
        }
        keys
    }

    /// The XKB modifier mask for the held modifiers.
    ///
    /// The indices are fixed by the standard set every ordinary keymap declares
    /// in the same order — Shift, Lock, Control, Mod1..Mod5 — so `Shift` is bit
    /// 0 and `Control` is bit 2 in any keymap a compositor will have compiled.
    pub fn modifier_mask(&self) -> u32 {
        let mut mask = 0;
        if self.shift {
            mask |= 1 << 0;
        }
        if self.ctrl {
            mask |= 1 << 2;
        }
        if self.alt {
            mask |= 1 << 3;
        }
        if self.logo {
            mask |= 1 << 6;
        }
        mask
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (held, label) in [
            (self.ctrl, "Ctrl"),
            (self.alt, "Alt"),
            (self.logo, "Super"),
            (self.shift, "Shift"),
        ] {
            if held {
                write!(f, "{label}+")?;
            }
        }
        // Upper-cased because that is how a shortcut is written everywhere it
        // is written for a person to read, including in this project's own
        // documentation.
        write!(f, "{}", self.name.to_uppercase())
    }
}

/// Why a shortcut could not be read.
///
/// Every variant names what was wrong and, where there is one, what would have
/// been right: this reaches the user as the reason `clippod` refused to start,
/// and a message they cannot act on wastes the only chance to say it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseChordError {
    /// The string was empty or nothing but separators.
    Empty,
    /// A part before the last was not a modifier this understands.
    UnknownModifier(String),
    /// The last part was not a key this understands.
    UnknownKey(String),
    /// The same modifier was written more than once.
    RepeatedModifier(String),
    /// There were modifiers but no key to press with them.
    NoKey,
}

impl fmt::Display for ParseChordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "a shortcut cannot be empty"),
            Self::UnknownModifier(part) => write!(
                f,
                "{part:?} is not a modifier; expected one of Ctrl, Shift, Alt or Super"
            ),
            Self::UnknownKey(part) => write!(
                f,
                "{part:?} is not a key clippo can press; expected a letter, a digit, or Insert"
            ),
            Self::RepeatedModifier(part) => write!(f, "{part:?} is given twice"),
            Self::NoKey => write!(f, "a shortcut needs a key to press, not only modifiers"),
        }
    }
}

impl std::error::Error for ParseChordError {}

impl std::str::FromStr for Chord {
    type Err = ParseChordError;

    /// Read `Ctrl+Shift+V`, or any casing and spacing of it.
    ///
    /// The last `+`-separated part is the key and everything before it is a
    /// modifier, which is the rule every other program writing these strings
    /// uses. `Control` and `Ctrl` are the same thing, as are `Super`, `Meta`
    /// and `Logo`, because a user who knows one name should not have to guess
    /// which one this parser learned.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = text
            .split('+')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        let Some((key, modifiers)) = parts.split_last() else {
            return Err(ParseChordError::Empty);
        };

        let mut chord = Chord {
            ctrl: false,
            shift: false,
            alt: false,
            logo: false,
            key: 0,
            name: "",
        };

        for part in modifiers {
            let slot = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => &mut chord.ctrl,
                "shift" => &mut chord.shift,
                "alt" | "option" => &mut chord.alt,
                "super" | "meta" | "logo" | "win" => &mut chord.logo,
                _ => return Err(ParseChordError::UnknownModifier((*part).to_owned())),
            };
            if *slot {
                return Err(ParseChordError::RepeatedModifier((*part).to_owned()));
            }
            *slot = true;
        }

        // A trailing `+` — "Ctrl+" — leaves a modifier as the last part rather
        // than a key, and "needs a key" is a better answer than "Ctrl is not a
        // key", which is true but sounds like the modifier was the mistake.
        let lowered = key.to_ascii_lowercase();
        if matches!(
            lowered.as_str(),
            "ctrl" | "control" | "shift" | "alt" | "option" | "super" | "meta" | "logo" | "win"
        ) {
            return Err(ParseChordError::NoKey);
        }

        let Some((name, code)) = KEYS.iter().find(|(name, _)| *name == lowered) else {
            return Err(ParseChordError::UnknownKey((*key).to_owned()));
        };
        chord.key = *code;
        chord.name = name;
        Ok(chord)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(text: &str) -> Chord {
        text.parse().expect("parses")
    }

    #[test]
    fn the_ordinary_paste_shortcut_reads() {
        let ctrl_v = chord("Ctrl+V");
        assert!(ctrl_v.ctrl);
        assert!(!ctrl_v.shift);
        assert_eq!(ctrl_v.key, 47);
    }

    #[test]
    fn a_terminals_paste_shortcut_reads() {
        let chord = chord("Ctrl+Shift+V");
        assert!(chord.ctrl && chord.shift);
        assert_eq!(chord.key, 47);
        assert_eq!(chord.modifier_keys(), vec![KEY_LEFTCTRL, KEY_LEFTSHIFT]);
    }

    #[test]
    fn shift_insert_reads() {
        let chord = chord("Shift+Insert");
        assert!(chord.shift && !chord.ctrl);
        assert_eq!(chord.key, 110);
    }

    /// A user should not have to guess which spelling this parser learned.
    #[test]
    fn casing_spacing_and_synonyms_are_all_the_same_chord() {
        let canonical = chord("Ctrl+Shift+V");
        for spelling in [
            "ctrl+shift+v",
            "CTRL+SHIFT+V",
            " Ctrl + Shift + V ",
            "control+shift+v",
            "Shift+Ctrl+V",
        ] {
            assert_eq!(chord(spelling), canonical, "{spelling:?}");
        }
    }

    #[test]
    fn every_super_synonym_is_the_same_modifier() {
        for spelling in ["Super+V", "Meta+V", "Logo+V", "Win+V"] {
            assert!(chord(spelling).logo, "{spelling:?}");
        }
    }

    /// The canonical spelling is what the daemon logs, so a user can see that
    /// what they wrote was understood as what they meant.
    #[test]
    fn display_is_the_canonical_spelling() {
        assert_eq!(chord("ctrl+v").to_string(), "Ctrl+V");
        assert_eq!(chord("shift+ctrl+v").to_string(), "Ctrl+Shift+V");
        assert_eq!(chord("shift+insert").to_string(), "Shift+INSERT");
    }

    #[test]
    fn a_bare_key_needs_no_modifiers() {
        let chord = chord("v");
        assert!(!chord.ctrl && !chord.shift && !chord.alt && !chord.logo);
        assert_eq!(chord.modifier_keys(), Vec::<u32>::new());
        assert_eq!(chord.modifier_mask(), 0);
    }

    #[test]
    fn the_modifier_mask_matches_the_standard_indices() {
        assert_eq!(chord("Shift+V").modifier_mask(), 1);
        assert_eq!(chord("Ctrl+V").modifier_mask(), 4);
        assert_eq!(chord("Ctrl+Shift+V").modifier_mask(), 5);
    }

    /// Each of these is a way a config file is actually wrong, and each answer
    /// has to be actionable — see [`ParseChordError`].
    #[test]
    fn a_wrong_shortcut_says_what_is_wrong_with_it() {
        assert_eq!("".parse::<Chord>(), Err(ParseChordError::Empty));
        assert_eq!("+".parse::<Chord>(), Err(ParseChordError::Empty));
        assert_eq!("Ctrl+".parse::<Chord>(), Err(ParseChordError::NoKey));
        assert_eq!("Ctrl".parse::<Chord>(), Err(ParseChordError::NoKey));
        assert_eq!(
            "Hyper+V".parse::<Chord>(),
            Err(ParseChordError::UnknownModifier("Hyper".to_owned()))
        );
        assert_eq!(
            "Ctrl+F13".parse::<Chord>(),
            Err(ParseChordError::UnknownKey("F13".to_owned()))
        );
        assert_eq!(
            "Ctrl+Ctrl+V".parse::<Chord>(),
            Err(ParseChordError::RepeatedModifier("Ctrl".to_owned()))
        );
    }

    /// The message is the whole value of rejecting these, so it has to name the
    /// offending part and say what was expected.
    #[test]
    fn the_message_names_the_part_and_the_alternative() {
        let message = "Ctrl+F13".parse::<Chord>().unwrap_err().to_string();
        assert!(message.contains("F13"), "{message}");
        assert!(message.contains("Insert"), "{message}");
    }

    #[test]
    fn no_key_is_named_twice_in_the_table() {
        let mut names: Vec<&str> = KEYS.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        let mut unique = names.clone();
        unique.dedup();
        assert_eq!(names, unique, "a key name is in the table twice");
    }

    #[test]
    fn every_name_in_the_table_parses_to_its_own_code() {
        for (name, code) in KEYS {
            assert_eq!(chord(name).key, *code, "{name}");
        }
    }
}
