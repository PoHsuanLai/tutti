/// SMPTE frame rate for MTC/LTC.
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

    pub fn fps(&self) -> f64 {
        match self {
            Self::Fps24 => 24.0,
            Self::Fps25 => 25.0,
            Self::Fps2997Df | Self::Fps2997Ndf => 30000.0 / 1001.0,
            Self::Fps30 => 30.0,
        }
    }

    pub fn is_drop_frame(&self) -> bool {
        matches!(self, Self::Fps2997Df)
    }
}
