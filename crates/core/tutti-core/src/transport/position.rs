//! Musical Position and Loop Range
//!
//! Provides position handling with Ardour-style "squishing" for seamless loop wrapping.

#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub(crate) struct MusicalPosition {
    pub(crate) beats: f64,
}

impl MusicalPosition {
    #[inline]
    pub(crate) const fn from_beats(beats: f64) -> Self {
        Self { beats }
    }
}

impl core::ops::Add<f64> for MusicalPosition {
    type Output = Self;

    #[inline]
    fn add(self, beats: f64) -> Self {
        Self {
            beats: self.beats + beats,
        }
    }
}

impl core::ops::AddAssign<f64> for MusicalPosition {
    #[inline]
    fn add_assign(&mut self, beats: f64) {
        self.beats += beats;
    }
}

impl core::ops::Sub<f64> for MusicalPosition {
    type Output = Self;

    #[inline]
    fn sub(self, beats: f64) -> Self {
        Self {
            beats: self.beats - beats,
        }
    }
}

impl core::ops::Sub<MusicalPosition> for MusicalPosition {
    type Output = f64;

    #[inline]
    fn sub(self, other: MusicalPosition) -> f64 {
        self.beats - other.beats
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LoopRange {
    pub(crate) start: f64,
    pub(crate) end: f64,
}

impl LoopRange {
    #[inline]
    pub(crate) const fn new(start: f64, end: f64) -> Self {
        Self { start, end }
    }
}

impl Default for LoopRange {
    fn default() -> Self {
        Self {
            start: 0.0,
            end: 4.0, // Default 4-beat loop
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_position_from_beats() {
        let pos = MusicalPosition::from_beats(4.5);
        assert_eq!(pos.beats, 4.5);
    }

    #[test]
    fn test_position_arithmetic() {
        let pos = MusicalPosition::from_beats(2.0);
        let pos2 = pos + 1.5;
        assert_eq!(pos2.beats, 3.5);

        let diff = pos2 - pos;
        assert_eq!(diff, 1.5);
    }

    #[test]
    fn test_loop_range_creation() {
        let range = LoopRange::new(0.0, 4.0);
        assert_eq!(range.start, 0.0);
        assert_eq!(range.end, 4.0);

        let range_with_offset = LoopRange::new(4.0, 8.0);
        assert_eq!(range_with_offset.start, 4.0);
        assert_eq!(range_with_offset.end, 8.0);
    }
}
