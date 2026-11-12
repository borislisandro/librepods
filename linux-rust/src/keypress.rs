//! Presses keyboard keys through a virtual keyboard (/dev/uinput), so an
//! AirPods gesture can trigger another app's keybind (e.g. Discord's mute).
//! Works on Wayland and X11 alike: the key arrives like one from a real keyboard.

use log::{error, info};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Mutex;
use std::time::Duration;

/// Keys that can be pressed: name (as the UI toolkit names the physical key,
/// and as stored in settings) and Linux key code. Codes are key positions, so
/// a captured key comes out the same under any keyboard layout.
const KEYS: &[(&str, u16)] = &[
    ("KeyA", 30), ("KeyB", 48), ("KeyC", 46), ("KeyD", 32), ("KeyE", 18), ("KeyF", 33),
    ("KeyG", 34), ("KeyH", 35), ("KeyI", 23), ("KeyJ", 36), ("KeyK", 37), ("KeyL", 38),
    ("KeyM", 50), ("KeyN", 49), ("KeyO", 24), ("KeyP", 25), ("KeyQ", 16), ("KeyR", 19),
    ("KeyS", 31), ("KeyT", 20), ("KeyU", 22), ("KeyV", 47), ("KeyW", 17), ("KeyX", 45),
    ("KeyY", 21), ("KeyZ", 44),
    ("Digit1", 2), ("Digit2", 3), ("Digit3", 4), ("Digit4", 5), ("Digit5", 6),
    ("Digit6", 7), ("Digit7", 8), ("Digit8", 9), ("Digit9", 10), ("Digit0", 11),
    ("F1", 59), ("F2", 60), ("F3", 61), ("F4", 62), ("F5", 63), ("F6", 64), ("F7", 65),
    ("F8", 66), ("F9", 67), ("F10", 68), ("F11", 87), ("F12", 88), ("F13", 183),
    ("F14", 184), ("F15", 185), ("F16", 186), ("F17", 187), ("F18", 188), ("F19", 189),
    ("F20", 190), ("F21", 191), ("F22", 192), ("F23", 193), ("F24", 194),
    ("Minus", 12), ("Equal", 13), ("Backspace", 14), ("Tab", 15), ("BracketLeft", 26),
    ("BracketRight", 27), ("Enter", 28), ("Semicolon", 39), ("Quote", 40),
    ("Backquote", 41), ("Backslash", 43), ("Comma", 51), ("Period", 52), ("Slash", 53),
    ("Space", 57), ("CapsLock", 58), ("IntlBackslash", 86),
    ("PrintScreen", 99), ("ScrollLock", 70), ("Pause", 119), ("Insert", 110),
    ("Delete", 111), ("Home", 102), ("End", 107), ("PageUp", 104), ("PageDown", 109),
    ("ArrowUp", 103), ("ArrowDown", 108), ("ArrowLeft", 105), ("ArrowRight", 106),
    ("ContextMenu", 127),
    ("NumLock", 69), ("Numpad0", 82), ("Numpad1", 79), ("Numpad2", 80), ("Numpad3", 81),
    ("Numpad4", 75), ("Numpad5", 76), ("Numpad6", 77), ("Numpad7", 71), ("Numpad8", 72),
    ("Numpad9", 73), ("NumpadAdd", 78), ("NumpadSubtract", 74), ("NumpadMultiply", 55),
    ("NumpadDivide", 98), ("NumpadDecimal", 83), ("NumpadEnter", 96),
    ("AudioVolumeMute", 113), ("AudioVolumeDown", 114), ("AudioVolumeUp", 115),
    ("MediaPlayPause", 164), ("MediaStop", 166), ("MediaTrackPrevious", 165),
    ("MediaTrackNext", 163),
];

const KEY_LEFTCTRL: u16 = 29;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_LEFTALT: u16 = 56;
const KEY_LEFTMETA: u16 = 125;

/// A key with optional modifiers, written "Ctrl+Shift+PageDown" in settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyCombo {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
    name: &'static str,
    code: u16,
}

impl KeyCombo {
    /// `key` is a name from KEYS; None if it isn't a key we can press.
    pub fn new(key: &str, ctrl: bool, shift: bool, alt: bool, meta: bool) -> Option<Self> {
        let &(name, code) = KEYS.iter().find(|(n, _)| n.eq_ignore_ascii_case(key))?;
        Some(Self {
            ctrl,
            shift,
            alt,
            meta,
            name,
            code,
        })
    }

    pub fn parse(text: &str) -> Option<Self> {
        let mut parts: Vec<&str> = text.split('+').map(str::trim).collect();
        let key = parts.pop()?;
        let (mut ctrl, mut shift, mut alt, mut meta) = (false, false, false, false);
        for part in parts {
            match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => ctrl = true,
                "shift" => shift = true,
                "alt" => alt = true,
                "meta" | "super" => meta = true,
                _ => return None,
            }
        }
        Self::new(key, ctrl, shift, alt, meta)
    }

    fn modifiers(&self) -> impl Iterator<Item = (&'static str, u16)> {
        [
            (self.ctrl, "Ctrl", KEY_LEFTCTRL),
            (self.shift, "Shift", KEY_LEFTSHIFT),
            (self.alt, "Alt", KEY_LEFTALT),
            (self.meta, "Meta", KEY_LEFTMETA),
        ]
        .into_iter()
        .filter_map(|(on, name, code)| on.then_some((name, code)))
    }

    /// For display: "Ctrl + Shift + Page Down".
    pub fn label(&self) -> String {
        let name = self
            .name
            .strip_prefix("Key")
            .or_else(|| self.name.strip_prefix("Digit"))
            .unwrap_or(self.name);
        // "PageDown" -> "Page Down"
        let mut key = String::new();
        let mut previous_lower = false;
        for c in name.chars() {
            if c.is_ascii_uppercase() && previous_lower {
                key.push(' ');
            }
            previous_lower = c.is_ascii_lowercase();
            key.push(c);
        }
        self.modifiers()
            .map(|(name, _)| name.to_string())
            .chain([key])
            .collect::<Vec<_>>()
            .join(" + ")
    }
}

/// The settings form, which parse() reads back.
impl std::fmt::Display for KeyCombo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (name, _) in self.modifiers() {
            write!(f, "{name}+")?;
        }
        write!(f, "{}", self.name)
    }
}

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const BUS_VIRTUAL: u16 = 0x06;
// <linux/uinput.h>
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;

#[repr(C)]
struct UinputSetup {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
    name: [u8; 80],
    ff_effects_max: u32,
}

/// The virtual keyboard, created on first use and kept for the process's life
/// (the kernel removes it when the file closes).
static KEYBOARD: Mutex<Option<File>> = Mutex::new(None);

fn create_keyboard() -> std::io::Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/uinput")?;
    let fd = file.as_raw_fd();
    let check = |ret: libc::c_int| {
        if ret < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    let mut setup = UinputSetup {
        bustype: BUS_VIRTUAL,
        vendor: 0x1d6b,
        product: 0x4c50,
        version: 1,
        name: [0; 80],
        ff_effects_max: 0,
    };
    let name = b"LibrePods virtual keyboard";
    setup.name[..name.len()].copy_from_slice(name);
    // SAFETY: fd is an open uinput device; the ioctls take an int or a pointer
    // to a uinput_setup, which UinputSetup matches.
    unsafe {
        check(libc::ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_int))?;
        let modifiers = [KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_LEFTALT, KEY_LEFTMETA];
        for code in KEYS.iter().map(|&(_, code)| code).chain(modifiers) {
            check(libc::ioctl(fd, UI_SET_KEYBIT, code as libc::c_int))?;
        }
        check(libc::ioctl(fd, UI_DEV_SETUP, &setup as *const UinputSetup))?;
        check(libc::ioctl(fd, UI_DEV_CREATE))?;
    }
    Ok(file)
}

fn write_event(file: &mut File, type_: u16, code: u16, value: i32) -> std::io::Result<()> {
    let event = libc::input_event {
        time: libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        type_,
        code,
        value,
    };
    // SAFETY: input_event is plain data; the slice covers exactly that struct.
    let bytes = unsafe {
        std::slice::from_raw_parts(
            &event as *const libc::input_event as *const u8,
            std::mem::size_of::<libc::input_event>(),
        )
    };
    file.write_all(bytes)
}

/// Create the virtual keyboard ahead of the first press. The desktop takes a
/// moment to pick up a new keyboard, and ignores keys sent before it has.
pub fn prepare() -> bool {
    let mut keyboard = KEYBOARD.lock().unwrap_or_else(|e| e.into_inner());
    if keyboard.is_some() {
        return true;
    }
    match create_keyboard() {
        Ok(file) => {
            info!("Created the virtual keyboard for gesture key presses");
            *keyboard = Some(file);
            // Let the desktop register the new keyboard before anything is typed.
            std::thread::sleep(Duration::from_millis(500));
            true
        }
        Err(e) => {
            error!(
                "Cannot create a virtual keyboard through /dev/uinput ({e}); \
                 gesture key presses won't work. Your user needs write access to /dev/uinput."
            );
            false
        }
    }
}

/// Press and release the key or shortcut written as `combo` (see
/// KeyCombo::parse). Blocks briefly.
pub fn press(combo: &str) {
    let Some(key) = KeyCombo::parse(combo) else {
        error!("Unknown key '{combo}' for a gesture key press");
        return;
    };
    if !prepare() {
        return;
    }
    let mut keyboard = KEYBOARD.lock().unwrap_or_else(|e| e.into_inner());
    let Some(file) = keyboard.as_mut() else {
        return;
    };
    let modifiers: Vec<u16> = key.modifiers().map(|(_, code)| code).collect();
    let result = (|| {
        for &code in &modifiers {
            write_event(file, EV_KEY, code, 1)?;
        }
        write_event(file, EV_KEY, key.code, 1)?;
        write_event(file, EV_SYN, 0, 0)?;
        // Held long enough for apps that poll the keyboard to notice.
        std::thread::sleep(Duration::from_millis(40));
        write_event(file, EV_KEY, key.code, 0)?;
        for &code in modifiers.iter().rev() {
            write_event(file, EV_KEY, code, 0)?;
        }
        write_event(file, EV_SYN, 0, 0)
    })();
    match result {
        Ok(()) => info!("Pressed {key} through the virtual keyboard"),
        Err(e) => {
            error!("Failed to press {key}: {e}; recreating the virtual keyboard next time");
            *keyboard = None;
        }
    }
}

#[cfg(test)]
mod tests {
    /// Needs write access to /dev/uinput. Presses F24, which nothing binds.
    #[test]
    fn parses_and_formats_combos() {
        let combo = super::KeyCombo::parse("ctrl+Shift+pagedown").unwrap();
        assert_eq!(combo.to_string(), "Ctrl+Shift+PageDown");
        assert_eq!(combo.label(), "Ctrl + Shift + Page Down");
        assert_eq!(super::KeyCombo::parse("KeyM").unwrap().label(), "M");
        assert_eq!(super::KeyCombo::parse("F13").unwrap().label(), "F13");
        assert!(super::KeyCombo::parse("Hyper+KeyM").is_none());
        assert!(super::KeyCombo::parse("NoSuchKey").is_none());
    }

    #[test]
    #[ignore]
    fn presses_f24() {
        assert!(super::prepare());
        super::press("Ctrl+F24");
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}
