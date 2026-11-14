//! The values of Profile Configuration (M2-101 §7, M2-102 §2): a Profile Id, the
//! number of Channels a Profile uses, the two lists of a Reply to Profile Inquiry,
//! and the Inquiry Target and data of the Details messages.
//!
//! The messages themselves are [`CiMessage`](super::CiMessage)'s variants; the
//! Initiator's state over them is [`ProfileInitiator`](super::ProfileInitiator).
//!
//! Each message's destination follows its table in M2-101 §7. The four reports
//! (Added, Removed, Enabled, Disabled) always go to the Broadcast MUID and carry
//! no `to`; the Inquiry, the Reply, Set On, Set Off and the two Details messages
//! go to one MUID; Profile Specific Data carries a [`Destination`](super::Destination),
//! since §7.12 allows the Broadcast MUID only where the Profile defines it. A
//! message of the wrong kind decodes to an [`Unread`](super::Unread) with
//! [`WrongDestination`](super::UnreadReason::WrongDestination). A Message Format
//! Version 1 sender has no Number of Channels field; it reads as 0, which §5.3's rule to
//! read only the fields of the received version asks for.
//!
//! [`ProfileId::from_bytes`] refuses what [`ProfileId::to_bytes`] could not give
//! back, a one-byte id followed by a non-zero byte (§7.3.1 fills the rest
//! with zeroes), and a manufacturer Profile cannot carry the id `7E`, which would
//! read back as a Standard Profile. [`ProfileLists`] holds at most 16383 Profiles
//! per list, the most two 7-bit bytes can count, so a Reply it builds is always
//! writable.

use std::vec::Vec;

use tutti_ump::{ManufacturerId, U7, U14};

/// Profile Inquiry, Sub-ID#2 `0x20` (§7.2).
pub const SUB_ID2_PROFILE_INQUIRY: u8 = 0x20;
/// Reply to Profile Inquiry (§7.3).
pub const SUB_ID2_PROFILE_REPLY: u8 = 0x21;
/// Set Profile On (§7.8).
pub const SUB_ID2_SET_PROFILE_ON: u8 = 0x22;
/// Set Profile Off (§7.9).
pub const SUB_ID2_SET_PROFILE_OFF: u8 = 0x23;
/// Profile Enabled Report (§7.10).
pub const SUB_ID2_PROFILE_ENABLED: u8 = 0x24;
/// Profile Disabled Report (§7.11).
pub const SUB_ID2_PROFILE_DISABLED: u8 = 0x25;
/// Profile Added Report (§7.4).
pub const SUB_ID2_PROFILE_ADDED: u8 = 0x26;
/// Profile Removed Report (§7.5).
pub const SUB_ID2_PROFILE_REMOVED: u8 = 0x27;
/// Profile Details Inquiry (§7.6).
pub const SUB_ID2_PROFILE_DETAILS_INQUIRY: u8 = 0x28;
/// Reply to Profile Details Inquiry (§7.7).
pub const SUB_ID2_PROFILE_DETAILS_REPLY: u8 = 0x29;
/// Profile Specific Data (§7.12).
pub const SUB_ID2_PROFILE_SPECIFIC_DATA: u8 = 0x2F;

/// The first Profile Id byte of a Standard Defined Profile (§7.3.1).
const STANDARD_BYTE: u8 = 0x7E;

/// A Standard Defined Profile's Id (M2-102 §2.2, Table 5): the Profile Number
/// Bank, Profile Number, Profile Version and Profile Level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StandardProfile {
    bank: U7,
    number: U7,
    version: U7,
    level: U7,
}

impl StandardProfile {
    /// The MPE Profile (M2-120 section 3.2, Table 4): bank `0x31`, number
    /// `0x00`, version `0x01`, level `0x01`.
    pub const MPE: Self = Self::new(U7::of(0x31), U7::of(0x00), U7::of(0x01), U7::of(0x01));

    /// A Standard Defined Profile. Every combination of the four bytes is one;
    /// what each means is the Profile's specification's (byte 5, the level, is
    /// M2-102 §3.2's).
    #[must_use]
    pub const fn new(bank: U7, number: U7, version: U7, level: U7) -> Self {
        Self {
            bank,
            number,
            version,
            level,
        }
    }

    /// The Profile Number Bank (byte 2).
    #[must_use]
    pub const fn bank(self) -> U7 {
        self.bank
    }

    /// The Profile Number (byte 3).
    #[must_use]
    pub const fn number(self) -> U7 {
        self.number
    }

    /// The Profile Version (byte 4).
    #[must_use]
    pub const fn version(self) -> U7 {
        self.version
    }

    /// The Profile Level (byte 5).
    #[must_use]
    pub const fn level(self) -> U7 {
        self.level
    }
}

/// A manufacturer's own Profile (M2-101 §7.3.1, Table 19): its System Exclusive
/// id in bytes 1 to 3 and `Manufacturer Specific Info` in bytes 4 and 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ManufacturerProfile {
    manufacturer: ManufacturerId,
    info: [U7; 2],
}

impl ManufacturerProfile {
    /// A manufacturer's Profile, or `None` for the id `7E`: a first byte of `0x7E`
    /// is a Standard Defined Profile, so a manufacturer with that id could not be
    /// read back.
    #[must_use]
    pub fn new(manufacturer: ManufacturerId, info: [U7; 2]) -> Option<Self> {
        (manufacturer != ManufacturerId::UNIVERSAL_NON_REAL_TIME)
            .then_some(Self { manufacturer, info })
    }

    /// The manufacturer's System Exclusive id.
    #[must_use]
    pub const fn manufacturer(self) -> ManufacturerId {
        self.manufacturer
    }

    /// The two bytes of `Manufacturer Specific Info`: up to 16384 different
    /// Manufacturer Profile numbers.
    #[must_use]
    pub const fn info(self) -> [U7; 2] {
        self.info
    }
}

/// A Profile Id, five bytes (M2-101 §7.3.1): a Standard Defined Profile (first
/// byte `0x7E`) or a manufacturer's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProfileId {
    /// Adopted by AMEI and MA.
    Standard(StandardProfile),
    /// A manufacturer's own.
    Manufacturer(ManufacturerProfile),
}

impl ProfileId {
    /// The MPE Profile ([`StandardProfile::MPE`]).
    pub const MPE: Self = Self::Standard(StandardProfile::MPE);

    /// The five wire bytes.
    #[must_use]
    pub fn to_bytes(self) -> [u8; 5] {
        match self {
            Self::Standard(p) => [
                STANDARD_BYTE,
                p.bank.get(),
                p.number.get(),
                p.version.get(),
                p.level.get(),
            ],
            Self::Manufacturer(p) => {
                let [a, b, c] = p.manufacturer.sysex_bytes().map(U7::get);
                [a, b, c, p.info[0].get(), p.info[1].get()]
            }
        }
    }

    /// The Profile Id of five wire bytes, or `None` for a byte with its top bit
    /// set, or a manufacturer id that is not one (a one-byte id is followed by
    /// two zeroes, §7.3.1).
    #[must_use]
    pub fn from_bytes(bytes: [u8; 5]) -> Option<Self> {
        let [a, b, c, d, e] = bytes.map(U7::new);
        let (a, b, c, d, e) = (a?, b?, c?, d?, e?);
        if a.get() == STANDARD_BYTE {
            return Some(Self::Standard(StandardProfile::new(b, c, d, e)));
        }
        let manufacturer = ManufacturerId::from_sysex_bytes([a, b, c])?;
        Some(Self::Manufacturer(ManufacturerProfile::new(
            manufacturer,
            [d, e],
        )?))
    }
}

/// The number of Channels a Profile uses or is asked to use (§7.8, §7.10,
/// §7.11): the Manager plus Member Channels, 14 bits.
///
/// Zero is a value the spec gives a meaning: when the Profile Destination is
/// address 0x7E or 0x7F, the number of Channels is the
/// width of the Group or Function Block, and the field is then 0
/// (`BY_ADDRESS`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProfileChannels(U14);

impl ProfileChannels {
    /// `0x0000`: the width of the Group or Function Block addressed.
    pub(crate) const BY_ADDRESS: Self = Self(U14::of(0));
    /// The most the field holds, 16383.
    pub const MAX: u16 = U14::MAX.get();

    /// A count, or `None` above 14 bits.
    #[must_use]
    pub const fn new(count: u16) -> Option<Self> {
        match U14::new(count) {
            Some(count) => Some(Self(count)),
            None => None,
        }
    }

    /// A count for a literal or a constant.
    ///
    /// # Panics
    ///
    /// Above 14 bits; in a `const` that is a compile error.
    #[must_use]
    #[track_caller]
    pub const fn of(count: u16) -> Self {
        Self(U14::of(count))
    }

    /// The count.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0.get()
    }

    /// Whether the field is 0, the Group or Function Block's own width.
    #[must_use]
    pub const fn is_by_address(self) -> bool {
        self.0.get() == 0
    }

    pub(crate) const fn from_wire(raw: U14) -> Self {
        Self(raw)
    }
}

/// Whether a Profile is active (§7.3.2, §7.3.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProfileState {
    /// Supported and currently enabled.
    Enabled,
    /// Supported but currently disabled.
    Disabled,
}

/// A list the wire cannot hold: a count is 14 bits (§7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{} Profile Ids do not fit a 14-bit count (at most {})", .len, ProfileLists::MAX_LEN)]
pub struct ListTooLong {
    /// The length refused.
    pub len: usize,
}

/// The two lists of a Reply to Profile Inquiry (§7.3): the Profiles supported and
/// currently enabled, and those supported but currently disabled. Each holds at
/// most `MAX_LEN` Ids, the largest count the message carries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProfileLists {
    enabled: Vec<ProfileId>,
    disabled: Vec<ProfileId>,
}

impl ProfileLists {
    /// The longest list: the count is two 7-bit bytes.
    pub(crate) const MAX_LEN: usize = 0x3FFF;

    /// Two lists, or the length of the one that does not fit.
    ///
    /// # Errors
    ///
    /// [`ListTooLong`] when either list is longer than `MAX_LEN`.
    pub fn new(enabled: Vec<ProfileId>, disabled: Vec<ProfileId>) -> Result<Self, ListTooLong> {
        for list in [&enabled, &disabled] {
            if list.len() > Self::MAX_LEN {
                return Err(ListTooLong { len: list.len() });
            }
        }
        Ok(Self { enabled, disabled })
    }

    /// Both lists empty: no Profiles reported (§7.13).
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The Profiles currently enabled.
    #[must_use]
    pub fn enabled(&self) -> &[ProfileId] {
        &self.enabled
    }

    /// The Profiles supported and currently disabled.
    #[must_use]
    pub fn disabled(&self) -> &[ProfileId] {
        &self.disabled
    }

    pub(crate) fn encode_body(&self, out: &mut impl Extend<u8>) {
        for list in [&self.enabled, &self.disabled] {
            // `new` holds each length to 14 bits.
            out.extend([(list.len() & 0x7F) as u8, ((list.len() >> 7) & 0x7F) as u8]);
            for id in list {
                out.extend(id.to_bytes());
            }
        }
    }
}

/// The Inquiry Target of the Details messages (§7.6.1): `0x00` to `0x3F` is
/// Registered Target Data, `0x40` to `0x7F` Profile Specific Target Data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InquiryTarget(U7);

impl InquiryTarget {
    /// `Number of MIDI Channels`, the one Registered Target of M2-102 §2.5.1.
    pub const NUMBER_OF_CHANNELS: Self = Self(U7::of(0x00));

    /// A target: any 7-bit value is one.
    #[must_use]
    pub const fn new(raw: U7) -> Self {
        Self(raw)
    }

    /// The target byte.
    #[must_use]
    pub const fn get(self) -> U7 {
        self.0
    }

    /// Whether it is Registered Target Data (`0x00` to `0x3F`), whose format is
    /// the same for every Profile.
    #[must_use]
    pub const fn is_registered(self) -> bool {
        self.0.get() < 0x40
    }
}

/// Why bytes are not Profile data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, thiserror::Error)]
pub enum ProfileDataError {
    /// A byte has its top bit set: System Exclusive carries 7-bit data.
    #[error("a data byte has its top bit set")]
    NotSevenBit,
    /// More bytes than the message's length field holds.
    #[error("more data than the length field holds")]
    TooLong,
}

macro_rules! seven_bit_data {
    ($(#[$doc:meta])* $name:ident, $max:expr) => {
        $(#[$doc])*
        #[derive(Clone, Debug, Default, PartialEq, Eq)]
        pub struct $name(Vec<u8>);

        impl $name {
            /// The most bytes the length field holds.
            pub const MAX_LEN: usize = $max;

            /// The data, or why it cannot be carried.
            ///
            /// # Errors
            ///
            /// [`ProfileDataError`] for a byte with its top bit set, or more than
            /// [`MAX_LEN`](Self::MAX_LEN) bytes.
            pub fn new(bytes: Vec<u8>) -> Result<Self, ProfileDataError> {
                if bytes.len() > Self::MAX_LEN {
                    Err(ProfileDataError::TooLong)
                } else if bytes.iter().any(|b| b & 0x80 != 0) {
                    Err(ProfileDataError::NotSevenBit)
                } else {
                    Ok(Self(bytes))
                }
            }

            /// The bytes.
            #[must_use]
            pub fn as_bytes(&self) -> &[u8] {
                &self.0
            }

            /// The bytes, by value.
            #[must_use]
            pub fn into_bytes(self) -> Vec<u8> {
                self.0
            }
        }
    };
}

seven_bit_data!(
    /// The Inquiry Target Data of a Reply to Profile Details Inquiry (§7.7.3): at
    /// most 16383 bytes (the length is 14 bits), 7-bit. Its format is the target's.
    DetailsData,
    0x3FFF
);

seven_bit_data!(
    /// The data of a Profile Specific Data message (§7.12): 7-bit bytes, at most
    /// 2^28 - 1 of them (the length is four 7-bit bytes). Their meaning is the
    /// Profile's.
    SpecificData,
    0x0FFF_FFFF
);

/// The Registered Target Data `Number of MIDI Channels` (M2-102 §2.5.1, Table 8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChannelsDetail {
    /// The number of Channels currently in use by this Profile, 0 when it is
    /// not enabled.
    pub in_use: ProfileChannels,
    /// The maximum number of Channels available for use by this Profile.
    pub max: ProfileChannels,
}

impl ChannelsDetail {
    /// The data of a reply to [`InquiryTarget::NUMBER_OF_CHANNELS`]: length 4.
    #[must_use]
    pub fn to_data(self) -> DetailsData {
        let [a, b] = split(self.in_use);
        let [c, d] = split(self.max);
        DetailsData(vec![a, b, c, d])
    }
}

fn split(count: ProfileChannels) -> [u8; 2] {
    [(count.get() & 0x7F) as u8, (count.get() >> 7) as u8]
}

impl DetailsData {
    /// The data read as [`ChannelsDetail`], or `None` when it is not four bytes.
    /// It means that only for a reply to [`InquiryTarget::NUMBER_OF_CHANNELS`].
    #[must_use]
    pub fn as_channels(&self) -> Option<ChannelsDetail> {
        let [a, b, c, d] = self.0[..] else {
            return None;
        };
        let count = |lsb: u8, msb: u8| {
            ProfileChannels::from_wire(U14::masked(u16::from(msb) << 7 | u16::from(lsb)))
        };
        Some(ChannelsDetail {
            in_use: count(a, b),
            max: count(c, d),
        })
    }
}

#[cfg(test)]
mod tests;
