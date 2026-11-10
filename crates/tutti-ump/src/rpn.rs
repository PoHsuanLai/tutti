//! The address of a Registered Parameter Number.

use crate::U7;

/// The address of a Registered Controller (RPN): a 7-bit bank and a 7-bit
/// index (M2-104 section 7.4.7), the two halves a
/// [`Voice2::Registered`](crate::Voice2::Registered) carries.
///
/// The addresses tutti reads are named constants; any other address is
/// [`new`](Self::new).
///
/// ```
/// use tutti_ump::{Rpn, U7, Voice2};
///
/// let message = Voice2::Registered {
///     bank: U7::of(0),
///     index: U7::of(7),
///     value: 24 << 25,
/// };
/// let Voice2::Registered { bank, index, .. } = message else { unreachable!() };
/// assert_eq!(Rpn::new(bank, index), Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rpn {
    bank: U7,
    index: U7,
}

impl Rpn {
    /// RPN #00/00, **Channel Pitch Bend Sensitivity**. Its data is the classic
    /// MSB semitones, LSB cents form, not the per-note 7.25 fixed point.
    pub const PITCH_BEND_SENSITIVITY: Self = Self::new(U7::of(0), U7::of(0));

    /// RPN #00/06, the **MPE Configuration Message** (RP-053; M2-104 lists it as
    /// RPN 0x0006, "MPE MCM"). Its data is the member-channel count of the zone
    /// the message's channel names; `0` disables the zone.
    pub const MPE_CONFIGURATION: Self = Self::new(U7::of(0), U7::of(6));

    /// RPN #00/07, **Sensitivity of Per-Note Pitch Bend** (M2-104 section
    /// 7.4.13.1). Its data is a 7.25 fixed-point semitone range, shared by every
    /// note number on the channel.
    pub const PER_NOTE_PITCH_BEND_SENSITIVITY: Self = Self::new(U7::of(0), U7::of(7));

    /// The address of `bank` and `index`.
    #[inline]
    #[must_use]
    pub const fn new(bank: U7, index: U7) -> Self {
        Self { bank, index }
    }

    /// The bank (the RPN MSB).
    #[inline]
    #[must_use]
    pub const fn bank(self) -> U7 {
        self.bank
    }

    /// The index (the RPN LSB).
    #[inline]
    #[must_use]
    pub const fn index(self) -> U7 {
        self.index
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each constant against the number the spec gives it, in one table
    /// (M2-104 sections 7.4.7 and 7.4.13.1: bank, index).
    ///
    /// Mutation: change `MPE_CONFIGURATION` to index 7 and the row fails.
    #[test]
    fn the_named_addresses_are_the_specs_numbers() {
        let table = [
            (Rpn::PITCH_BEND_SENSITIVITY, 0u8, 0u8),
            (Rpn::MPE_CONFIGURATION, 0, 6),
            (Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY, 0, 7),
        ];
        for (rpn, bank, index) in table {
            assert_eq!((rpn.bank().get(), rpn.index().get()), (bank, index));
        }
    }

    #[test]
    fn new_keeps_both_halves() {
        let rpn = Rpn::new(U7::of(0x12), U7::of(0x34));
        assert_eq!((rpn.bank().get(), rpn.index().get()), (0x12, 0x34));
        assert_ne!(rpn, Rpn::new(U7::of(0x34), U7::of(0x12)));
    }
}
