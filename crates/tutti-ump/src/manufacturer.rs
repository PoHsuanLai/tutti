//! [`ManufacturerId`]: the 16-bit manufacturer id of M2-104 section 7.10.

use crate::U7;

/// A manufacturer's id in the 16-bit form (MfrID) that System Exclusive 8,
/// Mixed Data Sets and the Device Identity use (M2-104 section 7.10), and the
/// MIDI 1.0 System Exclusive ids it encodes: a 7-bit id (one byte, which includes
/// the Special and Universal ids) or a 21-bit id (`00 xx yy`).
///
/// A one-byte id is `0x00xx`. A three-byte id has the high bit of the word set
/// and its second and third bytes in the high and low bytes: `0x8000 | xx << 8 | yy`.
/// So a word is a valid id when it is `0x0000..=0x007F`, or when its top bit is set
/// and bits 14 to 8 and 6 to 0 hold the two 7-bit bytes, and bit 7 is clear:
/// [`new`](Self::new) refuses any other word and never masks.
///
/// `0x0000` is the Reserved one-byte id, and the three-byte `00 00 00` is
/// `0x8000`; both write as three zero bytes, and [`from_sysex_bytes`](Self::from_sysex_bytes)
/// reads three zero bytes as `0x8000`, as the specification's conversion code
/// does (a first byte of `00` selects the three-byte form).
///
/// ```
/// use tutti_ump::{ManufacturerId, U7};
///
/// // A 21-bit id, 00 21 09 (Table 21's style of example).
/// let id = ManufacturerId::from_sysex_bytes([0x00, 0x21, 0x09].map(U7::of)).unwrap();
/// assert_eq!(id.get(), 0xA109);
/// assert_eq!(id.sysex_bytes().map(U7::get), [0x00, 0x21, 0x09]);
/// // A 7-bit id, 0x41, is 0x0041 and writes as `41 00 00` in a Device Identity.
/// let id = ManufacturerId::from_sysex_bytes([0x41, 0, 0].map(U7::of)).unwrap();
/// assert_eq!(id.get(), 0x0041);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u16", into = "u16"))]
pub struct ManufacturerId(u16);

/// A word that is no [`ManufacturerId`]: it is neither a 7-bit id nor a 21-bit
/// id in the 16-bit form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{:#06x} is not a 16-bit manufacturer id", .0)]
pub struct NotManufacturerId(pub u16);

impl ManufacturerId {
    /// The Reserved id, `0x0000` (Table 20).
    pub const RESERVED: Self = Self(0x0000);
    /// Non-Commercial / Research, No Public Release: `0x007D`.
    pub const NON_COMMERCIAL: Self = Self(0x007D);
    /// Universal System Exclusive, Non-Real Time: `0x007E`.
    pub const UNIVERSAL_NON_REAL_TIME: Self = Self(0x007E);
    /// Universal System Exclusive, Real Time: `0x007F`.
    pub const UNIVERSAL_REAL_TIME: Self = Self(0x007F);

    /// The id of the word `raw`, or `None` if it is neither form.
    #[inline]
    #[must_use]
    pub const fn new(raw: u16) -> Option<Self> {
        let valid = if raw & 0x8000 == 0 {
            raw <= 0x007F
        } else {
            raw & 0x0080 == 0
        };
        if valid { Some(Self(raw)) } else { None }
    }

    /// The id for a literal or a constant.
    ///
    /// # Panics
    ///
    /// When `raw` is no id; in a `const` that is a compile error.
    #[inline]
    #[must_use]
    #[track_caller]
    pub const fn of(raw: u16) -> Self {
        match Self::new(raw) {
            Some(id) => id,
            None => panic!("not a 16-bit manufacturer id"),
        }
    }

    /// The word.
    #[inline]
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }

    /// The id of three System Exclusive id bytes as a Device Inquiry reply or a
    /// Device Identity carries them: a first byte of `00` selects the 21-bit id of
    /// the other two (`0x8000 | b2 << 8 | b3`), any other first byte is the one-byte
    /// id and the other two are zero (M2-104 section 7.10, "Example Conversion
    /// Code", and section 7.1.3: "the remaining 2 bytes are filled with zeroes").
    /// `None` for a one-byte id followed by a non-zero byte, which the code would
    /// read as the one-byte id alone and so not give back.
    #[inline]
    #[must_use]
    pub const fn from_sysex_bytes(bytes: [U7; 3]) -> Option<Self> {
        if bytes[0].get() == 0 {
            Some(Self(
                0x8000 | (bytes[1].get() as u16) << 8 | bytes[2].get() as u16,
            ))
        } else if bytes[1].get() == 0 && bytes[2].get() == 0 {
            Some(Self(bytes[0].get() as u16))
        } else {
            None
        }
    }

    /// The three System Exclusive id bytes: `xx 00 00` for a one-byte id (what a
    /// Device Identity carries, M2-104 section 7.1.3) and `00 xx yy` for a 21-bit one.
    #[inline]
    #[must_use]
    pub const fn sysex_bytes(self) -> [U7; 3] {
        if self.0 & 0x8000 == 0 {
            [
                U7::masked((self.0 & 0x7F) as u8),
                U7::masked(0),
                U7::masked(0),
            ]
        } else {
            [
                U7::masked(0),
                U7::masked((self.0 >> 8) as u8),
                U7::masked(self.0 as u8),
            ]
        }
    }

    /// Whether this is a 21-bit id (three bytes on a MIDI 1.0 wire).
    #[inline]
    #[must_use]
    pub const fn is_extended(self) -> bool {
        self.0 & 0x8000 != 0
    }
}

impl TryFrom<u16> for ManufacturerId {
    type Error = NotManufacturerId;

    #[inline]
    fn try_from(raw: u16) -> Result<Self, NotManufacturerId> {
        Self::new(raw).ok_or(NotManufacturerId(raw))
    }
}

impl From<ManufacturerId> for u16 {
    #[inline]
    fn from(id: ManufacturerId) -> u16 {
        id.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(b: [u8; 3]) -> [U7; 3] {
        b.map(U7::of)
    }

    /// M2-104 section 7.10, "7-Bit (1-byte) Manufacturer IDs" and Table 20:
    /// the high byte is `00` and the low byte is the MIDI 1.0 value.
    #[test]
    fn a_one_byte_id_is_the_byte_in_the_low_half() {
        for (byte, word) in [
            (0x41, 0x0041),
            (0x7D, 0x007D),
            (0x7E, 0x007E),
            (0x7F, 0x007F),
        ] {
            let id = ManufacturerId::from_sysex_bytes(bytes([byte, 0, 0])).unwrap();
            assert_eq!(id.get(), word);
            assert!(!id.is_extended());
            assert_eq!(id.sysex_bytes(), bytes([byte, 0, 0]));
        }
        assert_eq!(ManufacturerId::NON_COMMERCIAL.get(), 0x007D);
        assert_eq!(ManufacturerId::UNIVERSAL_NON_REAL_TIME.get(), 0x007E);
        assert_eq!(ManufacturerId::UNIVERSAL_REAL_TIME.get(), 0x007F);
    }

    /// Section 7.10, "21-Bit (3-byte) Manufacturer IDs": the leading `00` is
    /// replaced by the top bit of the word, bytes 2 and 3 go to the high and
    /// low bytes (`0x8000 | MFID_2 << 8 | MFID_3`).
    #[test]
    fn a_three_byte_id_sets_the_top_bit_and_keeps_its_last_two_bytes() {
        let id = ManufacturerId::from_sysex_bytes(bytes([0x00, 0x21, 0x09])).unwrap();
        assert_eq!(id.get(), 0xA109);
        assert!(id.is_extended());
        assert_eq!(id.sysex_bytes(), bytes([0x00, 0x21, 0x09]));
        let top = ManufacturerId::from_sysex_bytes(bytes([0x00, 0x7F, 0x7F])).unwrap();
        assert_eq!(top.get(), 0xFF7F);
    }

    /// Section 7.1.3: a one-byte id is followed by zeroes, so a nonzero byte
    /// after one is no id (reading it as the one-byte id would drop it).
    #[test]
    fn a_nonzero_first_byte_with_anything_after_it_is_refused() {
        assert_eq!(
            ManufacturerId::from_sysex_bytes(bytes([0x41, 0x12, 0x34])),
            None
        );
        assert_eq!(
            ManufacturerId::from_sysex_bytes(bytes([0x41, 0x00, 0x01])),
            None
        );
        assert_eq!(
            ManufacturerId::from_sysex_bytes(bytes([0x41, 0x01, 0x00])),
            None
        );
    }

    /// A word is an id only if it is `0x00xx` with `xx` 7-bit, or has the top
    /// bit set and both bytes 7-bit.
    #[test]
    fn a_word_that_is_neither_form_is_refused() {
        for bad in [0x0080u16, 0x0100, 0x7F00, 0x7FFF, 0x8080, 0xFFFF, 0x80FF] {
            assert_eq!(ManufacturerId::new(bad), None, "{bad:#x}");
            assert_eq!(ManufacturerId::try_from(bad), Err(NotManufacturerId(bad)));
        }
        for good in [0x0000u16, 0x007F, 0x8000, 0x8001, 0xFF7F] {
            assert_eq!(ManufacturerId::new(good).map(u16::from), Some(good));
        }
    }

    /// Every word `new` accepts survives the three-byte form, but the Reserved
    /// `0x0000`, which shares `00 00 00` with `0x8000` and reads as it.
    #[test]
    fn every_id_survives_the_three_byte_form() {
        for raw in 0..=u16::MAX {
            let Some(id) = ManufacturerId::new(raw) else {
                continue;
            };
            let back = ManufacturerId::from_sysex_bytes(id.sysex_bytes()).unwrap();
            if raw == 0 {
                assert_eq!(back.get(), 0x8000);
            } else {
                assert_eq!(back, id, "{raw:#x}");
            }
        }
    }
}
