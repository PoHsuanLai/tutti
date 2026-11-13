//! Editor key events at the ABI edge: a [`KeyEvent`] as the three values
//! `effEditKeyDown` and `effEditKeyUp` carry.

use tutti_plugin_types::{KeyEvent, KeyModifiers, NamedKey};
use tutti_vst2_ffi::api::ModifierKey;
use tutti_vst2_ffi::editor::{Key, KeyCode as Vst2KeyCode};

/// The key as `effEditKeyDown`'s three arguments, or `None` when VST 2 cannot write it: a
/// character outside ASCII (`[index]` is an ASCII character), or a modifier the host's
/// platform has no bit for.
///
/// `command_is_apple` is whether the platform is macOS, where the SDK numbers the bits by
/// role: `kVstModifierControl` (8) is the Command key, and `kVstModifierCommand` (4) the
/// Control key. Elsewhere bit 8 is Control and no bit is the Windows key.
pub(crate) fn key_code_of(event: &KeyEvent, command_is_apple: bool) -> Option<Vst2KeyCode> {
    let character = match event.key.character() {
        Some(character) if character.is_ascii() => character,
        Some(_) => return None,
        None => '\0',
    };
    let mut modifier = ModifierKey::empty();
    for (flag, bit) in [
        (KeyModifiers::SHIFT, Some(ModifierKey::SHIFT)),
        (KeyModifiers::ALT, Some(ModifierKey::ALT)),
        (
            KeyModifiers::CONTROL,
            Some(if command_is_apple {
                ModifierKey::COMMAND
            } else {
                ModifierKey::CONTROL
            }),
        ),
        (
            KeyModifiers::COMMAND,
            command_is_apple.then_some(ModifierKey::CONTROL),
        ),
    ] {
        if event.modifiers.contains(flag) {
            modifier |= bit?;
        }
    }
    Some(Vst2KeyCode {
        character,
        key: event.key.named().map_or(Key::None, key_of),
        modifier: modifier.bits(),
    })
}

/// The `VKEY_*` a named key is.
fn key_of(key: NamedKey) -> Key {
    match key {
        NamedKey::Backspace => Key::Back,
        NamedKey::Tab => Key::Tab,
        NamedKey::Clear => Key::Clear,
        NamedKey::Return => Key::Return,
        NamedKey::Pause => Key::Pause,
        NamedKey::Escape => Key::Escape,
        NamedKey::Space => Key::Space,
        NamedKey::Next => Key::Next,
        NamedKey::End => Key::End,
        NamedKey::Home => Key::Home,
        NamedKey::Left => Key::Left,
        NamedKey::Up => Key::Up,
        NamedKey::Right => Key::Right,
        NamedKey::Down => Key::Down,
        NamedKey::PageUp => Key::PageUp,
        NamedKey::PageDown => Key::PageDown,
        NamedKey::Select => Key::Select,
        NamedKey::Print => Key::Print,
        NamedKey::Enter => Key::Enter,
        NamedKey::Snapshot => Key::Snapshot,
        NamedKey::Insert => Key::Insert,
        NamedKey::Delete => Key::Delete,
        NamedKey::Help => Key::Help,
        NamedKey::Numpad0 => Key::Numpad0,
        NamedKey::Numpad1 => Key::Numpad1,
        NamedKey::Numpad2 => Key::Numpad2,
        NamedKey::Numpad3 => Key::Numpad3,
        NamedKey::Numpad4 => Key::Numpad4,
        NamedKey::Numpad5 => Key::Numpad5,
        NamedKey::Numpad6 => Key::Numpad6,
        NamedKey::Numpad7 => Key::Numpad7,
        NamedKey::Numpad8 => Key::Numpad8,
        NamedKey::Numpad9 => Key::Numpad9,
        NamedKey::Multiply => Key::Multiply,
        NamedKey::Add => Key::Add,
        NamedKey::Separator => Key::Separator,
        NamedKey::Subtract => Key::Subtract,
        NamedKey::Decimal => Key::Decimal,
        NamedKey::Divide => Key::Divide,
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
        NamedKey::Equals => Key::Equals,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::{KeyCode, KeyPhase};

    fn event(character: Option<char>, key: Option<NamedKey>, modifiers: KeyModifiers) -> KeyEvent {
        let key = match (character, key) {
            (Some(c), Some(k)) => KeyCode::Both(c, k),
            (Some(c), None) => KeyCode::Char(c),
            (None, Some(k)) => KeyCode::Named(k),
            (None, None) => panic!("an event names a key"),
        };
        KeyEvent {
            phase: KeyPhase::Down,
            key,
            modifiers,
        }
    }

    /// The named keys are the SDK's `VKEY_*` numbers, written as literals: a transposed arm
    /// of `key_of` fails here though a test through `key_of` would agree with it.
    #[test]
    fn named_keys_are_the_sdks_virtual_key_numbers() {
        let sdk = [
            (NamedKey::Backspace, 1),
            (NamedKey::Tab, 2),
            (NamedKey::Return, 4),
            (NamedKey::Escape, 6),
            (NamedKey::Space, 7),
            (NamedKey::Left, 11),
            (NamedKey::Delete, 22),
            (NamedKey::Numpad0, 24),
            (NamedKey::Divide, 39),
            (NamedKey::F1, 40),
            (NamedKey::F12, 51),
            (NamedKey::NumLock, 52),
            (NamedKey::Equals, 57),
        ];
        for (named, number) in sdk {
            let code = key_code_of(&event(None, Some(named), KeyModifiers::empty()), false)
                .expect("expressible");
            assert_eq!(isize::from(code.key), number, "{named:?}");
        }
    }

    /// A key typed with no name is `Key::None` beside its ASCII character.
    #[test]
    fn a_character_alone_is_key_none() {
        let code =
            key_code_of(&event(Some('a'), None, KeyModifiers::empty()), false).expect("ASCII");
        assert_eq!(code.character, 'a');
        assert_eq!(isize::from(code.key), 0);
        assert_eq!(code.modifier, 0);
    }

    /// A character outside ASCII has no `[index]` to travel in: not forwarded.
    #[test]
    fn a_character_outside_ascii_is_not_expressible() {
        assert!(key_code_of(&event(Some('é'), None, KeyModifiers::empty()), false).is_none());
    }

    /// The SDK numbers modifiers `shift` 1, `alt` 2, `command` 4 (Control on a Mac) and
    /// `control` 8 (Ctrl on a PC, Command on a Mac): off a Mac, Control is 8 and the Windows
    /// key has no bit; on a Mac, Control is 4 and Command is 8.
    #[test]
    fn modifier_bits_follow_the_sdks_roles_per_platform() {
        let bits = |modifiers, apple| {
            key_code_of(&event(Some('a'), None, modifiers), apple).map(|code| code.modifier)
        };
        assert_eq!(
            bits(KeyModifiers::SHIFT | KeyModifiers::ALT, false),
            Some(3)
        );
        assert_eq!(bits(KeyModifiers::CONTROL, false), Some(8));
        assert_eq!(bits(KeyModifiers::COMMAND, false), None);
        assert_eq!(bits(KeyModifiers::CONTROL, true), Some(4));
        assert_eq!(bits(KeyModifiers::COMMAND, true), Some(8));
        assert_eq!(
            bits(KeyModifiers::all(), true),
            Some(1 | 2 | 4 | 8),
            "every modifier a Mac has"
        );
    }
}
