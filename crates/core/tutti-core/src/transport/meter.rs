//! Musical meter.

/// A musical time signature, e.g. 4/4 or 7/8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSignature {
    pub numerator: u32,
    pub denominator: u32,
}

impl TimeSignature {
    pub const fn new(numerator: u32, denominator: u32) -> Self {
        Self {
            numerator,
            denominator,
        }
    }

    /// Quarter-note beats per bar. 7/8 is 3.5 quarter-note beats.
    #[inline]
    pub fn beats_per_bar(&self) -> f64 {
        f64::from(self.numerator) * 4.0 / f64::from(self.denominator)
    }
}

impl Default for TimeSignature {
    fn default() -> Self {
        Self::new(4, 4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beats_per_bar_handles_compound_meter() {
        assert_eq!(TimeSignature::new(4, 4).beats_per_bar(), 4.0);
        assert_eq!(TimeSignature::new(7, 8).beats_per_bar(), 3.5);
        assert_eq!(TimeSignature::default(), TimeSignature::new(4, 4));
    }
}
