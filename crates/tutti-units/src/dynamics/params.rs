//! Shared parameter groupings for compressors, gates, and limiters.
//!
//! The three dynamics processors all track an attack/release pair and a
//! threshold-with-knee pair. Extracting them into named sub-structs means
//! each core reads as "this processor takes these *groups* of parameters"
//! rather than a flat list of seven atomics.

use tutti_core::{Db, Param, Seconds};

/// Envelope-follower attack/release times in seconds.
#[derive(Clone)]
pub struct AttackRelease {
    pub attack: Param<Seconds>,
    pub release: Param<Seconds>,
}

impl AttackRelease {
    pub fn new(attack: impl Into<Seconds>, release: impl Into<Seconds>) -> Self {
        Self {
            attack: Param::new(attack.into()),
            release: Param::new(release.into()),
        }
    }
}

/// Threshold + optional knee width (both in dB). Gates and limiters omit
/// the knee by constructing with `knee = 0.0`.
#[derive(Clone)]
pub struct ThresholdParams {
    pub threshold: Param<Db>,
    pub knee: Param<Db>,
}

impl ThresholdParams {
    pub fn new(threshold_db: impl Into<Db>, knee_db: impl Into<Db>) -> Self {
        Self {
            threshold: Param::new(threshold_db.into()),
            knee: Param::new(Db(knee_db.into().get().max(0.0))),
        }
    }

    #[inline]
    pub fn load(&self) -> (f32, f32) {
        (self.threshold.load().get(), self.knee.load().get())
    }
}
