//! Wire-width integers: a value of a stated range, so a field cannot hold one
//! the packet cannot carry.
//!
//! The public constructor refuses a value out of range. The codec builds one
//! from bits it has just masked, through the private `wire`.

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
    /// A Function Block number (`0..=31`).
    FunctionBlockId, u8, 0, 31
);
bounded!(
    /// A count of Function Blocks (`0..=32`).
    FunctionBlockCount, u8, 0, 32
);
bounded!(
    /// How many Groups a Function Block spans (`1..=16`).
    GroupSpan, u8, 1, 16
);
bounded!(
    /// Sharps (positive) or flats (negative) in a key signature, `-7..=7`.
    SharpsFlats, i8, -7, 7
);
