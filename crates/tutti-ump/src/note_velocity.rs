//! The velocity field of a MIDI 2.0 Note On or Note Off.

/// The 16-bit velocity of a MIDI 2.0 Note On or Note Off (M2-104 section 7.4.2).
///
/// This is the wire's 16-bit field, `0` to `0xFFFF`, and not a MIDI 1.0
/// velocity: `NoteVelocity::new(100)` is a velocity of 100 out of 65535, which
/// is nearly silent, and a MIDI 1.0 velocity of 100 is `51492` here. Every
/// `u16` is a velocity, so there is no refusing constructor. Zero is a Note On
/// here, not a Note Off.
///
/// A 7-bit velocity is widened by M2-115's scaling in
/// `tutti_midi_protocol::widen_velocity` (the scaling lives there, beside the
/// translator that shares it, since this crate depends on no other), and a
/// level in `0.0..=1.0` by `tutti_midi_protocol::unit_f32_to_velocity`. The
/// way back is `narrow_velocity` and `velocity_to_unit_f32`.
///
/// # Examples
///
/// ```
/// use tutti_ump::{Message, MidiChannel, NoteNumber, NoteVelocity, Voice2};
///
/// // Half of the 16-bit range, which is a MIDI 1.0 velocity of 64.
/// let on = Message::Midi2(
///     MidiChannel::FIRST.into(),
///     Voice2::note_on(NoteNumber::of(60), NoteVelocity::new(0x8000)),
/// );
/// assert!(matches!(on, Message::Midi2(..)));
/// ```
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct NoteVelocity(u16);

impl NoteVelocity {
    /// Velocity 0.
    pub const MIN: Self = Self(0);

    /// Velocity `0x8000`, the midpoint of the range: M2-115 section 3 widens a
    /// MIDI 1.0 velocity of 64 to it.
    pub const CENTER: Self = Self(0x8000);

    /// Velocity `0xFFFF`.
    pub const MAX: Self = Self(u16::MAX);

    /// The velocity whose field is `raw`.
    #[inline]
    #[must_use]
    pub const fn new(raw: u16) -> Self {
        Self(raw)
    }

    /// The field's 16 bits.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl From<NoteVelocity> for u16 {
    #[inline]
    fn from(velocity: NoteVelocity) -> Self {
        velocity.0
    }
}
