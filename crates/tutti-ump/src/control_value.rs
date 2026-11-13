//! The 32-bit data word of a MIDI 2.0 controller, pressure or per-note message.

/// The 32-bit value of a MIDI 2.0 Control Change, Channel or Polyphonic
/// Pressure, Registered or Assignable Controller, or Per-Note Controller
/// (M2-104 section 7.4).
///
/// Every `u32` is a value, so there is no refusing constructor, as for
/// [`NoteVelocity`](crate::NoteVelocity). The word is the wire's integer, not
/// a MIDI 1.0 value: a 7-bit value reaches it through
/// `tutti_midi_protocol::widen_value`, which scales by M2-115, and `127` as a
/// raw word is nearly zero.
///
/// # Examples
///
/// ```
/// use tutti_ump::{CCNumber, ControlValue, Message, MidiChannel, Voice2};
///
/// let half = ControlValue::new(0x8000_0000);
/// let cc = Message::Midi2(
///     MidiChannel::FIRST.into(),
///     Voice2::ControlChange { controller: CCNumber::VOLUME, value: half },
/// );
/// assert!(matches!(cc, Message::Midi2(..)));
/// assert_eq!(ControlValue::MAX.get(), u32::MAX);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct ControlValue(u32);

impl ControlValue {
    /// Value 0.
    pub const MIN: Self = Self(0);

    /// The centre of the range, `0x8000_0000`: where a 7-bit 64 widens to.
    pub const CENTER: Self = Self(0x8000_0000);

    /// Value `0xFFFF_FFFF`.
    pub const MAX: Self = Self(u32::MAX);

    /// The value whose word is `raw`.
    #[inline]
    #[must_use]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The word's 32 bits.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl From<ControlValue> for u32 {
    #[inline]
    fn from(value: ControlValue) -> Self {
        value.0
    }
}
