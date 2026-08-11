//! The SMPTE frame-rate enum, shared by MTC and LTC.
//!
//! Its own module because the rate is the one piece of timecode vocabulary that
//! outlives a decoder: a project holds a frame rate whether or not any MTC is
//! arriving, and [`crate::sync::mtc`] reads it rather than owning it.

/// SMPTE frame rate for MTC/LTC.
///
/// The discriminants are the two-bit rate field MTC carries in its final
/// quarter-frame, so they are wire values and must not be renumbered.
///
/// # Two rates are 29.97, and the difference is counting, not speed
///
/// [`Fps2997Df`](Self::Fps2997Df) and [`Fps2997Ndf`](Self::Fps2997Ndf) run at the
/// identical rate — [`fps`](Self::fps) returns the same number for both. They
/// differ in whether the *labels* skip: drop-frame omits two frame numbers a
/// minute so the timecode stays aligned with wall-clock time, non-drop counts
/// every frame and drifts about 3.6 seconds an hour. Treating one as the other
/// keeps playing at the right speed and reports the wrong position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum SmpteFrameRate {
    /// 24 fps (film)
    Fps24 = 0,
    /// 25 fps (PAL video)
    Fps25 = 1,
    /// 29.97 fps drop-frame (NTSC video)
    #[default]
    Fps2997Df = 2,
    /// 29.97 fps non-drop (rare)
    Fps2997Ndf = 3,
    /// 30 fps (audio/music)
    Fps30 = 4,
}

impl SmpteFrameRate {
    /// Decodes a discriminant back into a rate, falling back to
    /// [`Fps2997Df`](Self::Fps2997Df) for an out-of-range value.
    ///
    /// # Panics
    ///
    /// Debug builds assert `val <= 4`. Release builds take the fallback silently,
    /// on the grounds that a malformed byte from a device should not kill the
    /// transport — but a caller constructing this value itself has a bug, which
    /// is what the assert is there to catch.
    pub fn from_u8(val: u8) -> Self {
        debug_assert!(val <= 4, "invalid SmpteFrameRate discriminant: {val}");
        match val {
            0 => Self::Fps24,
            1 => Self::Fps25,
            2 => Self::Fps2997Df,
            3 => Self::Fps2997Ndf,
            4 => Self::Fps30,
            _ => Self::Fps2997Df,
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

    /// Reports whether frame *numbers* are skipped to track wall-clock time.
    ///
    /// True only for [`Fps2997Df`](Self::Fps2997Df). A renderer that ignores this
    /// produces timecode that is correct frame-by-frame and wrong by seconds an
    /// hour in.
    pub fn is_drop_frame(&self) -> bool {
        matches!(self, Self::Fps2997Df)
    }
}
