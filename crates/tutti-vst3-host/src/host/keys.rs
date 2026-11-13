//! Editor key events at the ABI edge: a [`KeyEvent`] as the three values
//! `IPlugView::onKeyDown` and `onKeyUp` carry.

use keyboard_types::{Code, Key as KeyboardKey, Modifiers, NamedKey};
use tutti_plugin_types::{KeyEvent, ModifierConvention};
use vst3::Steinberg::KeyModifier_ as M;
use vst3::Steinberg::VirtualKeyCodes_ as V;

use crate::helpers::sdk_enum_i32;

/// A key as `onKeyDown`'s arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ViewKey {
    /// The UTF-16 unit typed, or 0 for a key with no character.
    pub(crate) character: u16,
    /// The SDK's `VirtualKeyCodes` value, or 0 for a key with no name.
    pub(crate) virtual_key: i16,
    /// The `KeyModifier` bits.
    pub(crate) modifiers: i16,
}

/// The key as the view's arguments, or `None` when VST3 cannot write it: a character outside
/// the Basic Multilingual Plane, or a string of several (`char16` is one UTF-16 unit).
///
/// A key typed with a character travels as that character; a named key types none, and
/// travels as 0. The virtual key is the key's name where it has one, and for a character key
/// the one the space, `=` or numeric keypad position gives ([`virtual_key_of`]); a key with
/// none is 0.
///
/// `convention` is [`ModifierConvention::Mac`] on macOS, where `kCommandKey` is the Command
/// key and `kControlKey` the Control key. Under [`ModifierConvention::Pc`] `kCommandKey` is
/// Control and `kControlKey` the Windows key. Lock modifiers and AltGraph change what a key
/// typed, which `key` already says, and carry no bit.
pub(crate) fn view_key_of(event: &KeyEvent, convention: ModifierConvention) -> Option<ViewKey> {
    let character = match &event.key {
        KeyboardKey::Character(typed) => {
            let mut chars = typed.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => u16::try_from(u32::from(c)).ok()?,
                _ => return None,
            }
        }
        KeyboardKey::Named(_) => 0,
    };
    let (control, command) = if convention == ModifierConvention::Mac {
        (M::kControlKey, M::kCommandKey)
    } else {
        (M::kCommandKey, M::kControlKey)
    };
    let mut modifiers = 0;
    for (held, bit) in [
        (event.modifiers.contains(Modifiers::SHIFT), M::kShiftKey),
        (event.modifiers.contains(Modifiers::ALT), M::kAlternateKey),
        (event.modifiers.contains(Modifiers::CONTROL), control),
        (event.meta_held(), command),
    ] {
        if held {
            modifiers |= sdk_enum_i32(bit) as i16;
        }
    }
    Some(ViewKey {
        character,
        virtual_key: virtual_key_of(&event.key, event.code, convention)
            .map_or(0, |k| sdk_enum_i32(k) as i16),
        modifiers,
    })
}

/// The `KEY_*` a key is: a named key by its name, a character key by the space or `=` it
/// typed, and a numeric keypad key by its position, which `code` alone tells from the main
/// block's. `None` for a key VST3 has no code for.
///
/// `KEY_SUPER` is the Windows key, so only [`ModifierConvention::Pc`] names it; the SDK
/// numbers it as the Control key on a Mac, which `KEY_CONTROL` already is.
fn virtual_key_of(
    key: &KeyboardKey,
    code: Code,
    convention: ModifierConvention,
) -> Option<vst3::Steinberg::VirtualKeyCodes> {
    Some(match key {
        KeyboardKey::Named(named) => match named {
            NamedKey::Backspace => V::KEY_BACK,
            NamedKey::Tab => V::KEY_TAB,
            NamedKey::Clear => V::KEY_CLEAR,
            NamedKey::Enter if code == Code::NumpadEnter => V::KEY_ENTER,
            NamedKey::Enter => V::KEY_RETURN,
            NamedKey::Pause => V::KEY_PAUSE,
            NamedKey::Escape => V::KEY_ESCAPE,
            NamedKey::End => V::KEY_END,
            NamedKey::Home => V::KEY_HOME,
            NamedKey::ArrowLeft => V::KEY_LEFT,
            NamedKey::ArrowUp => V::KEY_UP,
            NamedKey::ArrowRight => V::KEY_RIGHT,
            NamedKey::ArrowDown => V::KEY_DOWN,
            NamedKey::PageUp => V::KEY_PAGEUP,
            NamedKey::PageDown => V::KEY_PAGEDOWN,
            NamedKey::Select => V::KEY_SELECT,
            NamedKey::Print => V::KEY_PRINT,
            NamedKey::PrintScreen => V::KEY_SNAPSHOT,
            NamedKey::Insert => V::KEY_INSERT,
            NamedKey::Delete => V::KEY_DELETE,
            NamedKey::Help => V::KEY_HELP,
            NamedKey::F1 => V::KEY_F1,
            NamedKey::F2 => V::KEY_F2,
            NamedKey::F3 => V::KEY_F3,
            NamedKey::F4 => V::KEY_F4,
            NamedKey::F5 => V::KEY_F5,
            NamedKey::F6 => V::KEY_F6,
            NamedKey::F7 => V::KEY_F7,
            NamedKey::F8 => V::KEY_F8,
            NamedKey::F9 => V::KEY_F9,
            NamedKey::F10 => V::KEY_F10,
            NamedKey::F11 => V::KEY_F11,
            NamedKey::F12 => V::KEY_F12,
            NamedKey::F13 => V::KEY_F13,
            NamedKey::F14 => V::KEY_F14,
            NamedKey::F15 => V::KEY_F15,
            NamedKey::F16 => V::KEY_F16,
            NamedKey::F17 => V::KEY_F17,
            NamedKey::F18 => V::KEY_F18,
            NamedKey::F19 => V::KEY_F19,
            NamedKey::F20 => V::KEY_F20,
            NamedKey::F21 => V::KEY_F21,
            NamedKey::F22 => V::KEY_F22,
            NamedKey::F23 => V::KEY_F23,
            NamedKey::F24 => V::KEY_F24,
            NamedKey::NumLock => V::KEY_NUMLOCK,
            NamedKey::ScrollLock => V::KEY_SCROLL,
            NamedKey::Shift => V::KEY_SHIFT,
            NamedKey::Control => V::KEY_CONTROL,
            NamedKey::Alt => V::KEY_ALT,
            NamedKey::ContextMenu => V::KEY_CONTEXTMENU,
            NamedKey::MediaPlay => V::KEY_MEDIA_PLAY,
            NamedKey::MediaStop => V::KEY_MEDIA_STOP,
            NamedKey::MediaTrackPrevious => V::KEY_MEDIA_PREV,
            NamedKey::MediaTrackNext => V::KEY_MEDIA_NEXT,
            NamedKey::AudioVolumeUp => V::KEY_VOLUME_UP,
            NamedKey::AudioVolumeDown => V::KEY_VOLUME_DOWN,
            NamedKey::Meta if convention == ModifierConvention::Pc => V::KEY_SUPER,
            _ => return None,
        },
        KeyboardKey::Character(typed) => match (typed.as_str(), code) {
            (" ", _) => V::KEY_SPACE,
            ("=", _) => V::KEY_EQUALS,
            (_, Code::Numpad0) => V::KEY_NUMPAD0,
            (_, Code::Numpad1) => V::KEY_NUMPAD1,
            (_, Code::Numpad2) => V::KEY_NUMPAD2,
            (_, Code::Numpad3) => V::KEY_NUMPAD3,
            (_, Code::Numpad4) => V::KEY_NUMPAD4,
            (_, Code::Numpad5) => V::KEY_NUMPAD5,
            (_, Code::Numpad6) => V::KEY_NUMPAD6,
            (_, Code::Numpad7) => V::KEY_NUMPAD7,
            (_, Code::Numpad8) => V::KEY_NUMPAD8,
            (_, Code::Numpad9) => V::KEY_NUMPAD9,
            (_, Code::NumpadMultiply) => V::KEY_MULTIPLY,
            (_, Code::NumpadAdd) => V::KEY_ADD,
            (_, Code::NumpadComma) => V::KEY_SEPARATOR,
            (_, Code::NumpadSubtract) => V::KEY_SUBTRACT,
            (_, Code::NumpadDecimal) => V::KEY_DECIMAL,
            (_, Code::NumpadDivide) => V::KEY_DIVIDE,
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

    fn view_key(key: KeyboardKey, code: Code, convention: ModifierConvention) -> Option<ViewKey> {
        view_key_of(&event(key, code, Modifiers::empty()), convention)
    }

    /// The keys are the SDK's `KEY_*` numbers, written as literals: a transposed arm of
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
            (named(NamedKey::ContextMenu), Code::ContextMenu, 58),
            (named(NamedKey::MediaPlay), Code::Unidentified, 59),
            (named(NamedKey::MediaStop), Code::MediaStop, 60),
            (
                named(NamedKey::MediaTrackPrevious),
                Code::MediaTrackPrevious,
                61,
            ),
            (named(NamedKey::MediaTrackNext), Code::MediaTrackNext, 62),
            (named(NamedKey::AudioVolumeUp), Code::AudioVolumeUp, 63),
            (named(NamedKey::AudioVolumeDown), Code::AudioVolumeDown, 64),
            (named(NamedKey::F13), Code::F13, 65),
            (named(NamedKey::F24), Code::F24, 76),
        ];
        for (key, code, number) in sdk {
            let sent = view_key(key.clone(), code, ModifierConvention::Pc).expect("expressible");
            assert_eq!(sent.virtual_key, number, "{key:?} {code:?}");
            assert_eq!(
                sent.character != 0,
                matches!(key, KeyboardKey::Character(_)),
                "{key:?}: a named key types no character, a character key does"
            );
        }
    }

    /// The Windows key is `KEY_SUPER` (77) off a Mac only; on a Mac the SDK numbers that
    /// code as Control, so the Meta key alone has none. A character on the main block, and
    /// a W3C key the SDK has no entry for, carry virtual key 0.
    #[test]
    fn super_is_the_windows_key_and_unlisted_keys_carry_none() {
        let meta = KeyboardKey::Named(NamedKey::Meta);
        let pc = view_key(meta.clone(), Code::MetaLeft, ModifierConvention::Pc).expect("ok");
        assert_eq!(pc.virtual_key, 77);
        let mac = view_key(meta, Code::MetaLeft, ModifierConvention::Mac).expect("ok");
        assert_eq!(mac.virtual_key, 0);
        for key in [
            character("a"),
            character("5"),
            KeyboardKey::Named(NamedKey::F25),
        ] {
            let sent = view_key(key, Code::Unidentified, ModifierConvention::Pc).expect("ok");
            assert_eq!(sent.virtual_key, 0);
        }
    }

    /// A character with no name travels as its UTF-16 unit beside virtual key 0; a keypad
    /// digit carries its own beside its key; one outside the Basic Multilingual Plane, or a
    /// string of several, has no `char16` and is not expressible.
    #[test]
    fn a_character_is_a_utf16_unit_and_a_pair_is_not_expressible() {
        let a = view_key(character("é"), Code::KeyE, ModifierConvention::Pc).expect("in the BMP");
        assert_eq!((a.character, a.virtual_key, a.modifiers), (0xE9, 0, 0));
        let seven = view_key(character("7"), Code::Numpad7, ModifierConvention::Pc).expect("ok");
        assert_eq!((seven.character, seven.virtual_key), (55, 31));
        for typed in ["\u{1D11E}", "ab", ""] {
            assert!(
                view_key(character(typed), Code::KeyA, ModifierConvention::Pc).is_none(),
                "{typed:?}"
            );
        }
    }

    /// The SDK numbers modifiers `kShiftKey` 1, `kAlternateKey` 2, `kCommandKey` 4 and
    /// `kControlKey` 8: off a Mac, Control is bit 4 and the Windows key bit 8; on a Mac,
    /// Command is 4 and Control is 8. Lock modifiers carry none.
    #[test]
    fn modifier_bits_follow_the_sdks_roles_per_platform() {
        let bits = |modifiers, convention| {
            view_key_of(&event(character("a"), Code::KeyA, modifiers), convention)
                .map(|k| k.modifiers)
        };
        assert_eq!(
            bits(Modifiers::SHIFT | Modifiers::ALT, ModifierConvention::Pc),
            Some(3)
        );
        assert_eq!(bits(Modifiers::CONTROL, ModifierConvention::Pc), Some(4));
        assert_eq!(bits(Modifiers::META, ModifierConvention::Pc), Some(8));
        assert_eq!(bits(Modifiers::CONTROL, ModifierConvention::Mac), Some(8));
        assert_eq!(bits(Modifiers::META, ModifierConvention::Mac), Some(4));
        // winit reports the Command and Windows key as `SUPER`.
        #[allow(deprecated)]
        {
            assert_eq!(bits(Modifiers::SUPER, ModifierConvention::Mac), Some(4));
            assert_eq!(bits(Modifiers::SUPER, ModifierConvention::Pc), Some(8));
        }
        let held = Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL | Modifiers::META;
        assert_eq!(bits(held, ModifierConvention::Mac), Some(15));
        assert_eq!(
            bits(
                held | Modifiers::CAPS_LOCK | Modifiers::ALT_GRAPH,
                ModifierConvention::Pc
            ),
            Some(15),
            "lock state and AltGraph carry no bit"
        );
    }
}
