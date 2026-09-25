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
    /// Blend between the dry input and the convolved signal: `0.0` fully dry,
    /// `1.0` fully wet.
    pub mix: Param<Mix>,
    /// Linear [`Amplitude`] applied to the wet path *before* the blend, floored
    /// at 0.
    ///
    /// Distinct from `mix`: this trims the convolved signal's level (an IR is
    /// rarely normalized), while `mix` decides how much of it is heard.
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

    /// Detach every cell (see [`Param::detach`]): the `isolate` half of this
    /// group, keeping the current values.
    pub fn detach(&mut self) {
        self.mix.detach();
        self.gain.detach();
    }

    /// The shared wet/dry [`Mix`] cell, for driving the blend from a modulator.
    ///
    /// Read once per block. Shared across clones, so a write reaches the live
    /// node.
    pub fn mix_handle(&self) -> Arc<AtomicF32> {
        self.mix.as_atomic()
    }

    /// The shared wet-path [`Amplitude`] cell.
    ///
    /// Writing the raw cell bypasses [`set_gain`](Self::set_gain)'s floor at 0;
    /// a negative gain inverts the wet signal's polarity, which partially
    /// cancels against the dry path.
    pub fn gain_handle(&self) -> Arc<AtomicF32> {
        self.gain.as_atomic()
    }

    /// Sets the wet/dry [`Mix`], clamped to `0.0..=1.0`.
    pub fn set_mix(&self, mix: impl Into<Mix>) {
        self.mix.store(Mix::new_clamped(mix.into().get()));
    }

    /// Sets the wet-path [`Amplitude`], floored at 0.
    ///
    /// `1.0` is unity. The floor keeps the wet path from inverting.
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
