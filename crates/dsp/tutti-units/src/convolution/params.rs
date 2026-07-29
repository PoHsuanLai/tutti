//! Parameter groupings for convolution nodes.
//!
//! Mirrors the shape of `dynamics::params` — named sub-structs replace
//! a flat list of `Param` fields, so each node reads as "an engine plus
//! its parameter block".

use tutti_core::{Amplitude, Arc, AtomicF32, Mix, Param};

/// Wet/dry mix + output gain.
///
/// - `mix`: 0.0 = fully dry, 1.0 = fully wet.
/// - `gain`: linear multiplier applied to the wet signal before mixing.
#[derive(Clone)]
pub struct WetDry {
    pub mix: Param<Mix>,
    pub gain: Param<Amplitude>,
}

impl WetDry {
    /// Build with the given wet/dry mix and wet-path gain.
    pub fn new(mix: impl Into<Mix>, gain: impl Into<Amplitude>) -> Self {
        Self {
            mix: Param::new(Mix::new_clamped(mix.into().get())),
            gain: Param::new(Amplitude(gain.into().get().max(0.0))),
        }
    }

    pub fn mix_handle(&self) -> Arc<AtomicF32> {
        self.mix.as_atomic()
    }

    pub fn gain_handle(&self) -> Arc<AtomicF32> {
        self.gain.as_atomic()
    }

    pub fn set_mix(&self, mix: impl Into<Mix>) {
        self.mix.store(Mix::new_clamped(mix.into().get()));
    }

    pub fn set_gain(&self, gain: impl Into<Amplitude>) {
        self.gain.store(Amplitude(gain.into().get().max(0.0)));
    }

    /// Snapshot both params at the start of a block.
    ///
    /// Typed: the two are different quantities (a blend ratio and a linear
    /// multiplier) that happen to share a representation, so an untyped pair
    /// invites swapping them at the call site.
    #[inline]
    pub fn load(&self) -> (Mix, Amplitude) {
        (self.mix.load(), self.gain.load())
    }
}

impl Default for WetDry {
    fn default() -> Self {
        Self::new(Mix(0.5), Amplitude::UNITY)
    }
}
