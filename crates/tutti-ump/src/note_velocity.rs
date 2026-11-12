//! The velocity field of a MIDI 2.0 Note On or Note Off.

/// The 16-bit velocity of a MIDI 2.0 Note On or Note Off (M2-104 section 7.4.2).
///
/// Every `u16` is a velocity, so there is no refusing constructor. Zero is a
/// Note On here, not a Note Off; a MIDI 1.0 velocity reaches this field through
/// `tutti_midi_protocol::widen_velocity`, and leaves it through
/// `narrow_velocity`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct NoteVelocity(u16);

impl NoteVelocity {
    /// Velocity 0.
    pub const MIN: Self = Self(0);

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
