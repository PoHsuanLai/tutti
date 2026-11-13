//! Editor key events at the ABI edge: a [`KeyEvent`] as the three values
//! `effEditKeyDown` and `effEditKeyUp` carry.

use keyboard_types::{Code, Key as KeyboardKey, Modifiers, NamedKey};
use tutti_plugin_types::{KeyEvent, ModifierConvention};
use tutti_vst2_ffi::api::ModifierKey;
use tutti_vst2_ffi::editor::{Key, KeyCode as Vst2KeyCode};

/// The key as `effEditKeyDown`'s three arguments, or `None` when VST 2 cannot write it: a
/// character outside ASCII or a string of several (`[index]` is one ASCII character), or a
/// held modifier the host's platform has no bit for.
///
/// A key typed with a character travels as that character; a named key types none, and
/// travels as `'\0'`. The virtual key is the key's name where it has one, and for a
/// character key the one the character or the numeric keypad position gives
/// ([`virtual_key_of`]); a key with none is [`Key::None`] beside its character.
///
/// `convention` is [`ModifierConvention::Mac`] on macOS, where the SDK numbers the bits by
/// role: `kVstModifierControl` (8) is the Command key, and `kVstModifierCommand` (4) the
/// Control key. Under [`ModifierConvention::Pc`] bit 8 is Control and no bit is the Windows
/// key. Lock modifiers and AltGraph change what a key typed, which `key` already says, and
/// carry no bit.
pub(crate) fn key_code_of(event: &KeyEvent, convention: ModifierConvention) -> Option<Vst2KeyCode> {
    let character = match &event.key {
        KeyboardKey::Character(typed) => {
            let mut chars = typed.chars();
            match (chars.next(), chars.next()) {
                (Some(character), None) if character.is_ascii() => character,
                _ => return None,
            }
        }
        KeyboardKey::Named(_) => '\0',
    };
    let apple = convention == ModifierConvention::Mac;
    let mut modifier = ModifierKey::empty();
    for (held, bit) in [
        (
            event.modifiers.contains(Modifiers::SHIFT),
            Some(ModifierKey::SHIFT),
        ),
        (
            event.modifiers.contains(Modifiers::ALT),
            Some(ModifierKey::ALT),
        ),
        (
            event.modifiers.contains(Modifiers::CONTROL),
            Some(if apple {
                ModifierKey::COMMAND
            } else {
                ModifierKey::CONTROL
            }),
        ),
        (event.meta_held(), apple.then_some(ModifierKey::CONTROL)),
    ] {
        if held {
            modifier |= bit?;
        }
    }
    Some(Vst2KeyCode {
        character,
        key: virtual_key_of(&event.key, event.code).unwrap_or(Key::None),
        modifier: modifier.bits(),
    })
}

/// The `VKEY_*` a key is: a named key by its name, a character key by the space or `=` it
/// typed, and a numeric keypad key by its position, which `code` alone tells from the main
/// block's. `None` for a key VST 2 has no code for.
fn virtual_key_of(key: &KeyboardKey, code: Code) -> Option<Key> {
    Some(match key {
        KeyboardKey::Named(named) => match named {
            NamedKey::Backspace => Key::Back,
            NamedKey::Tab => Key::Tab,
            NamedKey::Clear => Key::Clear,
            NamedKey::Enter if code == Code::NumpadEnter => Key::Enter,
            NamedKey::Enter => Key::Return,
            NamedKey::Pause => Key::Pause,
            NamedKey::Escape => Key::Escape,
            NamedKey::End => Key::End,
            NamedKey::Home => Key::Home,
            NamedKey::ArrowLeft => Key::Left,
            NamedKey::ArrowUp => Key::Up,
            NamedKey::ArrowRight => Key::Right,
            NamedKey::ArrowDown => Key::Down,
            NamedKey::PageUp => Key::PageUp,
            NamedKey::PageDown => Key::PageDown,
            NamedKey::Select => Key::Select,
            NamedKey::Print => Key::Print,
            NamedKey::PrintScreen => Key::Snapshot,
            NamedKey::Insert => Key::Insert,
            NamedKey::Delete => Key::Delete,
            NamedKey::Help => Key::Help,
            NamedKey::F1 => Key::F1,
            NamedKey::F2 => Key::F2,
            NamedKey::F3 => Key::F3,
            NamedKey::F4 => Key::F4,
            NamedKey::F5 => Key::F5,
            NamedKey::F6 => Key::F6,
            NamedKey::F7 => Key::F7,
            NamedKey::F8 => Key::F8,
            NamedKey::F9 => Key::F9,
            NamedKey::F10 => Key::F10,
            NamedKey::F11 => Key::F11,
            NamedKey::F12 => Key::F12,
            NamedKey::NumLock => Key::Numlock,
            NamedKey::ScrollLock => Key::Scroll,
            NamedKey::Shift => Key::Shift,
            NamedKey::Control => Key::Control,
            NamedKey::Alt => Key::Alt,
            _ => return None,
        },
        KeyboardKey::Character(typed) => match (typed.as_str(), code) {
            (" ", _) => Key::Space,
            ("=", _) => Key::Equals,
            (_, Code::Numpad0) => Key::Numpad0,
            (_, Code::Numpad1) => Key::Numpad1,
            (_, Code::Numpad2) => Key::Numpad2,
            (_, Code::Numpad3) => Key::Numpad3,
            (_, Code::Numpad4) => Key::Numpad4,
            (_, Code::Numpad5) => Key::Numpad5,
            (_, Code::Numpad6) => Key::Numpad6,
            (_, Code::Numpad7) => Key::Numpad7,
            (_, Code::Numpad8) => Key::Numpad8,
            (_, Code::Numpad9) => Key::Numpad9,
            (_, Code::NumpadMultiply) => Key::Multiply,
            (_, Code::NumpadAdd) => Key::Add,
            (_, Code::NumpadComma) => Key::Separator,
            (_, Code::NumpadSubtract) => Key::Subtract,
            (_, Code::NumpadDecimal) => Key::Decimal,
            (_, Code::NumpadDivide) => Key::Divide,
            _ => return None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use keyboard_types::KeyState;

    fn event(key: KeyboardKey, code: Code, modifiers: Modifiers) -> KeyEvent {
        KeyEvent {
            state: KeyState::Down,
            key,
            code,
            modifiers,
        }
    }

    fn character(c: &str) -> KeyboardKey {
        KeyboardKey::Character(c.into())
    }

    fn vkey(key: KeyboardKey, code: Code) -> isize {
        let code = key_code_of(
            &event(key, code, Modifiers::empty()),
            ModifierConvention::Pc,
        )
        .expect("expressible");
        isize::from(code.key)
    }

    /// The keys are the SDK's `VKEY_*` numbers, written as literals: a transposed arm of
    /// `virtual_key_of` fails here though a test through it would agree with it.
    #[test]
    fn keys_are_the_sdks_virtual_key_numbers() {
        let named = |n| KeyboardKey::Named(n);
        let sdk = [
            (named(NamedKey::Backspace), Code::Backspace, 1),
            (named(NamedKey::Tab), Code::Tab, 2),
            (named(NamedKey::Clear), Code::NumpadClear, 3),
            (named(NamedKey::Enter), Code::Enter, 4),
            (named(NamedKey::Pause), Code::Pause, 5),
            (named(NamedKey::Escape), Code::Escape, 6),
            (character(" "), Code::Space, 7),
            (named(NamedKey::End), Code::End, 9),
            (named(NamedKey::Home), Code::Home, 10),
            (named(NamedKey::ArrowLeft), Code::ArrowLeft, 11),
            (named(NamedKey::ArrowUp), Code::ArrowUp, 12),
            (named(NamedKey::ArrowRight), Code::ArrowRight, 13),
            (named(NamedKey::ArrowDown), Code::ArrowDown, 14),
            (named(NamedKey::PageUp), Code::PageUp, 15),
            (named(NamedKey::PageDown), Code::PageDown, 16),
            (named(NamedKey::Select), Code::Unidentified, 17),
            (named(NamedKey::Print), Code::Unidentified, 18),
            (named(NamedKey::Enter), Code::NumpadEnter, 19),
            (named(NamedKey::PrintScreen), Code::PrintScreen, 20),
            (named(NamedKey::Insert), Code::Insert, 21),
            (named(NamedKey::Delete), Code::Delete, 22),
            (named(NamedKey::Help), Code::Help, 23),
            (character("0"), Code::Numpad0, 24),
            (character("9"), Code::Numpad9, 33),
            (character("*"), Code::NumpadMultiply, 34),
            (character("+"), Code::NumpadAdd, 35),
            (character(","), Code::NumpadComma, 36),
            (character("-"), Code::NumpadSubtract, 37),
            (character("."), Code::NumpadDecimal, 38),
            (character("/"), Code::NumpadDivide, 39),
            (named(NamedKey::F1), Code::F1, 40),
            (named(NamedKey::F12), Code::F12, 51),
            (named(NamedKey::NumLock), Code::NumLock, 52),
            (named(NamedKey::ScrollLock), Code::ScrollLock, 53),
            (named(NamedKey::Shift), Code::ShiftLeft, 54),
            (named(NamedKey::Control), Code::ControlLeft, 55),
            (named(NamedKey::Alt), Code::AltLeft, 56),
            (character("="), Code::Equal, 57),
        ];
        for (key, code, number) in sdk {
            assert_eq!(vkey(key.clone(), code), number, "{key:?} {code:?}");
        }
    }

    /// A character on the main block, and a W3C key VST 2's list has no entry for, carry
    /// no virtual key: `VKEY_NONE` beside the character, or beside none.
    #[test]
    fn a_key_without_a_vst2_code_is_key_none() {
        assert_eq!(vkey(character("a"), Code::KeyA), 0);
        assert_eq!(vkey(character("5"), Code::Digit5), 0);
        assert_eq!(vkey(KeyboardKey::Named(NamedKey::F13), Code::F13), 0);
        assert_eq!(
            vkey(KeyboardKey::Named(NamedKey::ContextMenu), Code::ContextMenu),
            0
        );
    }

    /// A key typed with no name is `Key::None` beside its ASCII character.
    #[test]
    fn a_character_alone_is_key_none() {
        let code = key_code_of(
            &event(character("a"), Code::KeyA, Modifiers::empty()),
            ModifierConvention::Pc,
        )
        .expect("ASCII");
        assert_eq!(code.character, 'a');
        assert_eq!(isize::from(code.key), 0);
        assert_eq!(code.modifier, 0);
    }

    /// A named key types no character, and a keypad digit types its own beside its key.
    #[test]
    fn a_named_key_has_no_character_and_a_keypad_digit_has_its_own() {
        let left = key_code_of(
            &event(
                KeyboardKey::Named(NamedKey::ArrowLeft),
                Code::ArrowLeft,
                Modifiers::empty(),
            ),
            ModifierConvention::Pc,
        )
        .expect("expressible");
        assert_eq!((left.character, isize::from(left.key)), ('\0', 11));
        let seven = key_code_of(
            &event(character("7"), Code::Numpad7, Modifiers::empty()),
            ModifierConvention::Pc,
        )
        .expect("expressible");
        assert_eq!((seven.character, isize::from(seven.key)), ('7', 31));
    }

    /// A character outside ASCII, or a string of several, has no `[index]` to travel in:
    /// not forwarded.
    #[test]
    fn a_character_outside_ascii_is_not_expressible() {
        for typed in ["é", "ab", ""] {
            assert!(
                key_code_of(
                    &event(character(typed), Code::KeyA, Modifiers::empty()),
                    ModifierConvention::Pc
                )
                .is_none(),
                "{typed:?}"
            );
        }
    }

    /// The SDK numbers modifiers `shift` 1, `alt` 2, `command` 4 (Control on a Mac) and
    /// `control` 8 (Ctrl on a PC, Command on a Mac): off a Mac, Control is 8 and the Windows
    /// key has no bit; on a Mac, Control is 4 and Command is 8. Lock modifiers carry none.
    #[test]
    fn modifier_bits_follow_the_sdks_roles_per_platform() {
        let bits = |modifiers, convention| {
            key_code_of(&event(character("a"), Code::KeyA, modifiers), convention)
                .map(|code| code.modifier)
        };
        assert_eq!(
            bits(Modifiers::SHIFT | Modifiers::ALT, ModifierConvention::Pc),
            Some(3)
        );
        assert_eq!(bits(Modifiers::CONTROL, ModifierConvention::Pc), Some(8));
        assert_eq!(bits(Modifiers::META, ModifierConvention::Pc), None);
        assert_eq!(bits(Modifiers::CONTROL, ModifierConvention::Mac), Some(4));
        assert_eq!(bits(Modifiers::META, ModifierConvention::Mac), Some(8));
        // winit reports the Command and Windows key as `SUPER`.
        #[allow(deprecated)]
        {
            assert_eq!(bits(Modifiers::SUPER, ModifierConvention::Mac), Some(8));
            assert_eq!(bits(Modifiers::SUPER, ModifierConvention::Pc), None);
        }
        assert_eq!(
            bits(
                Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL | Modifiers::META,
                ModifierConvention::Mac
            ),
            Some(1 | 2 | 4 | 8),
            "every modifier a Mac has"
        );
        assert_eq!(
            bits(
                Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK | Modifiers::ALT_GRAPH,
                ModifierConvention::Pc
            ),
            Some(0),
            "lock state and AltGraph carry no bit"
        );
    }
}
