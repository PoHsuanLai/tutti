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
