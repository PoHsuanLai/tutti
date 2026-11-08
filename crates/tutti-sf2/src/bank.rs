use std::fmt;

use tutti_ump::U7;

/// A preset's bank (SF2 2.04 section 7.2, `wBank`): a MIDI bank 0 to 127, or
/// the General MIDI percussion bank, which a `wBank` of 128 names.
///
/// `new` refuses what the field cannot mean; a preset whose bank is not one of
/// these cannot be played but is kept (section 7.2), and says so by having no
/// [`Preset::bank`](crate::Preset::bank).
///
/// With the `serde` feature it serialises as the bare `wBank` and refuses what
/// `new` refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "u16", into = "u16")
)]
pub enum Bank {
    /// A MIDI bank, 0 to 127.
    Melodic(U7),
    /// The percussion bank, `wBank` 128.
    Percussion,
}

impl Bank {
    /// The `wBank` of the percussion bank.
    pub const PERCUSSION_NUMBER: u16 = 128;

    /// The bank a `wBank` names, or `None` above 128. Never clamps.
    #[must_use]
    pub const fn new(raw: u16) -> Option<Self> {
        if raw == Self::PERCUSSION_NUMBER {
            return Some(Self::Percussion);
        }
        // `U7::new` refuses above 127, so 129 and up end here; the narrowing
        // cannot wrap one into range.
        if raw > 127 {
            return None;
        }
        match U7::new(raw as u8) {
            Some(bank) => Some(Self::Melodic(bank)),
            None => None,
        }
    }

    /// The bank's `wBank`.
    #[must_use]
    pub const fn get(self) -> u16 {
        match self {
            Self::Melodic(bank) => bank.get() as u16,
            Self::Percussion => Self::PERCUSSION_NUMBER,
        }
    }
}

/// The refused `wBank`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BankError(pub u16);

impl fmt::Display for BankError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is not a bank: 0 to 127, or 128 for percussion",
            self.0
        )
    }
}

impl std::error::Error for BankError {}

impl TryFrom<u16> for Bank {
    type Error = BankError;
    fn try_from(raw: u16) -> Result<Self, BankError> {
        Self::new(raw).ok_or(BankError(raw))
    }
}

impl From<Bank> for u16 {
    fn from(bank: Bank) -> u16 {
        bank.get()
    }
}

impl fmt::Display for Bank {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 7.2: 0 to 127 and 128 are banks; nothing else is, and nothing is clamped.
    ///
    /// Mutation (run): `None => Some(Self::Percussion)` in `Bank::new` accepts
    /// every raw and fails the invalid rows.
    #[test]
    fn a_bank_is_0_to_127_or_the_percussion_128() {
        for raw in [0, 1, 127, 128] {
            let bank = Bank::new(raw).unwrap_or_else(|| panic!("{raw}"));
            assert_eq!(bank.get(), raw);
            assert_eq!(Bank::try_from(raw), Ok(bank));
            assert_eq!(u16::from(bank), raw);
        }
        for raw in [129, 255, 256, u16::MAX] {
            assert_eq!(Bank::new(raw), None, "{raw}");
            assert_eq!(Bank::try_from(raw), Err(BankError(raw)));
        }
        assert_eq!(Bank::new(128), Some(Bank::Percussion));
        assert!(BankError(300).to_string().contains("300"));
    }
}
