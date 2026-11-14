//! The SMPTE frame-rate enum, shared by MTC and LTC.
//!
//! Its own module because the rate is the one piece of timecode vocabulary that
//! outlives a decoder: a project holds a frame rate whether or not any MTC is
//! arriving, and the MTC Full Message and the runtime's decoders read it rather
//! than owning it.

use tutti_types::Seconds;

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

impl MtcRate {
    /// The rate as a [`SmpteFrameRate`].
    #[must_use]
    pub const fn frame_rate(self) -> SmpteFrameRate {
        match self {
            Self::Fps24 => SmpteFrameRate::Fps24,
            Self::Fps25 => SmpteFrameRate::Fps25,
            Self::Fps2997Df => SmpteFrameRate::Fps2997Df,
            Self::Fps30 => SmpteFrameRate::Fps30,
        }
    }
}

impl From<MtcRate> for SmpteFrameRate {
    fn from(rate: MtcRate) -> Self {
        rate.frame_rate()
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

/// An assembled SMPTE position: `HH:MM:SS:FF` plus the rate that gives the frame
/// count meaning.
///
/// The fields are the timecode's own *labels*, not a duration — under
/// [`SmpteFrameRate::Fps2997Df`] some frame numbers never occur, so arithmetic on
/// these fields directly is wrong. Convert with [`to_seconds`](Self::to_seconds)
/// first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SmpteTimecode {
    hours: u8,
    minutes: u8,
    seconds: u8,
    frames: u8,
    frame_rate: SmpteFrameRate,
}

impl SmpteTimecode {
    /// `00:00:00:00` at `frame_rate`.
    #[must_use]
    pub const fn origin(frame_rate: SmpteFrameRate) -> Self {
        Self {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
            frame_rate,
        }
    }

    /// The position `hours:minutes:seconds:frames` counted at `frame_rate`, or
    /// `None` when a field is out of range: hours 0..=23, minutes and seconds
    /// 0..=59, frames below the rate's label count (24, 25 or 30:
    /// [`SmpteFrameRate::labels_per_second`]).
    ///
    /// The labels a drop-frame minute omits (`;00` and `;01`) are accepted:
    /// they are in range, and [`frame_count`](Self::frame_count) reads each as
    /// the next label that occurs.
    #[must_use]
    pub const fn new(
        hours: u8,
        minutes: u8,
        seconds: u8,
        frames: u8,
        frame_rate: SmpteFrameRate,
    ) -> Option<Self> {
        if hours < 24
            && minutes < 60
            && seconds < 60
            && (frames as u64) < frame_rate.labels_per_second()
        {
            Some(Self {
                hours,
                minutes,
                seconds,
                frames,
                frame_rate,
            })
        } else {
            None
        }
    }

    /// Hours, 0..=23. MTC carries this in 5 bits split across two nibbles.
    #[must_use]
    pub const fn hours(&self) -> u8 {
        self.hours
    }

    /// Minutes, 0..=59.
    #[must_use]
    pub const fn minutes(&self) -> u8 {
        self.minutes
    }

    /// Seconds, 0..=59.
    #[must_use]
    pub const fn seconds(&self) -> u8 {
        self.seconds
    }

    /// Frames within the second, below [`frame_rate`](Self::frame_rate)'s label
    /// count.
    #[must_use]
    pub const fn frames(&self) -> u8 {
        self.frames
    }

    /// The rate this position is counted at. Carried in the same quarter-frame as
    /// the top hours bit, so it arrives with the position rather than being
    /// configured out of band.
    #[must_use]
    pub const fn frame_rate(&self) -> SmpteFrameRate {
        self.frame_rate
    }
}

impl core::fmt::Display for SmpteTimecode {
    /// Canonical `HH:MM:SS:FF` SMPTE form (colon-separated, zero-padded).
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:02}:{:02}:{:02}:{:02}",
            self.hours, self.minutes, self.seconds, self.frames
        )
    }
}

impl SmpteTimecode {
    /// Elapsed time from `00:00:00:00`, in seconds.
    ///
    /// Counts through [`frame_count`](Self::frame_count), so drop-frame labels
    /// are mapped to the frames that elapsed. Non-drop 29.97 labels are counted
    /// at face value, at 30000/1001 frames per second.
    #[must_use]
    pub fn to_seconds(&self) -> Seconds {
        let (num, den) = self.frame_rate.ratio();
        Seconds::new(self.frame_count() as f64 * den as f64 / num as f64)
    }

    /// Frames elapsed since `00:00:00:00`.
    ///
    /// Under [`SmpteFrameRate::Fps2997Df`] this skips the labels `;00` and
    /// `;01` that each minute not divisible by ten omits (SMPTE 12M). Labels
    /// that cannot occur in drop-frame are counted as the next label that does.
    pub fn frame_count(&self) -> u64 {
        let per_sec = self.frame_rate.labels_per_second();
        let minutes = u64::from(self.hours) * 60 + u64::from(self.minutes);
        let mut frames = u64::from(self.frames);
        // `;00` and `;01` do not exist in a minute not divisible by ten: they
        // count as `;02`, the next label that does.
        let dropped_label =
            self.frame_rate.is_drop_frame() && self.seconds == 0 && minutes % 10 != 0;
        if dropped_label {
            frames = frames.max(2);
        }
        let labelled = (minutes * 60 + u64::from(self.seconds)) * per_sec + frames;
        if self.frame_rate.is_drop_frame() {
            labelled.saturating_sub(2 * (minutes - minutes / 10))
        } else {
            labelled
        }
    }

    /// The label of frame number `count` since `00:00:00:00` at `frame_rate`;
    /// the inverse of [`frame_count`](Self::frame_count). Hours wrap at 24.
    #[must_use]
    pub fn from_frame_count(count: u64, frame_rate: SmpteFrameRate) -> Self {
        let per_sec = frame_rate.labels_per_second();
        let labelled = if frame_rate.is_drop_frame() {
            // 17_982 frames per ten minutes: the first minute keeps all 1800
            // labels, the other nine drop two.
            let (tens, rem) = (count / 17_982, count % 17_982);
            let dropped = if rem < 2 { 0 } else { 2 * ((rem - 2) / 1798) };
            count + 18 * tens + dropped
        } else {
            count
        };
        let secs = labelled / per_sec;
        Self {
            hours: ((secs / 3600) % 24) as u8,
            minutes: ((secs / 60) % 60) as u8,
            seconds: (secs % 60) as u8,
            frames: (labelled % per_sec) as u8,
            frame_rate,
        }
    }
}

#[cfg(test)]
mod tests;
