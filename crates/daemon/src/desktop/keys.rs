//! DOM key names to X keysyms.
//!
//! A browser speaks two languages for one keystroke: `KeyboardEvent.code`
//! is the physical position (`KeyW`, layout-independent) and
//! `KeyboardEvent.key` is what it produced (`w`, or `W` under shift). X11
//! speaks keysyms, and XTEST speaks the keycodes a keysym lives on — so
//! replay maps the DOM name to a keysym here and the display's keymap
//! resolves it to a keycode there.
//!
//! [`code_to_keysym`] covers the physical positions; [`key_to_keysym`]
//! is the fallback for anything position can't name — mostly the named
//! keys a synthetic event reports only by product.

/// A code suffix that names exactly one character, or nothing — the
/// families below are validated as one rather than trusted.
fn single_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let ch = chars.next()?;
    chars.next().is_none().then_some(ch)
}

/// The X keysym for a DOM `KeyboardEvent.code`.
///
/// Keysyms are from X11's `keysymdef.h`: letters and digits are their
/// lowercase forms (shift is a separate keystroke the DOM reports as its
/// own event), the `0xff..` range is the named-key space.
pub fn code_to_keysym(code: &str) -> Option<u32> {
    // The positional families spell their keysym: `KeyW` is `w` whether
    // or not shift was held, `Digit7` is `7`, `Numpad7` is the numpad's
    // own digit row, and `F7` is the seventh function keysym.
    if let Some(letter) = code.strip_prefix("Key").and_then(single_char)
        && letter.is_ascii_alphabetic()
    {
        return Some(u32::from(letter.to_ascii_lowercase()));
    }
    if let Some(digit) = code.strip_prefix("Digit").and_then(single_char)
        && digit.is_ascii_digit()
    {
        return Some(u32::from(digit));
    }
    if let Some(digit) = code.strip_prefix("Numpad").and_then(single_char)
        && digit.is_ascii_digit()
    {
        return Some(0xffb0 + u32::from(digit) - u32::from('0'));
    }
    if let Some(function) = code
        .strip_prefix("F")
        .and_then(|rest| rest.parse::<u32>().ok())
        .filter(|n| (1..=12).contains(n))
    {
        return Some(0xffbe + function - 1);
    }
    Some(match code {
        "Enter" => 0xff0d,
        "Escape" => 0xff1b,
        "Backspace" => 0xff08,
        "Tab" => 0xff09,
        "Space" => 0x20,
        "Minus" => 0x2d,
        "Equal" => 0x3d,
        "BracketLeft" => 0x5b,
        "BracketRight" => 0x5d,
        "Backslash" => 0x5c,
        "Semicolon" => 0x3b,
        "Quote" => 0x27,
        "Backquote" => 0x60,
        "Comma" => 0x2c,
        "Period" => 0x2e,
        "Slash" => 0x2f,
        "CapsLock" => 0xffe5,
        "PrintScreen" => 0xff61,
        "ScrollLock" => 0xff14,
        "Pause" => 0xff13,
        "Insert" => 0xff63,
        "Home" => 0xff50,
        "PageUp" => 0xff55,
        "Delete" => 0xffff,
        "End" => 0xff57,
        "PageDown" => 0xff56,
        "ArrowRight" => 0xff53,
        "ArrowLeft" => 0xff51,
        "ArrowDown" => 0xff54,
        "ArrowUp" => 0xff52,
        "NumLock" => 0xff7f,
        "NumpadDivide" => 0xffaf,
        "NumpadMultiply" => 0xffaa,
        "NumpadSubtract" => 0xffad,
        "NumpadAdd" => 0xffab,
        "NumpadEnter" => 0xff8d,
        "NumpadDecimal" => 0xffae,
        "ContextMenu" => 0xff67,
        "ShiftLeft" => 0xffe1,
        "ShiftRight" => 0xffe2,
        "ControlLeft" => 0xffe3,
        "ControlRight" => 0xffe4,
        "AltLeft" => 0xffe9,
        "AltRight" | "AltGraph" => 0xfe03,
        "MetaLeft" => 0xffe7,
        "MetaRight" => 0xffe8,
        "IntlBackslash" => 0x3c,
        "IntlRo" => 0xff27,
        "IntlYen" => 0xa5,
        _ => return None,
    })
}

/// The X keysym for a DOM `KeyboardEvent.key` — the fallback when the
/// physical position is unmapped or the event named only its product.
///
/// A single character is its own keysym below U+0100 and takes the
/// Unicode keysym range (`0x01000000 | codepoint`) above it; the named
/// keys cover what a synthetic event reports by name alone.
pub fn key_to_keysym(key: &str) -> Option<u32> {
    if let Some(ch) = key.chars().next().filter(|_| key.chars().count() == 1) {
        let point = u32::from(ch);
        return Some(if point < 0x100 {
            point
        } else {
            0x0100_0000 | point
        });
    }
    Some(match key {
        "Enter" => 0xff0d,
        "Escape" => 0xff1b,
        "Backspace" => 0xff08,
        "Tab" => 0xff09,
        "CapsLock" => 0xffe5,
        "Shift" => 0xffe1,
        "Control" => 0xffe3,
        "Alt" => 0xffe9,
        "Meta" | "OS" => 0xffe7,
        "Delete" => 0xffff,
        "Insert" => 0xff63,
        "Home" => 0xff50,
        "End" => 0xff57,
        "PageUp" => 0xff55,
        "PageDown" => 0xff56,
        "ArrowUp" => 0xff52,
        "ArrowDown" => 0xff54,
        "ArrowLeft" => 0xff51,
        "ArrowRight" => 0xff53,
        "F1" => 0xffbe,
        "F2" => 0xffbf,
        "F3" => 0xffc0,
        "F4" => 0xffc1,
        "F5" => 0xffc2,
        "F6" => 0xffc3,
        "F7" => 0xffc4,
        "F8" => 0xffc5,
        "F9" => 0xffc6,
        "F10" => 0xffc7,
        "F11" => 0xffc8,
        "F12" => 0xffc9,
        "ContextMenu" | "Menu" => 0xff67,
        "ScrollLock" => 0xff14,
        "Pause" => 0xff13,
        "PrintScreen" => 0xff61,
        "NumLock" => 0xff7f,
        "Dead" => 0xfe50,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{code_to_keysym, key_to_keysym};

    #[test]
    fn physical_codes_cover_the_letter_row() {
        assert_eq!(code_to_keysym("KeyW"), Some(0x77));
        assert_eq!(code_to_keysym("Digit5"), Some(0x35));
        assert_eq!(code_to_keysym("ShiftLeft"), Some(0xffe1));
        assert_eq!(code_to_keysym("F5"), Some(0xffc2));
        assert_eq!(code_to_keysym("DoesNotExist"), None);
    }

    #[test]
    fn the_produced_key_is_the_fallback() {
        assert_eq!(key_to_keysym("w"), Some(0x77));
        assert_eq!(key_to_keysym("W"), Some(0x57));
        assert_eq!(key_to_keysym("Enter"), Some(0xff0d));
        // A non-Latin-1 character takes the Unicode keysym range.
        assert_eq!(key_to_keysym("é"), Some(0xe9));
        assert_eq!(key_to_keysym("中"), Some(0x0100_0000 | 0x4e2d));
        assert_eq!(key_to_keysym("Unidentified"), None);
    }
}
