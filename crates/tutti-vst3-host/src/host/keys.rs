//! Editor key events at the ABI edge: a [`KeyEvent`] as the three values
//! `IPlugView::onKeyDown` and `onKeyUp` carry.

use tutti_plugin_types::{KeyEvent, KeyModifiers, ModifierConvention, NamedKey};
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

/// The key as the view's arguments, or `None` when VST3 cannot write it: a character
/// outside the Basic Multilingual Plane (`char16` is one UTF-16 unit).
///
/// `convention` is [`ModifierConvention::Mac`] on macOS, where `kCommandKey` is the Command
/// key and `kControlKey` the Control key. Under [`ModifierConvention::Pc`] `kCommandKey` is Control and
/// `kControlKey` the Windows key.
pub(crate) fn view_key_of(event: &KeyEvent, convention: ModifierConvention) -> Option<ViewKey> {
    let character = match event.key.character() {
        Some(c) => u16::try_from(u32::from(c)).ok()?,
        None => 0,
    };
    let (control, command) = if convention == ModifierConvention::Mac {
        (M::kControlKey, M::kCommandKey)
    } else {
        (M::kCommandKey, M::kControlKey)
    };
    let mut modifiers = 0;
    for (flag, bit) in [
        (KeyModifiers::SHIFT, M::kShiftKey),
        (KeyModifiers::ALT, M::kAlternateKey),
        (KeyModifiers::CONTROL, control),
        (KeyModifiers::COMMAND, command),
    ] {
        if event.modifiers.contains(flag) {
            modifiers |= sdk_enum_i32(bit) as i16;
        }
    }
    Some(ViewKey {
        character,
        virtual_key: event
            .key
            .named()
            .map_or(0, |k| sdk_enum_i32(key_of(k)) as i16),
        modifiers,
    })
}

/// The `KEY_*` a named key is.
fn key_of(key: NamedKey) -> vst3::Steinberg::VirtualKeyCodes {
    match key {
        NamedKey::Backspace => V::KEY_BACK,
        NamedKey::Tab => V::KEY_TAB,
        NamedKey::Clear => V::KEY_CLEAR,
        NamedKey::Return => V::KEY_RETURN,
        NamedKey::Pause => V::KEY_PAUSE,
        NamedKey::Escape => V::KEY_ESCAPE,
        NamedKey::Space => V::KEY_SPACE,
        NamedKey::Next => V::KEY_NEXT,
        NamedKey::End => V::KEY_END,
        NamedKey::Home => V::KEY_HOME,
        NamedKey::Left => V::KEY_LEFT,
        NamedKey::Up => V::KEY_UP,
        NamedKey::Right => V::KEY_RIGHT,
        NamedKey::Down => V::KEY_DOWN,
        NamedKey::PageUp => V::KEY_PAGEUP,
        NamedKey::PageDown => V::KEY_PAGEDOWN,
        NamedKey::Select => V::KEY_SELECT,
        NamedKey::Print => V::KEY_PRINT,
        NamedKey::Enter => V::KEY_ENTER,
        NamedKey::Snapshot => V::KEY_SNAPSHOT,
        NamedKey::Insert => V::KEY_INSERT,
        NamedKey::Delete => V::KEY_DELETE,
        NamedKey::Help => V::KEY_HELP,
        NamedKey::Numpad0 => V::KEY_NUMPAD0,
        NamedKey::Numpad1 => V::KEY_NUMPAD1,
        NamedKey::Numpad2 => V::KEY_NUMPAD2,
        NamedKey::Numpad3 => V::KEY_NUMPAD3,
        NamedKey::Numpad4 => V::KEY_NUMPAD4,
        NamedKey::Numpad5 => V::KEY_NUMPAD5,
        NamedKey::Numpad6 => V::KEY_NUMPAD6,
        NamedKey::Numpad7 => V::KEY_NUMPAD7,
        NamedKey::Numpad8 => V::KEY_NUMPAD8,
        NamedKey::Numpad9 => V::KEY_NUMPAD9,
        NamedKey::Multiply => V::KEY_MULTIPLY,
        NamedKey::Add => V::KEY_ADD,
        NamedKey::Separator => V::KEY_SEPARATOR,
        NamedKey::Subtract => V::KEY_SUBTRACT,
        NamedKey::Decimal => V::KEY_DECIMAL,
        NamedKey::Divide => V::KEY_DIVIDE,
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
        NamedKey::NumLock => V::KEY_NUMLOCK,
        NamedKey::ScrollLock => V::KEY_SCROLL,
        NamedKey::Shift => V::KEY_SHIFT,
        NamedKey::Control => V::KEY_CONTROL,
        NamedKey::Alt => V::KEY_ALT,
        NamedKey::Equals => V::KEY_EQUALS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin_types::{KeyCode, KeyPhase};

    fn event(key: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            phase: KeyPhase::Down,
            key,
            modifiers,
        }
    }

    /// The named keys are the SDK's `KEY_*` numbers, written as literals: a transposed arm
    /// of `key_of` fails here though a test through `key_of` would agree with it.
    #[test]
    fn named_keys_are_the_sdks_virtual_key_numbers() {
        let sdk = [
            (NamedKey::Backspace, 1),
            (NamedKey::Tab, 2),
            (NamedKey::Return, 4),
            (NamedKey::Escape, 6),
            (NamedKey::Space, 7),
            (NamedKey::Next, 8),
            (NamedKey::Left, 11),
            (NamedKey::PageDown, 16),
            (NamedKey::Delete, 22),
            (NamedKey::Numpad0, 24),
            (NamedKey::Divide, 39),
            (NamedKey::F1, 40),
            (NamedKey::F12, 51),
            (NamedKey::NumLock, 52),
            (NamedKey::ScrollLock, 53),
            (NamedKey::Equals, 57),
        ];
        for (named, number) in sdk {
            let key = view_key_of(
                &event(KeyCode::Named(named), KeyModifiers::empty()),
                ModifierConvention::Pc,
            )
            .expect("expressible");
            assert_eq!(key.virtual_key, number, "{named:?}");
            assert_eq!(key.character, 0, "{named:?} types no character");
        }
    }

    /// A character with no name travels as its UTF-16 unit beside virtual key 0; one
    /// outside the Basic Multilingual Plane has no `char16` and is not expressible.
    #[test]
    fn a_character_is_a_utf16_unit_and_a_pair_is_not_expressible() {
        let a = view_key_of(
            &event(KeyCode::Char('é'), KeyModifiers::empty()),
            ModifierConvention::Pc,
        )
        .expect("in the BMP");
        assert_eq!((a.character, a.virtual_key, a.modifiers), (0xE9, 0, 0));
        let both = view_key_of(
            &event(KeyCode::Both('a', NamedKey::Left), KeyModifiers::empty()),
            ModifierConvention::Pc,
        )
        .expect("expressible");
        assert_eq!((both.character, both.virtual_key), (97, 11));
        assert!(
            view_key_of(
                &event(KeyCode::Char('\u{1D11E}'), KeyModifiers::empty()),
                ModifierConvention::Pc
            )
            .is_none()
        );
    }

    /// The SDK numbers modifiers `kShiftKey` 1, `kAlternateKey` 2, `kCommandKey` 4 and
    /// `kControlKey` 8: off a Mac, Control is bit 4 and the Windows key bit 8; on a Mac,
    /// Command is 4 and Control is 8.
    #[test]
    fn modifier_bits_follow_the_sdks_roles_per_platform() {
        let bits = |modifiers, convention| {
            view_key_of(&event(KeyCode::Char('a'), modifiers), convention).map(|k| k.modifiers)
        };
        assert_eq!(
            bits(
                KeyModifiers::SHIFT | KeyModifiers::ALT,
                ModifierConvention::Pc
            ),
            Some(3)
        );
        assert_eq!(bits(KeyModifiers::CONTROL, ModifierConvention::Pc), Some(4));
        assert_eq!(bits(KeyModifiers::COMMAND, ModifierConvention::Pc), Some(8));
        assert_eq!(
            bits(KeyModifiers::CONTROL, ModifierConvention::Mac),
            Some(8)
        );
        assert_eq!(
            bits(KeyModifiers::COMMAND, ModifierConvention::Mac),
            Some(4)
        );
        assert_eq!(bits(KeyModifiers::all(), ModifierConvention::Mac), Some(15));
    }
}
