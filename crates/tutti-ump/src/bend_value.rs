//! The 32-bit pitch bend word of a MIDI 2.0 Pitch Bend or Per-Note Pitch Bend.

/// The 32-bit value of a MIDI 2.0 Pitch Bend or Per-Note Pitch Bend (M2-104
/// sections 7.4.11 and 7.4.7), centred on [`CENTER`](Self::CENTER).
///
/// Every `u32` is a bend, so there is no refusing constructor. The word is the
/// wire's integer: a MIDI 1.0 14-bit bend reaches it through
/// `tutti_midi_protocol::widen_bend`, which scales by M2-115, and the raw
/// `0x2000` of a 14-bit centre is far below the centre here.
///
/// # Examples
///
/// ```
/// use tutti_ump::{BendValue, Message, MidiChannel};
///
/// let bend = Message::pitch_bend2(MidiChannel::FIRST, BendValue::CENTER);
/// assert!(matches!(bend, Message::Midi2(..)));
/// assert_eq!(BendValue::CENTER.get(), 0x8000_0000);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct BendValue(u32);

impl BendValue {
    /// The lowest bend, `0`.
    pub const MIN: Self = Self(0);

    /// No bend, `0x8000_0000`.
    pub const CENTER: Self = Self(0x8000_0000);

    /// The highest bend, `0xFFFF_FFFF`.
    pub const MAX: Self = Self(u32::MAX);

    /// The bend whose word is `raw`.
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

impl From<BendValue> for u32 {
    #[inline]
    fn from(bend: BendValue) -> Self {
        bend.0
    }
}
