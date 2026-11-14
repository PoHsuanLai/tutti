//! The SMPTE frame-rate enum, shared by MTC and LTC.
//!
//! Its own module because the rate is the one piece of timecode vocabulary that
//! outlives a decoder: a project holds a frame rate whether or not any MTC is
//! arriving, and the MTC Full Message and the runtime's decoders read it rather
//! than owning it.

/// SMPTE frame rate for MTC/LTC.
///
/// The discriminants are not MTC's wire values: they are the encoding the
/// clock master keeps in an atomic (`TryFrom<u8>` reads it back). MTC's two-bit
/// rate code in the last Quarter Frame and in a Full Message's hours byte is
/// [`MtcRate`], which has no 29.97 non-drop: [`mtc_rate`](Self::mtc_rate) is
/// `None` for it, and every `MtcRate` converts to a `SmpteFrameRate`.
///
/// # Two rates are 29.97, and the difference is counting, not speed
///
/// [`Fps2997Df`](Self::Fps2997Df) and [`Fps2997Ndf`](Self::Fps2997Ndf) run at the
/// identical rate — [`fps`](Self::fps) returns the same number for both. They
/// differ in whether the *labels* skip: drop-frame omits two frame numbers a
/// minute so the timecode stays aligned with wall-clock time, non-drop counts
/// every frame and drifts about 3.6 seconds an hour. Treating one as the other
/// keeps playing at the right speed and reports the wrong position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SmpteFrameRate {
    /// 24 fps (film)
    Fps24 = 0,
    /// 25 fps (PAL video)
    Fps25 = 1,
    /// 29.97 fps drop-frame (NTSC video)
    Fps2997Df = 2,
    /// 29.97 fps non-drop (rare)
    Fps2997Ndf = 3,
    /// 30 fps (audio/music)
    Fps30 = 4,
}

/// The frame rate MTC names in its two-bit code: in the last Quarter Frame of a
/// cycle and in the hours byte of a Full Message (M1 p.53 and T-9). The
/// discriminants are the wire codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MtcRate {
    /// Code 0: 24 frames per second.
    Fps24 = 0,
    /// Code 1: 25 frames per second.
    Fps25 = 1,
    /// Code 2: 29.97 frames per second, drop-frame (30 labels a second).
    Fps2997Df = 2,
    /// Code 3: 30 frames per second, non-drop.
    Fps30 = 3,
}

impl MtcRate {
    /// Every rate, in code order.
    pub const ALL: [Self; 4] = [Self::Fps24, Self::Fps25, Self::Fps2997Df, Self::Fps30];

    /// The rate the low two bits of `bits` name: the field is two bits wide on
    /// the wire, so the bits above it (the hours' or a piece's own) are masked
    /// off.
    #[must_use]
    pub const fn masked(bits: u8) -> Self {
        match bits & 0x03 {
            0 => Self::Fps24,
            1 => Self::Fps25,
            2 => Self::Fps2997Df,
            _ => Self::Fps30,
        }
    }

    /// The frame labels in a second at this rate: the exclusive bound of a
    /// frame field.
    #[must_use]
    pub const fn labels_per_second(self) -> u8 {
        match self {
            Self::Fps24 => 24,
            Self::Fps25 => 25,
            Self::Fps2997Df | Self::Fps30 => 30,
        }
    }

    /// The two-bit code on the wire.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }
}

impl From<MtcRate> for SmpteFrameRate {
    fn from(rate: MtcRate) -> Self {
        match rate {
            MtcRate::Fps24 => Self::Fps24,
            MtcRate::Fps25 => Self::Fps25,
            MtcRate::Fps2997Df => Self::Fps2997Df,
            MtcRate::Fps30 => Self::Fps30,
        }
    }
}

/// A byte that is none of [`SmpteFrameRate`]'s discriminants (0 to 4): what
/// `SmpteFrameRate::try_from` refuses, carrying the byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{} is not an SMPTE frame rate discriminant (0 to 4)", .0)]
pub struct UnknownFrameRate(pub u8);

impl TryFrom<u8> for SmpteFrameRate {
    type Error = UnknownFrameRate;

    /// Reads a discriminant (`rate as u8`) back into a rate.
    fn try_from(val: u8) -> Result<Self, UnknownFrameRate> {
        match val {
            0 => Ok(Self::Fps24),
            1 => Ok(Self::Fps25),
            2 => Ok(Self::Fps2997Df),
            3 => Ok(Self::Fps2997Ndf),
            4 => Ok(Self::Fps30),
            raw => Err(UnknownFrameRate(raw)),
        }
    }
}

impl SmpteFrameRate {
    /// Every rate, in discriminant order.
    pub const ALL: [Self; 5] = [
        Self::Fps24,
        Self::Fps25,
        Self::Fps2997Df,
        Self::Fps2997Ndf,
        Self::Fps30,
    ];

    /// The MTC rate this one is sent as, or `None` for
    /// [`Fps2997Ndf`](Self::Fps2997Ndf), which MTC has no code for.
    #[must_use]
    pub const fn mtc_rate(self) -> Option<MtcRate> {
        match self {
            Self::Fps24 => Some(MtcRate::Fps24),
            Self::Fps25 => Some(MtcRate::Fps25),
            Self::Fps2997Df => Some(MtcRate::Fps2997Df),
            Self::Fps30 => Some(MtcRate::Fps30),
            Self::Fps2997Ndf => None,
        }
    }

    /// Frames per second, as the exact rational the standard specifies.
    ///
    /// **`f64`, deliberately, and not a unit type.** The 29.97 rates are
    /// `30000 / 1001`, which no binary float represents exactly, and a timecode
    /// position is multiplied by this over durations measured in hours. `Seconds`
    /// is `f32` and would accumulate visible drift across a long-form timeline —
    /// this is the "precision the unit cannot carry" case the units rule carves
    /// out. Narrow to `f32` at the point of use, not here.
    ///
    /// Identical for both 29.97 variants; see the type docs for what actually
    /// separates them.
    pub fn fps(&self) -> f64 {
        match self {
            Self::Fps24 => 24.0,
            Self::Fps25 => 25.0,
            Self::Fps2997Df | Self::Fps2997Ndf => 30000.0 / 1001.0,
            Self::Fps30 => 30.0,
        }
    }

    /// The rate as an exact fraction of frames over seconds, `(num, den)`:
    /// `24/1`, `25/1`, `30000/1001` for both 29.97 rates, `30/1`. Integer
    /// frame arithmetic at a position in samples uses this, not [`fps`](Self::fps).
    pub const fn ratio(&self) -> (u64, u64) {
        match self {
            Self::Fps24 => (24, 1),
            Self::Fps25 => (25, 1),
            Self::Fps2997Df | Self::Fps2997Ndf => (30_000, 1001),
            Self::Fps30 => (30, 1),
        }
    }

    /// Frame labels per second: the count the `FF` field wraps at (24, 25, 30,
    /// 30, 30 for the two 29.97 rates).
    pub const fn labels_per_second(&self) -> u64 {
        match self {
            Self::Fps24 => 24,
            Self::Fps25 => 25,
            Self::Fps2997Df | Self::Fps2997Ndf | Self::Fps30 => 30,
        }
    }

    /// Reports whether frame *numbers* are skipped to track wall-clock time.
    ///
    /// True only for [`Fps2997Df`](Self::Fps2997Df). A renderer that ignores this
    /// produces timecode that is correct frame-by-frame and wrong by seconds an
    /// hour in.
    #[must_use]
    pub fn is_drop_frame(&self) -> bool {
        matches!(self, Self::Fps2997Df)
    }
}

#[cfg(test)]
mod tests;
