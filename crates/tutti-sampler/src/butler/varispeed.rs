//! Varispeed and playback direction control.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlayDirection {
    #[default]
    Forward,
    Reverse,
}

impl PlayDirection {
    pub fn is_reverse(&self) -> bool {
        matches!(self, Self::Reverse)
    }
}

/// Varispeed configuration for a stream.
#[derive(Debug, Clone, Copy)]
pub struct Varispeed {
    pub direction: PlayDirection,
    /// 1.0 = normal, 0.5 = half, 2.0 = double
    pub speed: f32,
}
