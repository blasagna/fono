// SPDX-License-Identifier: GPL-3.0-only
//! Translate a parsed Fono hotkey into the integer encoding Qt (and
//! therefore KDE's `KGlobalAccel`) uses on the wire.
//!
//! `org.kde.KGlobalAccel.setShortcutKeys` takes each key combination as
//! a single `int`: the `Qt::Key_*` code bitwise-OR'd with the
//! `Qt::KeyboardModifier` bits. Values are from `qnamespace.h` and are
//! ABI-stable across Qt 5 / 6. Sanity check against a live session:
//!
//! ```text
//! $ busctl --user call org.kde.kglobalaccel /component/org_kde_konsole \
//!     org.kde.kglobalaccel.Component allShortcutInfos
//! … "dictation" "Toggle voice dictation" … 1 201326624 0
//! ```
//!
//! `201326624` is `0x0C00_0020` — `Ctrl | Alt | Key_Space`.
//!
//! Parsing itself stays in [`crate::parse`], so a KDE binding is read
//! from `config.toml` exactly the way the X11 and portal backends read
//! it; this module only re-encodes the result.

use global_hotkey::hotkey::{Code, Modifiers};

use crate::parse::ParsedHotkey;

const SHIFT: i32 = 0x0200_0000;
const CONTROL: i32 = 0x0400_0000;
const ALT: i32 = 0x0800_0000;
/// `Qt::MetaModifier` — what X11 / Wayland call Super (the "Windows"
/// key). Fono's parser folds `super`/`meta`/`cmd`/`win` into
/// [`Modifiers::SUPER`].
const META: i32 = 0x1000_0000;

/// `Qt::Key_F1`. F1..F35 are contiguous from here.
const KEY_F1: i32 = 0x0100_0030;
const F_KEY_MAX: u32 = 35;

/// Encode `p` as a Qt key combination, or `None` when the key has no
/// `Qt::Key_*` equivalent we are confident about. Callers should warn
/// and skip that binding rather than register something wrong — a
/// mis-encoded shortcut would grab an unrelated key system-wide.
#[must_use]
pub fn to_qt_key(p: &ParsedHotkey) -> Option<i32> {
    let mut v = qt_key_code(p.code)?;
    if p.modifiers.contains(Modifiers::SHIFT) {
        v |= SHIFT;
    }
    if p.modifiers.contains(Modifiers::CONTROL) {
        v |= CONTROL;
    }
    if p.modifiers.contains(Modifiers::ALT) {
        v |= ALT;
    }
    if p.modifiers.intersects(Modifiers::SUPER | Modifiers::META) {
        v |= META;
    }
    Some(v)
}

/// The bare `Qt::Key_*` value for a physical key code.
fn qt_key_code(code: Code) -> Option<i32> {
    // Letters and digits are ASCII in Qt: Key_A = 0x41, Key_0 = 0x30.
    if let Some(c) = letter_or_digit(code) {
        return Some(c);
    }
    // Function keys are contiguous from Key_F1.
    if let Some(n) = function_key_number(code) {
        return (1..=F_KEY_MAX).contains(&n).then(|| KEY_F1 + (n as i32 - 1));
    }
    punctuation_or_named(code)
}

fn letter_or_digit(code: Code) -> Option<i32> {
    // `Code`'s Debug name is the W3C code ("KeyA", "Digit7"), which is
    // exactly the shape `crate::parse` builds them from. Matching on
    // the name keeps this table from spanning 36 explicit arms.
    let name = format!("{code:?}");
    if let Some(letter) = name.strip_prefix("Key") {
        let mut chars = letter.chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else { return None };
        if ch.is_ascii_uppercase() {
            return Some(ch as i32);
        }
        return None;
    }
    if let Some(digit) = name.strip_prefix("Digit") {
        let mut chars = digit.chars();
        let (Some(ch), None) = (chars.next(), chars.next()) else { return None };
        if ch.is_ascii_digit() {
            return Some(ch as i32);
        }
    }
    None
}

fn function_key_number(code: Code) -> Option<u32> {
    let name = format!("{code:?}");
    let rest = name.strip_prefix('F')?;
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

/// Keys whose Qt value is neither ASCII-alphanumeric nor a function
/// key. Deliberately limited to what [`crate::parse::parse_hotkey`] can
/// produce plus the obvious navigation keys — anything else returns
/// `None` so we fail loudly instead of grabbing the wrong key.
fn punctuation_or_named(code: Code) -> Option<i32> {
    Some(match code {
        Code::Space => 0x20,
        Code::Quote => 0x27,
        Code::Comma => 0x2c,
        Code::Minus => 0x2d,
        Code::Period => 0x2e,
        Code::Slash => 0x2f,
        Code::Semicolon => 0x3b,
        Code::Equal => 0x3d,
        Code::BracketLeft => 0x5b,
        Code::Backslash => 0x5c,
        Code::BracketRight => 0x5d,
        Code::Backquote => 0x60,
        Code::Escape => 0x0100_0000,
        Code::Tab => 0x0100_0001,
        Code::Backspace => 0x0100_0003,
        Code::Enter => 0x0100_0004,
        Code::Insert => 0x0100_0006,
        Code::Delete => 0x0100_0007,
        Code::Pause => 0x0100_0008,
        Code::PrintScreen => 0x0100_0009,
        Code::Home => 0x0100_0010,
        Code::End => 0x0100_0011,
        Code::ArrowLeft => 0x0100_0012,
        Code::ArrowUp => 0x0100_0013,
        Code::ArrowRight => 0x0100_0014,
        Code::ArrowDown => 0x0100_0015,
        Code::PageUp => 0x0100_0016,
        Code::PageDown => 0x0100_0017,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_hotkey;

    fn qt(s: &str) -> Option<i32> {
        to_qt_key(&parse_hotkey(s).unwrap())
    }

    /// The one value pinned against a real Plasma session (see the
    /// module docs): Ctrl+Alt+Space as KWin stored it.
    #[test]
    fn matches_value_observed_on_a_live_kwin_session() {
        assert_eq!(qt("Ctrl+Alt+Space"), Some(201_326_624));
        assert_eq!(qt("Ctrl+Alt+Space"), Some(0x0C00_0020));
    }

    #[test]
    fn function_keys_are_contiguous_from_f1() {
        assert_eq!(qt("F1"), Some(0x0100_0030));
        assert_eq!(qt("F7"), Some(0x0100_0036));
        assert_eq!(qt("F8"), Some(0x0100_0037));
        assert_eq!(qt("F12"), Some(0x0100_003b));
    }

    #[test]
    fn bare_and_modified_named_keys() {
        assert_eq!(qt("Escape"), Some(0x0100_0000));
        assert_eq!(qt("Esc"), Some(0x0100_0000));
        assert_eq!(qt("Ctrl+Alt+Grave"), Some(CONTROL | ALT | 0x60));
    }

    #[test]
    fn letters_and_digits_are_ascii() {
        assert_eq!(qt("A"), Some(0x41));
        assert_eq!(qt("z"), Some(0x5a));
        assert_eq!(qt("Super+7"), Some(META | 0x37));
    }

    #[test]
    fn every_modifier_maps_to_its_qt_bit() {
        assert_eq!(qt("Shift+F5"), Some(SHIFT | 0x0100_0034));
        assert_eq!(qt("Ctrl+F5"), Some(CONTROL | 0x0100_0034));
        assert_eq!(qt("Alt+F5"), Some(ALT | 0x0100_0034));
        assert_eq!(qt("Super+F5"), Some(META | 0x0100_0034));
        assert_eq!(qt("Meta+F5"), Some(META | 0x0100_0034));
    }

    /// Keys we have no confident Qt value for must be rejected rather
    /// than guessed — registering the wrong code would grab an
    /// unrelated key across the whole session.
    #[test]
    fn unmapped_keys_are_rejected() {
        for code in [Code::Numpad5, Code::CapsLock, Code::ContextMenu, Code::IntlBackslash] {
            assert_eq!(to_qt_key(&ParsedHotkey { modifiers: Modifiers::empty(), code }), None);
        }
    }
}
