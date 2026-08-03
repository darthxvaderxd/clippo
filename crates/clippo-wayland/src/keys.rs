//! Pressing a key combination for the user, through
//! `zwp_virtual_keyboard_v1`.
//!
//! One caller: `Paste(id)`, which is `Copy(id)` and then this. Putting an entry
//! on the clipboard is only ever half of what the user wanted — the other half
//! is it appearing where their cursor is — and Wayland gives a client no way to
//! write into another application's surface. Synthesising the keystroke the
//! user would have pressed is the way across that line, and it is deliberately
//! a privileged protocol: a compositor that offers it is offering the ability
//! to type into any window.
//!
//! # Why the keymap is `us` and not the user's
//!
//! The compositor interprets our keycodes with *our* keymap, not the one the
//! user is typing on. That is what makes this correct rather than a bug waiting
//! for someone on Dvorak: keycode 47 means `v` because the keymap uploaded here
//! says so, whatever the physical layout would have made of the same code. A
//! shortcut written `Ctrl+V` therefore arrives as `Ctrl+V` everywhere.
//!
//! # Why the modifier is pressed and not only announced
//!
//! `zwp_virtual_keyboard_v1` has both a `modifiers` request and ordinary
//! `key` presses, and only sending the former does not work: the compositor
//! recomputes modifier state from the keys it believes are down, so the
//! announcement is overwritten by the very next keypress and the application
//! sees an unmodified `v`. This sends both — announce, hold the real modifier
//! key, press, then unwind — which is what every other tool doing this does,
//! and what the difference between a working and a silent paste turned out to
//! be.

use std::fmt;
use std::io::Write;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

use clippo_core::Chord;

/// The interface name, spelled once, for the failure message.
pub const VIRTUAL_KEYBOARD_PROTOCOL: &str = "zwp_virtual_keyboard_v1";

/// `wl_keyboard` keymap format 1: an XKB keymap as text.
const KEYMAP_FORMAT_XKB_V1: u32 = 1;

/// How long a synthesised key is held down.
///
/// Not zero: press and release in the same millisecond is a keystroke some
/// applications drop, and this is short enough that nobody sees it. It is also
/// why [`Keystrokes::send`] is documented as blocking.
const HOLD: Duration = Duration::from_millis(12);

/// Pressing a key combination into whatever currently has keyboard focus.
///
/// A trait for the same reason [`crate::Clipboard`] is one: `clippod` can then
/// be tested without a compositor, and the daemon's `Paste` is exercised
/// against a recording double rather than against the user's actual desktop.
pub trait Keystrokes: fmt::Debug + Send + Sync {
    /// Press and release `chord`, modifiers and all.
    ///
    /// **Blocks** for [`HOLD`] plus a round trip, so call it off the async
    /// runtime's worker threads.
    ///
    /// Whatever has keyboard focus *at the moment this runs* receives it, which
    /// is the caller's problem and not this one's: there is no way to address a
    /// particular window, and no way to ask which one will be focused next.
    fn send(&self, chord: &Chord) -> Result<(), KeyError>;
}

/// Why a keystroke could not be sent.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// No connection to a compositor.
    #[error("clippo could not connect to the wayland compositor to press a key: {0}")]
    Connect(String),

    /// The compositor does not offer the protocol.
    #[error(
        "this compositor does not offer {VIRTUAL_KEYBOARD_PROTOCOL}, so clippo cannot press \
         the paste shortcut for you; the entry is still on the clipboard and can be pasted \
         by hand"
    )]
    Unsupported,

    /// There is no seat to attach a keyboard to.
    #[error("this compositor offers no seat for clippo to press a key on")]
    NoSeat,

    /// The keymap could not be compiled or handed over.
    #[error("clippo could not build the keymap it presses keys with: {0}")]
    Keymap(String),

    /// The connection broke while sending.
    #[error("clippo lost its wayland connection while pressing a key: {0}")]
    Send(String),
}

/// Nothing here has events worth handling — a virtual keyboard is write-only,
/// and the registry is read once at startup — so the dispatch state is empty
/// and every `Dispatch` is a no-op.
struct State;

impl Dispatch<WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore WlSeat);
delegate_noop!(State: ignore ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: ignore ZwpVirtualKeyboardV1);

/// The queue and the keyboard, which have to be used together and neither of
/// which is `Sync` on its own.
struct Inner {
    queue: EventQueue<State>,
    keyboard: ZwpVirtualKeyboardV1,
}

/// A real virtual keyboard on the compositor.
///
/// Holds its own Wayland connection rather than sharing the watcher's. The two
/// have nothing to say to each other — one reads selections, the other writes
/// keys — and a second connection costs a socket, while threading a keyboard
/// through [`crate::watch`]'s event loop would put a write path inside the loop
/// whose job is not to block.
pub struct VirtualKeyboard {
    /// Kept alive because dropping it closes the socket and destroys the
    /// keyboard with it.
    _connection: Connection,
    inner: Mutex<Inner>,
    /// The base for the protocol's timestamps, which must share one.
    started: Instant,
}

impl fmt::Debug for VirtualKeyboard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualKeyboard").finish_non_exhaustive()
    }
}

impl VirtualKeyboard {
    /// Connect, bind a keyboard and give it a keymap.
    ///
    /// Done once at startup rather than per keystroke: the keymap is the
    /// expensive part — a compiled `us` layout is some 60 kB of text that the
    /// compositor then parses — and a `Paste` should not pay for it.
    ///
    /// Failing here is not fatal to the daemon. A compositor without the
    /// protocol still supports every other member; only `Paste` degrades, and
    /// it degrades to `Copy`, which is what the user gets today.
    pub fn new() -> Result<Self, KeyError> {
        let connection =
            Connection::connect_to_env().map_err(|error| KeyError::Connect(error.to_string()))?;
        let (globals, queue) = registry_queue_init::<State>(&connection)
            .map_err(|error| KeyError::Connect(error.to_string()))?;
        let qh = queue.handle();

        let seat: WlSeat = globals.bind(&qh, 1..=9, ()).map_err(|_| KeyError::NoSeat)?;
        let manager: ZwpVirtualKeyboardManagerV1 = globals
            .bind(&qh, 1..=1, ())
            .map_err(|_| KeyError::Unsupported)?;

        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());

        let (keymap, size) = keymap()?;
        keyboard.keymap(KEYMAP_FORMAT_XKB_V1, keymap.as_fd(), size);

        let mut inner = Inner { queue, keyboard };
        inner
            .queue
            .roundtrip(&mut State)
            .map_err(|error| KeyError::Keymap(error.to_string()))?;

        Ok(Self {
            _connection: connection,
            inner: Mutex::new(inner),
            started: Instant::now(),
        })
    }
}

impl Keystrokes for VirtualKeyboard {
    fn send(&self, chord: &Chord) -> Result<(), KeyError> {
        // Poisoning would mean a previous `send` panicked mid-chord, which
        // would have left modifiers down. Taking the lock anyway and running
        // the full press-and-release is what puts them back up.
        let mut inner = match self.inner.lock() {
            Ok(inner) => inner,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Inner { queue, keyboard } = &mut *inner;

        let at = || self.started.elapsed().as_millis() as u32;
        let modifiers = chord.modifier_keys();

        keyboard.modifiers(chord.modifier_mask(), 0, 0, 0);
        for key in &modifiers {
            keyboard.key(at(), *key, 1);
        }
        keyboard.key(at(), chord.key, 1);
        queue
            .flush()
            .map_err(|error| KeyError::Send(error.to_string()))?;

        std::thread::sleep(HOLD);

        keyboard.key(at(), chord.key, 0);
        // Reverse order, so the modifier state the compositor computes on the
        // way down is the mirror of the way up.
        for key in modifiers.iter().rev() {
            keyboard.key(at(), *key, 0);
        }
        keyboard.modifiers(0, 0, 0, 0);

        // A round trip rather than a flush: it is what makes sure the whole
        // chord reached the compositor before this returns, so a caller that
        // shuts down straight afterwards does not take the socket away with the
        // release events still in it — which would leave modifiers stuck down
        // on the user's desktop.
        queue
            .roundtrip(&mut State)
            .map_err(|error| KeyError::Send(error.to_string()))?;
        Ok(())
    }
}

/// Compile the `us` keymap and put it in an anonymous file for the compositor.
///
/// A `memfd` rather than a temporary file: the keymap never needs a name, and a
/// daemon that writes one into `/tmp` on every start is a daemon that leaves
/// them there after a crash.
fn keymap() -> Result<(rustix::fd::OwnedFd, u32), KeyError> {
    use rustix::fs::{MemfdFlags, SealFlags};

    let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
    let keymap = xkbcommon::xkb::Keymap::new_from_names(
        &context,
        "",
        "",
        "us",
        "",
        None,
        xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .ok_or_else(|| KeyError::Keymap("libxkbcommon would not compile the us layout".to_owned()))?;
    let text = keymap.get_as_string(xkbcommon::xkb::KEYMAP_FORMAT_TEXT_V1);

    let fd = rustix::fs::memfd_create(
        "clippo-keymap",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )
    .map_err(|error| KeyError::Keymap(error.to_string()))?;
    let mut file = std::fs::File::from(fd);
    file.write_all(text.as_bytes())
        .map_err(|error| KeyError::Keymap(error.to_string()))?;
    // The compositor reads the keymap as a NUL-terminated string and the size
    // it is given has to count the terminator.
    file.write_all(&[0])
        .map_err(|error| KeyError::Keymap(error.to_string()))?;
    file.flush()
        .map_err(|error| KeyError::Keymap(error.to_string()))?;
    let size = text.len() as u32 + 1;

    let fd = rustix::fd::OwnedFd::from(file);
    // Sealed so the compositor can map it without having to defend against the
    // size changing under it. Not every compositor requires this; the ones that
    // do, require exactly this.
    let _ = rustix::fs::fcntl_add_seals(
        &fd,
        SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE | SealFlags::SEAL,
    );
    Ok((fd, size))
}

use rustix::fd::AsFd;

#[cfg(test)]
mod tests {
    use super::*;

    /// The keymap is the one thing here that can be checked without a
    /// compositor, and it is worth checking: a keymap that will not compile
    /// makes every `Paste` fail at the last step, having already copied.
    #[test]
    fn the_keymap_compiles_and_is_nul_terminated() {
        let (fd, size) = keymap().expect("the us keymap compiles");
        assert!(size > 1, "a keymap of {size} bytes is not a keymap");

        let mut file = std::fs::File::from(fd);
        let mut text = Vec::new();
        use std::io::{Read, Seek};
        file.rewind().expect("rewind");
        file.read_to_end(&mut text).expect("read back");

        assert_eq!(text.len(), size as usize, "the size must count every byte");
        assert_eq!(text.last(), Some(&0), "the compositor reads a C string");
        let text = String::from_utf8_lossy(&text);
        assert!(text.contains("xkb_keymap"), "not an xkb keymap");
    }

    /// The `v` in `Ctrl+V` has to survive as `v`, whatever the user's own
    /// layout is, because the keymap the compositor reads our codes with is the
    /// one above rather than theirs.
    #[test]
    fn the_keymap_maps_the_paste_key_to_v() {
        let context = xkbcommon::xkb::Context::new(xkbcommon::xkb::CONTEXT_NO_FLAGS);
        let keymap = xkbcommon::xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us",
            "",
            None,
            xkbcommon::xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("compiles");
        let state = xkbcommon::xkb::State::new(&keymap);

        let chord: Chord = "Ctrl+V".parse().expect("parses");
        // XKB numbers keycodes eight higher than the kernel does, which is the
        // offset every Wayland keyboard carries.
        let symbol = state.key_get_one_sym((chord.key + 8).into());
        assert_eq!(xkbcommon::xkb::keysym_get_name(symbol), "v");
    }
}
