//! The values MIDI Machine Control commands and information fields are made
//! of: time code, speeds, track bitmaps, user bits and names (RP-013 pp.8 to 12).

use tutti_ump::{U4, U7};

use super::MmcBytes;
use tutti_types::Seconds;

use crate::smpte::{MtcRate, SmpteTimecode};
use crate::universal::MtcTimeCode;

/// Declares a fieldless enum with a wire byte for each variant.
macro_rules! wire_enum {
    (
        $(#[$meta:meta])*
        $name:ident {
            $($(#[$vmeta:meta])* $variant:ident = $value:literal),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant),+
        }

        impl $name {
            /// The variant a wire byte names, or `None` for a byte the
            /// specification leaves undefined.
            #[must_use]
            pub const fn from_wire(byte: u8) -> Option<Self> {
                match byte {
                    $($value => Some(Self::$variant),)+
                    _ => None,
                }
            }

            /// The wire byte of the variant.
            #[must_use]
            pub const fn wire(self) -> u8 {
                match self {
                    $(Self::$variant => $value),+
                }
            }
        }
    };
}
pub(super) use wire_enum;

wire_enum! {
    /// The direction of a speed or a step (RP-013 pp.11 and 30): the `g` bit.
    MmcDirection {
        /// `0`: forward.
        Forward = 0,
        /// `1`: reverse.
        Reverse = 1,
    }
}

wire_enum! {
    /// The sign of a time code or short time code (RP-013 pp.8 and 9): the
    /// `g` bit of the frames byte.
    MmcSign {
        /// `0`: zero or above.
        Positive = 0,
        /// `1`: below zero, where the field permits signed time code.
        Negative = 1,
    }
}

/// A fraction of a frame in hundredths, `0` to `99` (RP-013 p.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcSubframes(u8);

/// A subframe count of 100 or more.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0} is not a subframe count (0 to 99)")]
pub struct NotMmcSubframes(pub u8);

impl MmcSubframes {
    /// The count `raw`, or `None` from 100 up.
    #[must_use]
    pub const fn new(raw: u8) -> Option<Self> {
        if raw < 100 { Some(Self(raw)) } else { None }
    }

    /// The count for a literal or a constant.
    ///
    /// # Panics
    ///
    /// From 100 up; in a `const` that is a compile error.
    #[must_use]
    #[track_caller]
    pub const fn of(raw: u8) -> Self {
        match Self::new(raw) {
            Some(count) => count,
            None => panic!("a subframe count is 0 to 99"),
        }
    }

    /// The count in hundredths of a frame.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl From<MmcSubframes> for u8 {
    fn from(count: MmcSubframes) -> Self {
        count.0
    }
}

impl TryFrom<u8> for MmcSubframes {
    type Error = NotMmcSubframes;

    fn try_from(raw: u8) -> Result<Self, NotMmcSubframes> {
        Self::new(raw).ok_or(NotMmcSubframes(raw))
    }
}

/// The hours, minutes, seconds and frames of a time code with the flags that
/// ride in their spare bits (RP-013 p.8).
///
/// The position and its frame rate are an [`MtcTimeCode`]'s: the labels with
/// their ranges and the two-bit rate code in the hours byte. A **blank** label stands for time code never loaded and reads as all
/// zeroes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcTimeLabel {
    time_code: MtcTimeCode,
    color_frame: bool,
    blank: bool,
    sign: MmcSign,
}

impl MmcTimeLabel {
    /// The label of `position`, positive and not colour framed.
    #[must_use]
    pub const fn new(time_code: MtcTimeCode) -> Self {
        Self {
            time_code,
            color_frame: false,
            blank: false,
            sign: MmcSign::Positive,
        }
    }

    /// The blank label at `rate`: time code never loaded, `00:00:00:00`.
    #[must_use]
    pub const fn blank(rate: MtcRate) -> Self {
        Self {
            time_code: MtcTimeCode::origin(rate),
            color_frame: false,
            blank: true,
            sign: MmcSign::Positive,
        }
    }

    /// This label with the colour-frame flag set to `color_frame`.
    #[must_use]
    pub const fn with_color_frame(self, color_frame: bool) -> Self {
        Self {
            color_frame,
            ..self
        }
    }

    /// This label with `sign`, negative only where the field permits signed
    /// time code.
    #[must_use]
    pub const fn with_sign(self, sign: MmcSign) -> Self {
        Self { sign, ..self }
    }

    /// The labels and their frame rate.
    #[must_use]
    pub const fn time_code(self) -> MtcTimeCode {
        self.time_code
    }

    /// Whether the code is colour framed.
    #[must_use]
    pub const fn color_frame(self) -> bool {
        self.color_frame
    }

    /// Whether time code was never loaded into the field.
    #[must_use]
    pub const fn is_blank(self) -> bool {
        self.blank
    }

    /// The sign of the value.
    #[must_use]
    pub const fn sign(self) -> MmcSign {
        self.sign
    }

    /// The four bytes `hr mn sc fr`, the last without its final-byte flag.
    pub(super) fn head(self) -> [u8; 4] {
        let p = self.time_code;
        [
            p.rate().code() << 5 | p.timecode().hours(),
            u8::from(self.color_frame) << 6 | p.timecode().minutes(),
            u8::from(self.blank) << 6 | p.timecode().seconds(),
            self.sign.wire() << 6 | p.timecode().frames(),
        ]
    }

    /// The label of four data bytes and whether the last said status;
    /// `None` for a byte over 127, a field out of range, or a blank label with
    /// numbers in it.
    pub(super) const fn from_head(hr: u8, mn: u8, sc: u8, fr: u8) -> Option<(Self, bool)> {
        if hr >= 0x80 || mn >= 0x80 || sc >= 0x80 || fr >= 0x80 {
            return None;
        }
        let rate = MtcRate::masked(hr >> 5);
        let (h, m, s, f) = (hr & 0x1F, mn & 0x3F, sc & 0x3F, fr & 0x1F);
        let label = if sc & 0x40 != 0 {
            if h | m | s | f != 0 {
                return None;
            }
            Self::blank(rate)
        } else {
            match MtcTimeCode::new(rate, h, m, s, f) {
                Some(time_code) => Self::new(time_code),
                None => return None,
            }
        };
        let label = label
            .with_color_frame(mn & 0x40 != 0)
            .with_sign(sign_of(fr));
        Some((label, fr & 0x20 != 0))
    }
}

/// The status byte that stands in for subframes: where the code came from and
/// whether to trust it (RP-013 p.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcCodeStatus {
    /// `e`: tach or control track updated code, not read time code.
    pub estimated: bool,
    /// `v`: the number's validity cannot be confirmed (ignored if `estimated`).
    pub invalid: bool,
    /// `d`: the first frame of a four or eight field video sequence.
    pub field_one: bool,
    /// `n`: no time code has been read since power up or an MMC RESET.
    pub no_time_code: bool,
}

impl MmcCodeStatus {
    pub(super) fn wire(self) -> u8 {
        u8::from(self.estimated) << 6
            | u8::from(self.invalid) << 5
            | u8::from(self.field_one) << 4
            | u8::from(self.no_time_code) << 3
    }

    /// `None` when a reserved bit is set or the byte is not a data byte.
    pub(super) const fn from_wire(byte: u8) -> Option<Self> {
        if byte >= 0x80 || byte & 0x07 != 0 {
            return None;
        }
        Some(Self {
            estimated: byte & 0x40 != 0,
            invalid: byte & 0x20 != 0,
            field_one: byte & 0x10 != 0,
            no_time_code: byte & 0x08 != 0,
        })
    }
}

/// The last byte of a time code: subframes, or status in their place
/// (RP-013 p.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MmcTimeTail {
    /// Fractional frames, type `{ff}`.
    Subframes(MmcSubframes),
    /// Code status, type `{st}`.
    Status(MmcCodeStatus),
}

impl MmcTimeTail {
    pub(super) fn wire(self) -> (bool, u8) {
        match self {
            Self::Subframes(count) => (false, count.get()),
            Self::Status(status) => (true, status.wire()),
        }
    }

    pub(super) const fn from_wire(is_status: bool, byte: u8) -> Option<Self> {
        if is_status {
            match MmcCodeStatus::from_wire(byte) {
                Some(status) => Some(Self::Status(status)),
                None => None,
            }
        } else {
            match MmcSubframes::new(byte) {
                Some(count) => Some(Self::Subframes(count)),
                None => None,
            }
        }
    }
}

/// The direction in bit 6 of `byte`.
pub(super) const fn direction_of(byte: u8) -> MmcDirection {
    if byte & 0x40 != 0 {
        MmcDirection::Reverse
    } else {
        MmcDirection::Forward
    }
}

/// The sign in bit 6 of `byte`.
pub(super) const fn sign_of(byte: u8) -> MmcSign {
    if byte & 0x40 != 0 {
        MmcSign::Negative
    } else {
        MmcSign::Positive
    }
}

/// The standard time code of five bytes, `hr mn sc fr` and subframes or
/// status (RP-013 p.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcTimeCode {
    /// The labels.
    pub label: MmcTimeLabel,
    /// Subframes or status.
    pub tail: MmcTimeTail,
}

/// A time code that carries subframes (type `{ff}`): what a locate target and
/// a general purpose register hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcPosition {
    /// The labels.
    pub label: MmcTimeLabel,
    /// The fraction of a frame.
    pub subframes: MmcSubframes,
}

impl MmcPosition {
    /// The position of `seconds` at `rate`: the label of the frame it falls in
    /// and the hundredths of the way through that frame. `None` for a time no
    /// label names: before zero, NaN, or 24 hours and beyond.
    #[must_use]
    pub fn from_seconds(seconds: Seconds, rate: MtcRate) -> Option<Self> {
        if !(0.0..86_400.0).contains(&seconds.get()) {
            return None;
        }
        let frame_rate = rate.frame_rate();
        let frames = seconds.get() * frame_rate.fps();
        let whole = frames.floor();
        // `whole` is a finite whole number from 0 below one day of frames.
        let label = SmpteTimecode::from_frame_count(whole as u64, frame_rate);
        let hundredths = ((frames - whole) * 100.0).floor().clamp(0.0, 99.0);
        Some(Self {
            label: MmcTimeLabel::new(MtcTimeCode::new(
                rate,
                label.hours(),
                label.minutes(),
                label.seconds(),
                label.frames(),
            )?),
            // `hundredths` is a whole number in 0..=99.
            subframes: MmcSubframes::new(hundredths as u8)?,
        })
    }
}

/// A time code whose last byte is status, not subframes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the time code carries status, not subframes")]
pub struct NotMmcPosition(pub MmcTimeCode);

impl From<MmcPosition> for MmcTimeCode {
    fn from(position: MmcPosition) -> Self {
        Self {
            label: position.label,
            tail: MmcTimeTail::Subframes(position.subframes),
        }
    }
}

impl TryFrom<MmcTimeCode> for MmcPosition {
    type Error = NotMmcPosition;

    fn try_from(code: MmcTimeCode) -> Result<Self, NotMmcPosition> {
        match code.tail {
            MmcTimeTail::Subframes(subframes) => Ok(Self {
                label: code.label,
                subframes,
            }),
            MmcTimeTail::Status(_) => Err(NotMmcPosition(code)),
        }
    }
}

/// The two-byte short time code, frames and subframes or status
/// (RP-013 p.9): hours, minutes and seconds are left out until they change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcShortTimeCode {
    frames: u8,
    sign: MmcSign,
    tail: MmcTimeTail,
}

impl MmcShortTimeCode {
    /// The short code of `frames` (0..=29) and `tail`, or `None` for 30 or more.
    #[must_use]
    pub const fn new(frames: u8, sign: MmcSign, tail: MmcTimeTail) -> Option<Self> {
        if frames < 30 {
            Some(Self { frames, sign, tail })
        } else {
            None
        }
    }

    /// Frames, 0..=29.
    #[must_use]
    pub const fn frames(self) -> u8 {
        self.frames
    }

    /// The sign of the value.
    #[must_use]
    pub const fn sign(self) -> MmcSign {
        self.sign
    }

    /// Subframes or status.
    #[must_use]
    pub const fn tail(self) -> MmcTimeTail {
        self.tail
    }
}

/// A speed and direction in the standard three bytes (RP-013 p.11): a 17 bit
/// mantissa with its binary point `3 + shift` bits from the top, so its value
/// is a multiple of play speed of `mantissa / 2^(14 - shift)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcSpeed {
    direction: MmcDirection,
    shift: u8,
    mantissa: u32,
}

impl MmcSpeed {
    /// Forward play speed: one times, shift 0.
    pub const PLAY: Self = Self {
        direction: MmcDirection::Forward,
        shift: 0,
        mantissa: 1 << 14,
    };

    /// The speed of `mantissa` (17 bits) at `shift` (0..=7) in `direction`,
    /// or `None` for a field out of range.
    #[must_use]
    pub const fn new(direction: MmcDirection, shift: u8, mantissa: u32) -> Option<Self> {
        if shift < 8 && mantissa < 1 << 17 {
            Some(Self {
                direction,
                shift,
                mantissa,
            })
        } else {
            None
        }
    }

    /// The direction.
    #[must_use]
    pub const fn direction(self) -> MmcDirection {
        self.direction
    }

    /// The shift left count, 0..=7.
    #[must_use]
    pub const fn shift(self) -> u8 {
        self.shift
    }

    /// The 17 bit mantissa.
    #[must_use]
    pub const fn mantissa(self) -> u32 {
        self.mantissa
    }

    /// How many bits of the mantissa are fractional: `14 - shift`.
    #[must_use]
    pub const fn fraction_bits(self) -> u8 {
        14 - self.shift
    }

    pub(super) const fn wire(self) -> [u8; 3] {
        [
            self.direction.wire() << 6 | self.shift << 3 | (self.mantissa >> 14) as u8,
            (self.mantissa >> 7) as u8 & 0x7F,
            self.mantissa as u8 & 0x7F,
        ]
    }

    pub(super) const fn from_wire(sh: u8, sm: u8, sl: u8) -> Option<Self> {
        if sh >= 0x80 || sm >= 0x80 || sl >= 0x80 {
            return None;
        }
        Self::new(
            direction_of(sh),
            (sh >> 3) & 0x07,
            ((sh & 0x07) as u32) << 14 | (sm as u32) << 7 | sl as u32,
        )
    }
}

/// A signed count of step lengths for STEP (RP-013 p.30).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcStep {
    direction: MmcDirection,
    count: u8,
}

impl MmcStep {
    /// `count` step lengths (0..=63) in `direction`, or `None` above 63.
    #[must_use]
    pub const fn new(direction: MmcDirection, count: u8) -> Option<Self> {
        if count < 64 {
            Some(Self { direction, count })
        } else {
            None
        }
    }

    /// The direction of the step.
    #[must_use]
    pub const fn direction(self) -> MmcDirection {
        self.direction
    }

    /// The number of step lengths.
    #[must_use]
    pub const fn count(self) -> u8 {
        self.count
    }

    pub(super) const fn wire(self) -> u8 {
        self.direction.wire() << 6 | self.count
    }

    pub(super) const fn from_wire(byte: u8) -> Option<Self> {
        if byte >= 0x80 {
            return None;
        }
        Some(Self {
            direction: direction_of(byte),
            count: byte & 0x3F,
        })
    }
}

/// One track of the standard track bitmap (RP-013 p.11).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MmcTrack {
    /// The video track.
    Video,
    /// The dedicated time code track.
    TimeCode,
    /// Auxiliary track A.
    AuxA,
    /// Auxiliary track B.
    AuxB,
    /// Audio track `n`, counted from 1.
    Audio(u16),
}

/// The most bitmap bytes a field holds in one message: 317 tracks.
pub const MMC_MAX_TRACK_BITMAP_BYTES: usize = 46;

/// The standard track bitmap (RP-013 p.11): a bit a track, seven to a byte,
/// trailing zero bytes left off. A track past the bytes is inactive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcTrackBitmap {
    bytes: [u8; MMC_MAX_TRACK_BITMAP_BYTES],
    len: u8,
}

impl MmcTrackBitmap {
    /// No track active.
    pub const NONE: Self = Self {
        bytes: [0; MMC_MAX_TRACK_BITMAP_BYTES],
        len: 0,
    };

    /// The bitmap of wire bytes, or `None` for a byte over 127, a reserved bit
    /// set, or more than [`MMC_MAX_TRACK_BITMAP_BYTES`].
    #[must_use]
    pub fn from_bytes(wire: &[u8]) -> Option<Self> {
        let len = u8::try_from(wire.len()).ok()?;
        if wire.len() > MMC_MAX_TRACK_BITMAP_BYTES
            || wire.iter().any(|b| *b >= 0x80)
            || wire.first().is_some_and(|b| b & 0x02 != 0)
        {
            return None;
        }
        let mut bytes = [0; MMC_MAX_TRACK_BITMAP_BYTES];
        bytes[..wire.len()].copy_from_slice(wire);
        Some(Self { bytes, len })
    }

    /// The wire bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }

    /// The byte and bit of `track`, or `None` for a track the bitmap cannot
    /// hold (audio track 0, or past 317).
    fn place(track: MmcTrack) -> Option<(usize, u8)> {
        match track {
            MmcTrack::Video => Some((0, 0)),
            MmcTrack::TimeCode => Some((0, 2)),
            MmcTrack::AuxA => Some((0, 3)),
            MmcTrack::AuxB => Some((0, 4)),
            MmcTrack::Audio(1) => Some((0, 5)),
            MmcTrack::Audio(2) => Some((0, 6)),
            MmcTrack::Audio(n @ 3..=317) => {
                let k = n - 3;
                let bit = u8::try_from(k % 7).expect("a remainder of 7 is below 256");
                Some((1 + usize::from(k / 7), bit))
            }
            MmcTrack::Audio(_) => None,
        }
    }

    /// Whether `track` is active.
    #[must_use]
    pub fn contains(&self, track: MmcTrack) -> bool {
        Self::place(track)
            .is_some_and(|(byte, bit)| self.as_bytes().get(byte).is_some_and(|b| b >> bit & 1 != 0))
    }

    /// This bitmap with `track` set, or `None` for a track it cannot hold.
    #[must_use]
    pub fn with(mut self, track: MmcTrack) -> Option<Self> {
        let (byte, bit) = Self::place(track)?;
        self.bytes[byte] |= 1 << bit;
        let end = u8::try_from(byte + 1).ok()?;
        self.len = self.len.max(end);
        Some(self)
    }
}

/// The flags byte of the user bits (RP-013 p.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcUserBitFlags {
    /// `t`: the user bits carry secondary time code.
    pub secondary_time_code: bool,
    /// `j`: binary group flag 1.
    pub group_flag_1: bool,
    /// `i`: binary group flag 0.
    pub group_flag_0: bool,
}

/// The standard user bits: eight binary groups and their flags (RP-013 p.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcUserBits {
    /// Binary groups 1 to 8, in order.
    pub groups: [U4; 8],
    /// The flags.
    pub flags: MmcUserBitFlags,
}

/// The extension level of a code: how many `00` bytes precede it
/// (RP-013 p.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MmcExtension {
    /// The base set.
    Base,
    /// `00 nn`: the first extension set.
    First,
    /// `00 00 nn`: the second extension set.
    Second,
}

/// A command or information field name, with its extension level
/// (RP-013 pp.5 and 6): one to three bytes, the last `01` to `7F`. Three
/// leading zeros are not a name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcName {
    extension: MmcExtension,
    code: U7,
}

impl MmcName {
    /// The name `code` at `extension`, or `None` for code 0.
    #[must_use]
    pub const fn new(extension: MmcExtension, code: U7) -> Option<Self> {
        if code.get() == 0 {
            None
        } else {
            Some(Self { extension, code })
        }
    }

    /// The base-set name `code`, or `None` for 0.
    #[must_use]
    pub const fn base(code: U7) -> Option<Self> {
        Self::new(MmcExtension::Base, code)
    }

    /// The base-set name for a literal or a constant.
    ///
    /// # Panics
    ///
    /// For 0 or a byte over 127; in a `const` that is a compile error.
    #[must_use]
    #[track_caller]
    pub const fn of(code: u8) -> Self {
        match Self::base(U7::of(code)) {
            Some(name) => name,
            None => panic!("a name is 01 to 7F"),
        }
    }

    /// The extension level.
    #[must_use]
    pub const fn extension(self) -> MmcExtension {
        self.extension
    }

    /// The final byte.
    #[must_use]
    pub const fn code(self) -> U7 {
        self.code
    }
}

/// A procedure or event name: `00` to `7E` (`7F` is reserved,
/// RP-013 pp.34 to 38).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcSlot(U7);

/// A `7F`, which is no procedure's or event's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("7F is reserved, not a procedure or event name")]
pub struct NotMmcSlot;

impl MmcSlot {
    /// The slot `id`, or `None` for `7F`.
    #[must_use]
    pub const fn new(id: U7) -> Option<Self> {
        if id.get() == 0x7F {
            None
        } else {
            Some(Self(id))
        }
    }

    /// The slot for a literal or a constant.
    ///
    /// # Panics
    ///
    /// For `7F`; in a `const` that is a compile error.
    #[must_use]
    #[track_caller]
    pub const fn of(id: u8) -> Self {
        match Self::new(U7::of(id)) {
            Some(slot) => slot,
            None => panic!("7F is reserved"),
        }
    }

    /// The name as its 7-bit byte.
    #[must_use]
    pub const fn get(self) -> U7 {
        self.0
    }
}

impl From<MmcSlot> for U7 {
    fn from(slot: MmcSlot) -> Self {
        slot.0
    }
}

impl TryFrom<U7> for MmcSlot {
    type Error = NotMmcSlot;

    fn try_from(id: U7) -> Result<Self, NotMmcSlot> {
        Self::new(id).ok_or(NotMmcSlot)
    }
}

/// One slot or all of them: `7F` selects every procedure, event or group.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MmcSelection {
    /// The one named.
    One(MmcSlot),
    /// Every one (`7F`).
    All,
}

/// Up to 46 raw bytes of a message that is not parsed further: a segment of
/// a longer message, an error's offending command (possibly cut short) or a
/// failure's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MmcFragment(MmcBytes);

impl MmcFragment {
    /// The most bytes a fragment holds.
    pub const MAX: usize = 46;

    /// The fragment of `bytes`, or `None` for a byte over 127 or more than
    /// [`MAX`](Self::MAX).
    #[must_use]
    pub fn new(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > Self::MAX || bytes.iter().any(|b| *b >= 0x80) {
            return None;
        }
        MmcBytes::from_slice(bytes).map(Self)
    }

    /// The bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.as_slice()
    }

    /// The number of bytes, at most [`MAX`](Self::MAX).
    pub(super) const fn len(&self) -> u8 {
        self.0.len()
    }
}
