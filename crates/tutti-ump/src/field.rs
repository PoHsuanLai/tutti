//! Wire-width integers: a value of a stated range, so a field cannot hold one
//! the packet cannot carry.
//!
//! The public constructor refuses a value out of range. The codec builds one
//! from bits it has just masked, through the private `wire`.

/// A raw number above the largest the type holds, from `TryFrom<u8>` on an
/// address type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutOfRange {
    /// The refused number.
    pub value: u8,
    /// The largest the type accepts.
    pub max: u8,
}

impl core::fmt::Display for OutOfRange {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} is above the largest value, {}", self.value, self.max)
    }
}

impl std::error::Error for OutOfRange {}

// A bit-width field: `new` refuses above `MAX`, `wire` masks.
macro_rules! bits {
    ($(#[$doc:meta])* $name:ident, $repr:ty, $width:expr) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name($repr);

        impl $name {
            /// The largest value.
            pub const MAX: Self = Self((1 << $width) - 1);

            /// The value, or `None` when it needs more bits than the field has.
            #[must_use]
            pub const fn new(raw: $repr) -> Option<Self> {
                if raw <= Self::MAX.0 { Some(Self(raw)) } else { None }
            }

            /// The raw value.
            #[must_use]
            pub const fn get(self) -> $repr {
                self.0
            }

            // Masked, for the codec: the wire cannot carry more.
            pub(crate) const fn wire(raw: $repr) -> Self {
                Self(raw & Self::MAX.0)
            }
        }
    };
}

// A field with a stated inclusive range that is not a bit width.
macro_rules! bounded {
    ($(#[$doc:meta])* $name:ident, $repr:ty, $min:expr, $max:expr) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name($repr);

        impl $name {
            /// The value, or `None` when it is out of range.
            #[must_use]
            #[allow(unused_comparisons)]
            pub const fn new(raw: $repr) -> Option<Self> {
                if raw >= $min && raw <= $max { Some(Self(raw)) } else { None }
            }

            /// The raw value.
            #[must_use]
            pub const fn get(self) -> $repr {
                self.0
            }

            // The value as the wire sent it, in range or not: a reserved value
            // is kept so the message re-encodes exactly.
            #[allow(dead_code)] // `SharpsFlats` has no reserved value to keep.
            pub(crate) const fn wire(raw: $repr) -> Self {
                Self(raw)
            }
        }
    };
}

bits!(
    /// A 4-bit value: an MDS id, a chord degree.
    U4, u8, 4
);
bits!(
    /// A 7-bit data byte (`0..=127`): a velocity, a pressure, a program, a
    /// controller value, a SysEx byte.
    U7, u8, 7
);
bits!(
    /// A 14-bit value (`0..=16383`): a MIDI 1.0 pitch bend, a song position.
    U14, u16, 14
);
bits!(
    /// A MIDI note number on the wire (`0..=127`). The musical pitch is
    /// `tutti_types::Note`, which converts with `u8::try_from`.
    NoteNumber, u8, 7
);
bits!(
    /// A Delta Clockstamp: ticks since the previous event (20 bits).
    DeltaTicks, u32, 20
);

bounded!(
    /// A Function Block number (`0..=31`). A reserved number read from the wire
    /// is kept, so such a message re-encodes exactly.
    FunctionBlockId, u8, 0, 31
);
bounded!(
    /// A Group number as a Function Block declares its first Group (`0..=15`).
    /// A reserved number read from the wire is kept.
    FirstGroup, u8, 0, 15
);
bounded!(
    /// How many Groups a Function Block spans (`1..=16`). A reserved count read
    /// from the wire is kept.
    GroupSpan, u8, 1, 16
);
bounded!(
    /// Sharps (positive) or flats (negative) in a key signature, `-7..=7`.
    SharpsFlats, i8, -7, 7
);

/// An unsigned fixed-point pitch: a MIDI note number with a fraction of a
/// semitone, stored as Q7.25 (7 integer bits, 25 fractional bits). Note On
/// Attribute #3 carries it as Q7.9 (§7.4.15.3) and Registered Per-Note
/// Controller #3 as Q7.25 (§7.4.15.2). Q7.9 widens by zero extension, which is
/// exact (M2-115 §4), so one type holds both.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Pitch(u32);

impl Pitch {
    /// The pitch of a Q7.9 word.
    #[inline]
    #[must_use]
    pub const fn from_q7_9(bits: u16) -> Self {
        Self((bits as u32) << 16)
    }

    /// The pitch of a Q7.25 word.
    #[inline]
    #[must_use]
    pub const fn from_q7_25(bits: u32) -> Self {
        Self(bits)
    }

    /// The Q7.25 word.
    #[inline]
    #[must_use]
    pub const fn q7_25(self) -> u32 {
        self.0
    }

    /// The Q7.9 word: the fraction below 9 bits is dropped.
    #[inline]
    #[must_use]
    pub const fn q7_9(self) -> u16 {
        (self.0 >> 16) as u16
    }

    /// The fractional MIDI note number: 69.0 is A4, and 0.5 above is a
    /// quarter tone up.
    #[inline]
    #[must_use]
    pub fn note_number(self) -> f32 {
        self.0 as f32 / (1u32 << 25) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::Pitch;
    use crate::Attribute;

    // Mutation record: `from_q7_9` shifting by 15 -> `q7_9_is_widened_by_zero_extension`;
    // `note_number` dividing by 1 << 24 -> `the_note_number_is_the_integer_part_plus_the_fraction`.
    #[test]
    fn q7_9_is_widened_by_zero_extension() {
        for bits in [0u16, 1, 0x0200, 0x8C30, 0xFE00, 0xFFFF] {
            let p = Pitch::from_q7_9(bits);
            assert_eq!(p.q7_25(), u32::from(bits) << 16);
            assert_eq!(p.q7_9(), bits);
        }
    }

    #[test]
    fn the_note_number_is_the_integer_part_plus_the_fraction() {
        assert_eq!(Pitch::from_q7_9(60 << 9).note_number(), 60.0);
        assert_eq!(Pitch::from_q7_9((60 << 9) | 256).note_number(), 60.5);
        assert_eq!(Pitch::from_q7_25(69 << 25).note_number(), 69.0);
        assert_eq!(Pitch::from_q7_25(1 << 24).note_number(), 0.5);
        assert_eq!(Pitch::from_q7_25(127 << 25).note_number(), 127.0);
    }

    #[test]
    fn only_the_pitch_attribute_has_a_pitch() {
        assert_eq!(
            Attribute::Pitch(61 << 9).pitch(),
            Some(Pitch::from_q7_25(61 << 25))
        );
        assert_eq!(Attribute::None.pitch(), None);
        assert_eq!(Attribute::Manufacturer(61 << 9).pitch(), None);
        assert_eq!(Attribute::Reserved { kind: 4, data: 1 }.pitch(), None);
    }
}
