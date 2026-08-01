//! Audio-rate param modulation units — the per-sample tier of the modulation
//! matrix.
//!
//! Three small `AudioUnit`s that materialize an audio-rate modulation edge:
//!
//! ```text
//! source ─► ParamShaperUnit ─► ParamSumUnit ─► (param-input port on target)
//!           (depth·polarity·    (base + Σ mods,
//!            curve via LUT)      one clamp)
//! ```
//!
//! The base port is fed by an [`AtomicSourceUnit`] holding the authored value,
//! so the UI/automation handle path is unchanged: the same atomic a control-rate
//! [`AtomicTarget`](tutti_mod::AtomicTarget) mirrors into becomes this chain's
//! base. That is what makes the two tiers compose rather than compete — one
//! base, both tiers, no second accumulator.
//!
//! All three are RT-safe: no allocation and no locks in `tick`/`process` — only
//! atomic loads, arithmetic, and read-only LUT lookups.

use std::sync::Arc;

use tutti_core::dsp::Signal;
use tutti_core::{AtomicF32, AudioUnit, BufferMut, BufferRef, Ordering, SignalFrame, Tail};
use tutti_mod::{shape, CurveType, Polarity};

/// LUT resolution for [`ParamShaperUnit`]. 256 points + linear interpolation is
/// inaudibly smooth for a control-shaping curve and keeps the table in L1.
const LUT_N: usize = 256;

/// Maps a raw modulation signal to a shaped offset via `depth · polarity ·
/// curve`, baked into a fixed LUT at construction so `tick`/`process` are
/// branch-light (normalize → table lookup → lerp).
///
/// Input domain is `[-1, 1]` (bipolar CV or a normalized audio signal). The
/// output is the additive offset [`ParamSumUnit`] adds onto the base.
///
/// The shaping is [`tutti_mod::shape`] — the *same* function the control-rate
/// path applies in `ModPreFrame::run`. Baking it into a LUT here is a
/// performance decision, not a second implementation: both tiers agree on
/// values because they call one function.
#[derive(Clone)]
pub struct ParamShaperUnit {
    lut: Arc<[f32; LUT_N]>,
}

impl ParamShaperUnit {
    pub fn new(depth: impl Into<tutti_types::Depth>, polarity: Polarity, curve: CurveType) -> Self {
        let depth = depth.into();
        let mut lut = [0.0_f32; LUT_N];
        for (i, slot) in lut.iter_mut().enumerate() {
            // Map LUT index → input x ∈ [-1, 1].
            let x = (i as f32 / (LUT_N - 1) as f32) * 2.0 - 1.0;
            *slot = shape(x, depth, polarity, curve);
        }
        Self { lut: Arc::new(lut) }
    }

    /// Shaped offset for input `x`, linearly interpolated between LUT points.
    #[inline]
    fn eval(&self, x: f32) -> f32 {
        let pos = ((x.clamp(-1.0, 1.0) + 1.0) * 0.5) * (LUT_N - 1) as f32;
        let i = pos.floor() as usize;
        if i >= LUT_N - 1 {
            return self.lut[LUT_N - 1];
        }
        let frac = pos - i as f32;
        self.lut[i] + (self.lut[i + 1] - self.lut[i]) * frac
    }
}

impl AudioUnit for ParamShaperUnit {
    fn inputs(&self) -> usize {
        1
    }
    fn outputs(&self) -> usize {
        1
    }

    fn reset(&mut self) {}

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        output[0] = self.eval(input[0]);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            output.set_f32(0, i, self.eval(input.at_f32(0, i)));
        }
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(1);
        output.set(0, Signal::Latency(0.0));
        output
    }

    fn get_id(&self) -> u64 {
        crate::node_id::PARAM_SHAPER_ID
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    /// Control-rate and stateless: it stops with its input.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// The audio-rate analog of a control-rate accumulator's `final_value`: `base +
/// Σ offsets`, one clamp, per sample.
///
/// Port 0 is the base (authored + automation + any control-rate modulation,
/// arriving as one already-summed scalar); ports `1..=N` are shaped modulation
/// offsets. `mods == 0` passes the base through clamped.
///
/// This is the node that makes fan-in representable at all: `Net` holds one
/// source per input port and has no summing bus, so summing is a node's job.
#[derive(Clone)]
pub struct ParamSumUnit {
    mods: usize,
    min: f32,
    max: f32,
}

impl ParamSumUnit {
    pub fn new(mods: usize, min: f32, max: f32) -> Self {
        Self { mods, min, max }
    }

    #[inline]
    fn fold(&self, base: f32, offsets: impl Iterator<Item = f32>) -> f32 {
        (base + offsets.sum::<f32>()).clamp(self.min, self.max)
    }
}

impl AudioUnit for ParamSumUnit {
    fn inputs(&self) -> usize {
        1 + self.mods
    }
    fn outputs(&self) -> usize {
        1
    }

    fn reset(&mut self) {}

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        output[0] = self.fold(input[0], input[1..=self.mods].iter().copied());
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            let base = input.at_f32(0, i);
            let offsets = (1..=self.mods).map(|p| input.at_f32(p, i));
            output.set_f32(0, i, self.fold(base, offsets));
        }
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(1);
        output.set(0, Signal::Latency(0.0));
        output
    }

    fn get_id(&self) -> u64 {
        crate::node_id::PARAM_SUM_ID
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    /// Control-rate and stateless: it stops with its input.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// A settable constant source: 0 inputs, 1 output, value held in a shared
/// atomic.
///
/// Feeds [`ParamSumUnit`]'s base port so the existing UI/automation handle path
/// keeps working — it reads the same atomic a control-rate `AtomicTarget`
/// already mirrors into. (fundsp's `dc()` covers a *fixed* constant; this is the
/// settable equivalent.)
#[derive(Clone)]
pub struct AtomicSourceUnit {
    value: Arc<AtomicF32>,
}

impl AtomicSourceUnit {
    pub fn new(initial: f32) -> Self {
        Self {
            value: Arc::new(AtomicF32::new(initial)),
        }
    }

    /// Build one over an *existing* atomic — the handle a control-rate
    /// `AtomicTarget` mirrors into, so the two tiers share one base cell.
    pub fn over(value: Arc<AtomicF32>) -> Self {
        Self { value }
    }

    /// The shared atomic — clone it for control-thread writes.
    pub fn shared(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.value)
    }
}

impl AudioUnit for AtomicSourceUnit {
    fn inputs(&self) -> usize {
        0
    }
    fn outputs(&self) -> usize {
        1
    }

    fn reset(&mut self) {}

    #[inline]
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[0] = self.value.load(Ordering::Acquire);
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        // Read once per block: the value cannot change mid-block in any way a
        // consumer could rely on, and a per-sample atomic load is pure cost.
        let v = self.value.load(Ordering::Acquire);
        for i in 0..size {
            output.set_f32(0, i, v);
        }
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(1);
        output.set(0, Signal::Latency(0.0));
        output
    }

    fn get_id(&self) -> u64 {
        crate::node_id::ATOMIC_SOURCE_ID
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    /// Control-rate and stateless: it stops with its input.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}
