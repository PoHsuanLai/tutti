//! Parameter groupings for convolution nodes.
//!
//! Mirrors the shape of `dynamics::params` — named sub-structs replace
//! a flat list of `Param` fields, so each node reads as "an engine plus
//! its parameter block".

use tutti_core::{Arc, AtomicF32, Linear, Param};

/// Wet/dry mix + output gain.
///
/// - `mix`: 0.0 = fully dry, 1.0 = fully wet.
/// - `gain`: linear multiplier applied to the wet signal before mixing.
#[derive(Clone)]
pub struct WetDry {
    pub mix: Param<Linear>,
    pub gain: Param<Linear>,
}

impl WetDry {
    /// Build with the given wet/dry mix and wet-path gain.
    pub fn new(mix: f32, gain: f32) -> Self {
        Self {
            mix: Param::new(Linear(mix.clamp(0.0, 1.0))),
            gain: Param::new(Linear(gain.max(0.0))),
        }
    }

    pub fn mix_handle(&self) -> Arc<AtomicF32> {
        self.mix.as_atomic()
    }

    pub fn gain_handle(&self) -> Arc<AtomicF32> {
        self.gain.as_atomic()
    }

    pub fn set_mix(&self, mix: f32) {
        self.mix.store(Linear(mix.clamp(0.0, 1.0)));
    }

    pub fn set_gain(&self, gain: f32) {
        self.gain.store(Linear(gain.max(0.0)));
    }

    /// Snapshot both atomics at the start of a block.
    #[inline]
    pub fn load(&self) -> (f32, f32) {
        (self.mix.load().0, self.gain.load().0)
    }
}

impl Default for WetDry {
    fn default() -> Self {
        Self::new(0.5, 1.0)
    }
}
