//! The curves a hosted plugin's parameter automation samples: [`TimedParam`]
//! (one curve per parameter), and the curve shapes a host builds them from
//! ([`LfoCurve`], [`LfoOffset`], [`OffsetCurve`], [`PluginParamTarget`]).
//!
//! What samples them is the automation node
//! ([`PluginAutomation`](super::PluginAutomation)), an event source wired to
//! the plugin's event input; this module holds its point budget
//! ([`stride_for`], [`MAX_POINTS`]). Plugins receive real per-block parameter
//! ramps rather than a single frame-rate `set_parameter` value: automation
//! reaches a hosted plugin sample-accurate or not at all.

use std::sync::Arc;

use tutti_core::{Beat, BeatDuration, Depth, PhaseIncrement};
use tutti_nodes::automation::Curve;

use crate::protocol::ParamAddress;

/// One plugin parameter's automation curve, keyed by the parameter's address.
/// The [`Curve`] is evaluated against the transport beat — any beat-keyed curve
/// (breakpoint envelope, constant, LFO), not just an envelope.
#[derive(Clone)]
pub struct TimedParam {
    /// Which parameter this curve drives.
    ///
    /// A [`ParamAddress`](crate::protocol::ParamAddress) because this is where
    /// the automation is *authored*: the caller knows which plugin it is
    /// targeting, so it can say whether the number is an opaque handle or a
    /// VST2 index. A bare `u32` would push that question down to the loaders,
    /// each answering from its own identity rather than from anything the value
    /// carries.
    pub param_id: ParamAddress,
    /// The envelope sampled to produce this parameter's value per block.
    pub curve: Arc<dyn Curve>,
}

/// A pure [`tutti_nodes::Lfo`] as a beat-keyed [`Curve`], for driving a hosted
/// plugin parameter with an LFO through the same sample-accurate producer that
/// carries automation envelopes.
///
/// This is the plugin *adapter* over the pure modulator: it owns the transport
/// mapping (beat → phase) and the output mapping (`base + value·depth·range`,
/// clamped to `[min, max]`), while the waveform math stays in `tutti-mod`. It
/// closes the LFO→plugin gap without any fundsp node — a plugin param never
/// enters the graph, so the value is IPC-encoded like any other automation.
///
/// `Curve::value_at` is `&self` and stateless, so the waveform comes from
/// [`BeatLfo`](tutti_nodes::BeatLfo) — `tutti-mod`'s beat-clocked formulation,
/// which derives every shape from the position alone. The stepped shapes key
/// their randomness on the cycle index rather than a threaded stepper, so they
/// are addressable and a bar replays identically after a seek.
#[derive(Debug, Clone, Copy)]
pub struct LfoCurve {
    /// The pure modulator (config only) — its shape selects the `BeatLfo` form.
    lfo: tutti_nodes::Lfo,
    /// Beats per LFO cycle — a span, so a larger value is *slower*. A
    /// non-positive span freezes at the phase offset.
    beats_per_cycle: BeatDuration,
    depth: Depth,
    /// A displacement added to the generated position, not a position itself —
    /// and meaningfully negative, which a `Phase` cannot be.
    phase_offset: PhaseIncrement,
    /// Output mapping: `base + shaped·(max-min)`, clamped to `[min, max]`.
    base: f32,
    min: f32,
    max: f32,
}

impl LfoCurve {
    /// Build an LFO curve. `base` is the param's un-modulated value; `depth`
    /// scales the oscillation; `[min, max]` is the param's range (the offset is
    /// `value·depth·(max-min)`, matching the control-rate path).
    pub fn new(
        shape: tutti_nodes::LfoShape,
        beats_per_cycle: impl Into<BeatDuration>,
        depth: impl Into<Depth>,
        phase_offset: impl Into<PhaseIncrement>,
        base: f32,
        min: f32,
        max: f32,
    ) -> Self {
        Self {
            // Unit depth — `depth` is applied by `value_at`, so the modulator's
            // raw `[-1, 1]` output is what we sample here.
            lfo: tutti_nodes::Lfo::new(shape),
            beats_per_cycle: beats_per_cycle.into(),
            depth: depth.into(),
            phase_offset: phase_offset.into(),
            base,
            min,
            max,
        }
    }

    /// The raw modulator value in `[-1, 1]` at a given beat.
    ///
    /// Delegates to [`BeatLfo`](tutti_nodes::BeatLfo), the shared beat-clocked
    /// formulation in `tutti-mod`, rather than hashing the cycle index here.
    /// Hashing per cycle is right for `Random` but gives `RandomSmooth` one
    /// value per cycle too — a stair under a name that promises a ramp, which
    /// defeats the point of sub-block delivery.
    #[inline]
    fn raw_value(&self, beat: Beat) -> f32 {
        use tutti_nodes::CurveModulator;

        let cycles =
            beat.cycles_of(self.beats_per_cycle).unwrap_or(0.0) as f32 + self.phase_offset.get();
        tutti_nodes::BeatLfo::new(self.lfo.shape, self.beats_per_cycle).raw_at(cycles)
    }
}

impl Curve for LfoCurve {
    fn value_at(&self, beat: tutti_core::Beat) -> Option<f32> {
        let shaped = self.raw_value(beat) * self.depth.get() * (self.max - self.min);
        Some((self.base + shaped).clamp(self.min, self.max))
    }
}

/// An LFO as a bipolar **offset** curve, for summing as a modulation *layer* on a
/// [`PluginParamTarget`] (as opposed to [`LfoCurve`], which is an *absolute*
/// value in `[min, max]`).
///
/// The offset is `raw · depth · span`, `raw ∈ [-1, 1]`, where `span` is the
/// **target param's** range width — matching the native control-rate contract
/// (`raw · depth · (max − min)` against the *target's* range, tutti-mod's
/// `driver::run`). A layer's value swings around 0; the owning [`LayeredCurve`](tutti_nodes::LayeredCurve)
/// adds the base and applies the param's clamp, so this must NOT clamp to its own
/// `[-1, 1]` (that was the double-scaling bug: `LfoCurve` used its internal span
/// of 2, doubling the depth). Clamped symmetrically to `±span` so a full-depth
/// swing can't exceed the param range even before the target's own clamp.
#[derive(Debug, Clone, Copy)]
pub struct LfoOffset {
    lfo: LfoCurve,
    /// The target param's range width (`max − min`) — the offset scale.
    span: f32,
}

impl LfoOffset {
    /// `shape`/`beats_per_cycle`/`depth`/`phase_offset` as an LFO; `span` is the
    /// target param's range width (the offset scale, e.g. `1.0` for a normalized
    /// `[0, 1]` plugin param).
    pub fn new(
        shape: tutti_nodes::LfoShape,
        beats_per_cycle: impl Into<BeatDuration>,
        depth: impl Into<Depth>,
        phase_offset: impl Into<PhaseIncrement>,
        span: f32,
    ) -> Self {
        // Reuse LfoCurve only for its phase/waveform (`raw_value`); base 0, unit
        // range so the shaping here owns the scaling.
        Self {
            lfo: LfoCurve::new(shape, beats_per_cycle, depth, phase_offset, 0.0, 0.0, 1.0),
            span,
        }
    }
}

impl Curve for LfoOffset {
    fn value_at(&self, beat: tutti_core::Beat) -> Option<f32> {
        let raw = self.lfo.raw_value(beat); // [-1, 1]
        let offset = raw * self.lfo.depth.get() * self.span;
        Some(offset.clamp(-self.span, self.span))
    }
}

/// Turns an **absolute**-valued [`Curve`] (an automation envelope) into an
/// **offset** layer by subtracting a fixed reference — the target's base — so it
/// sums correctly in a [`LayeredCurve`](tutti_nodes::LayeredCurve) (`final = base + Σ offset`). Mirrors the
/// native path's `accumulate(AUTOMATION, value − base())`.
#[derive(Clone)]
pub struct OffsetCurve {
    inner: std::sync::Arc<dyn Curve>,
    subtract: f32,
}

// `inner` is `Arc<dyn Curve>` (not `Debug`), so derive can't apply — a manual
// impl prints the reference offset and elides the wrapped curve.
impl std::fmt::Debug for OffsetCurve {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OffsetCurve")
            .field("subtract", &self.subtract)
            .finish_non_exhaustive()
    }
}

impl OffsetCurve {
    /// Wraps `inner` so every sampled value has `subtract` taken off it.
    pub fn new(inner: std::sync::Arc<dyn Curve>, subtract: f32) -> Self {
        Self { inner, subtract }
    }
}

impl Curve for OffsetCurve {
    fn value_at(&self, beat: tutti_core::Beat) -> Option<f32> {
        self.inner.value_at(beat).map(|v| v - self.subtract)
    }

    /// Frozen when what it wraps is.
    fn frozen(&self) -> Option<std::sync::Arc<dyn Curve>> {
        let inner = self.inner.frozen()?;
        Some(std::sync::Arc::new(Self {
            inner,
            subtract: self.subtract,
        }))
    }
}

/// A hosted-plugin parameter as a modulation **target** — the *sub-block* sink
/// over a [`LayeredCurve`](tutti_nodes::LayeredCurve).
///
/// A plugin param never enters the audio graph, so unlike the native
/// [`AtomicTarget`](tutti_nodes::AtomicTarget) (which collapses its curve to one
/// scalar per frame and mirrors it into an atomic) this target **keeps the whole
/// curve** and re-evaluates it per beat: it implements [`Curve`] as
/// `layered.value_at(beat)`, and the plugin's per-block `ParameterChanges`
/// producer samples that several times per audio block. So a beat-varying
/// (LFO) layer traces a smooth ramp across the block instead of stepping once
/// per frame, while a constant (automation) layer sums in beside it — one
/// accumulator, finer rate.
///
/// Two ways contributions arrive:
/// - the [`ModTarget`](tutti_nodes::ModTarget) scalar API ([`accumulate`](tutti_nodes::ModTarget::accumulate))
///   installs a scalar layer (a frame-rate value, e.g. a native modulator the
///   driver already collapsed);
/// - [`set_curve_layer`](Self::set_curve_layer) installs a **beat-varying**
///   layer (an [`LfoCurve`], an automation envelope) that stays smooth.
///
/// This is how a plugin fits the one [`tutti_nodes::ModParams`] trait: a native
/// node returns an `AtomicTarget` over its own atomic; a plugin returns a
/// `PluginParamTarget`. The router routes to both identically; only the collapse
/// rate differs.
///
/// Plugin params are normalized `0..1` (the format-boundary convention), so the
/// caller passes that range; the `LayeredCurve` clamps to it.
///
/// **Lock-free reads.** The layers live in an [`RtPublish`](tutti_core::RtPublish) rather than a `Mutex`:
/// the per-block producer runs on the **audio thread** and calls `value_at`
/// several times per block, so it must never block. Reads `load()` the current
/// snapshot lock-free. Writes (control-rate, from the single ECS driver system)
/// clone the snapshot, edit, and `store` a fresh `Arc` — copy-on-write. Writer
/// edits are serialized by the one ECS system, so there is no writer/writer race;
/// the audio thread only ever reads. (Clone is an `Arc` bump per layer — cheap,
/// and off the hot path.)
pub struct PluginParamTarget {
    layered: tutti_core::RtPublish<tutti_nodes::LayeredCurve<f32>>,
}

impl PluginParamTarget {
    /// Creates a target resting at `base`, with modulation clamped to
    /// `min..=max`. All three are in the parameter's own units.
    pub fn new(base: f32, min: f32, max: f32) -> Self {
        Self {
            layered: tutti_core::RtPublish::new(tutti_nodes::LayeredCurve::new(base, min, max)),
        }
    }

    /// Copy-on-write edit: clone the current snapshot, apply `f`, store the fresh
    /// `Arc`. Off the hot path (control-rate writers only).
    fn edit(&self, f: impl FnOnce(&mut tutti_nodes::LayeredCurve<f32>)) {
        let mut next = (*self.layered.read()).clone();
        f(&mut next);
        self.layered.publish(std::sync::Arc::new(next));
    }

    /// Install (or replace) a **beat-varying** layer under `key` — an
    /// [`LfoCurve`], an automation envelope, any [`Curve`]. Re-inserting the same
    /// key updates in place. This is the path that keeps modulation smooth: the
    /// layer is evaluated at each block beat, not collapsed to a frame scalar.
    pub fn set_curve_layer(&self, key: tutti_nodes::LayerKey, curve: std::sync::Arc<dyn Curve>) {
        self.edit(|lc| lc.set_layer(key, curve));
    }
}

impl tutti_nodes::ModTarget for PluginParamTarget {
    #[inline]
    fn range(&self) -> (f32, f32) {
        self.layered.read().range()
    }
    #[inline]
    fn base(&self) -> f32 {
        self.layered.read().base()
    }
    #[inline]
    fn set_base(&self, value: f32) {
        self.edit(|lc| lc.set_base(value));
    }
    /// A scalar contribution — an allocation-free frame-rate offset. Use
    /// [`set_curve_layer`](Self::set_curve_layer) for a beat-varying source.
    #[inline]
    fn accumulate(&self, key: tutti_nodes::LayerKey, offset: f32) {
        self.edit(|lc| lc.set_scalar_layer(key, offset));
    }
    #[inline]
    fn clear(&self, key: tutti_nodes::LayerKey) {
        self.edit(|lc| lc.clear_layer(key));
    }
    /// Accepted — this is the sink curve layers exist for. Its reader is the
    /// plugin's per-block producer, which evaluates at each block beat, so a
    /// stored curve traces a smooth ramp where a frame-rate scalar would give a
    /// staircase.
    #[inline]
    fn accumulate_curve(
        &self,
        key: tutti_nodes::LayerKey,
        curve: std::sync::Arc<dyn Curve>,
    ) -> bool {
        self.set_curve_layer(key, curve);
        true
    }
    /// A frame snapshot at beat 0 — for a non-`Curve` reader. The plugin path
    /// reads the beat-accurate [`Curve::value_at`] instead.
    #[inline]
    fn final_value(&self) -> f32 {
        self.layered
            .read()
            .value_at(tutti_core::Beat(0.0))
            .unwrap_or(0.0)
    }
}

impl Curve for PluginParamTarget {
    /// `clamp(base + Σ layer(beat), range)` at the requested beat — the plugin's
    /// per-block producer (on the audio thread) calls this several times per
    /// block, **lock-free** (`RtPublish::read`), so any beat-varying layer stays
    /// smooth without blocking the callback.
    fn value_at(&self, beat: tutti_core::Beat) -> Option<f32> {
        self.layered.read().value_at(beat)
    }

    /// The **authored** part, frozen: the base, the range and the
    /// [`LayerKey::AUTOMATION`](tutti_nodes::LayerKey::AUTOMATION) layer as they
    /// stand now, with every modulation layer (scalar or curve) dropped.
    ///
    /// This target is live state: the mod router writes its layers every frame
    /// and a UI moves its base. A fork that shared it would have a live LFO
    /// writing into an export. Modulation for an export has to come from the
    /// export's own offline driver (not built yet), so until then a forked
    /// plugin renders its base plus authored automation — doc 013 gap 7.
    fn frozen(&self) -> Option<std::sync::Arc<dyn Curve>> {
        let mut authored = (*self.layered.read()).clone();
        authored.clear_mod_layers();
        // The automation layer may itself read live state (a take in progress
        // is an immutable `Recorder` once shared, but a wrapper is possible).
        Some(
            authored
                .frozen()
                .unwrap_or_else(|| std::sync::Arc::new(authored)),
        )
    }
}

/// The densest spacing of automation points within one block: every 8
/// samples. The block boundaries themselves are always sampled (offset 0 and
/// the last sample).
const SAMPLE_STRIDE: usize = 8;

/// The most points one parameter gets per block: a `ParameterQueue`'s inline
/// capacity, so a queue never spills to the heap on the audio thread.
pub(super) const MAX_POINTS: usize = 10;

/// The spacing of automation points in a `block_size`-frame block: every
/// [`SAMPLE_STRIDE`] samples, widened for a long block so the points (the
/// strides, plus the final sample) never exceed [`MAX_POINTS`]. A plugin's
/// block is its host's device callback (doc 013, decision 8 reversed: up to
/// thousands of frames), where a fixed stride of 8 would put 64 points in a
/// 512-frame block and allocate. The plugin interpolates between points, so a
/// wider spacing over a longer block is a coarser ramp, not a lost value: the
/// block's end is still exact.
pub(super) fn stride_for(block_size: usize) -> usize {
    let last = block_size.saturating_sub(1);
    SAMPLE_STRIDE.max(last.div_ceil(MAX_POINTS - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ParamId;
    use audio_automation::{AutomationEnvelope, AutomationPoint};

    // ── PluginParamTarget: a ModTarget whose value reaches the Curve path ──

    #[test]
    fn plugin_param_target_accumulates_and_exposes_via_curve() {
        use tutti_core::Beat;
        use tutti_nodes::{LayerKey, ModTarget};

        // A plugin param normalized to [0, 1], centred at 0.5.
        let t = PluginParamTarget::new(0.5, 0.0, 1.0);
        assert_eq!(ModTarget::range(&t), (0.0, 1.0));
        assert_eq!(ModTarget::base(&t), 0.5);

        // A scalar contribution is a Const layer → beat-independent.
        t.accumulate(LayerKey(1), 0.3);
        assert!((t.value_at(Beat::new(0.0)).unwrap() - 0.8).abs() < 1e-6);
        assert!((t.value_at(Beat::new(123.4)).unwrap() - 0.8).abs() < 1e-6);

        // Clearing drops back to base.
        t.clear(LayerKey(1));
        assert!((t.value_at(Beat::new(0.0)).unwrap() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn plugin_param_target_clamps_to_range() {
        use tutti_core::Beat;
        use tutti_nodes::{LayerKey, ModTarget};
        let t = PluginParamTarget::new(0.5, 0.0, 1.0);
        t.accumulate(LayerKey(1), 5.0); // overdrive
        assert!((t.value_at(Beat::new(0.0)).unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn temporal_curve_layer_stays_smooth_across_the_block() {
        // The S5 payoff: a beat-varying layer is re-evaluated at each beat the
        // plugin's per-block producer samples — so it traces the curve, NOT one
        // frame-rate step. A frame snapshot returns the SAME value at every beat
        // within a block; this must produce a spread of distinct values.
        use tutti_core::Beat;
        use tutti_nodes::automation::Curve;
        use tutti_nodes::LayerKey;

        // A beat-varying offset the test fully controls: a triangle over one
        // beat, ±0.5 around 0 — so base 0.5 + offset spans the whole [0, 1].
        struct Triangle;
        impl Curve for Triangle {
            fn value_at(&self, beat: Beat) -> Option<f32> {
                let p = beat.get().rem_euclid(1.0) as f32; // [0, 1)
                Some(if p < 0.5 {
                    -0.5 + 2.0 * p
                } else {
                    1.5 - 2.0 * p
                })
            }
        }

        let t = PluginParamTarget::new(0.5, 0.0, 1.0);
        t.set_curve_layer(LayerKey(1), std::sync::Arc::new(Triangle));

        // Sample densely across one beat. A frozen snapshot would give one value;
        // a live temporal layer spans (near) the whole range.
        let samples: Vec<f32> = (0..16)
            .map(|i| t.value_at(Beat::new(i as f64 / 16.0)).unwrap())
            .collect();
        let min = samples.iter().cloned().fold(f32::INFINITY, f32::min);
        let max = samples.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        assert!(
            max - min > 0.8,
            "a temporal layer must trace the curve within a block, not freeze — \
             got span {min}..{max}"
        );
        assert!(samples.iter().all(|v| (0.0..=1.0).contains(v)));
    }

    #[test]
    fn constant_and_temporal_layers_compose_on_plugin_target() {
        // Automation (a Const layer) + an LFO (a temporal layer) SUM in the one
        // accumulator — the composition the old override-based path lacked.
        use tutti_core::Beat;
        use tutti_nodes::{LayerKey, ModTarget};

        let t = PluginParamTarget::new(0.4, 0.0, 1.0);
        // Automation snapshot: +0.1 (a Const contribution via the scalar API).
        t.accumulate(LayerKey::AUTOMATION, 0.1);
        // LFO at its trough-crossing (sin 0 = 0) contributes 0 at beat 0.
        let lfo = LfoCurve::new(tutti_nodes::LfoShape::Sine, 1.0, 0.4, 0.0, 0.0, 0.0, 1.0);
        t.set_curve_layer(LayerKey(1), std::sync::Arc::new(lfo));
        // base 0.4 + automation 0.1 + lfo(beat0)=0 → 0.5.
        assert!((t.value_at(Beat::new(0.0)).unwrap() - 0.5).abs() < 1e-3);
        // At the peak the LFO adds +0.4·1.0 → 0.4+0.1+0.4 = 0.9 (both layers live).
        assert!((t.value_at(Beat::new(0.25)).unwrap() - 0.9).abs() < 1e-3);
    }

    #[test]
    fn plugin_param_target_installs_as_a_timed_param() {
        // The same Arc is both the routed target and the installed Curve, so
        // accumulation is visible to the per-block ParameterChanges fill.
        use tutti_nodes::{LayerKey, ModTarget};
        let target = std::sync::Arc::new(PluginParamTarget::new(0.5, 0.0, 1.0));
        let timed = TimedParam {
            param_id: ParamAddress::Opaque(ParamId::new(7)),
            curve: target.clone() as std::sync::Arc<dyn Curve>,
        };
        // Route accumulates through the ModTarget handle …
        ModTarget::accumulate(&*target, LayerKey(1), 0.25);
        // … and the installed curve sees it.
        assert!((timed.curve.value_at(tutti_core::Beat::new(0.0)).unwrap() - 0.75).abs() < 1e-6);
    }

    #[test]
    fn lfo_curve_oscillates_around_center_within_range() {
        use tutti_core::Beat;
        // Sine, 4 beats/cycle, full depth, centred at 0.5 in [0,1].
        let c = LfoCurve::new(tutti_nodes::LfoShape::Sine, 4.0, 1.0, 0.0, 0.5, 0.0, 1.0);
        // Beat 0 → sine(0) = 0 → 0.5 (centre).
        assert!((c.value_at(Beat::new(0.0)).unwrap() - 0.5).abs() < 1e-3);
        // Beat 1 (quarter cycle) → sine peak (+1) → 0.5 + 0.5 = 1.0.
        assert!((c.value_at(Beat::new(1.0)).unwrap() - 1.0).abs() < 1e-3);
        // Beat 3 (three-quarter) → sine trough (-1) → 0.0.
        assert!(c.value_at(Beat::new(3.0)).unwrap().abs() < 1e-3);
        // Always clamped into range.
        for b in 0..40 {
            let v = c.value_at(Beat::new(b as f64 * 0.1)).unwrap();
            assert!((0.0..=1.0).contains(&v), "out of range: {v}");
        }
    }

    #[test]
    fn lfo_curve_depth_scales_swing() {
        use tutti_core::Beat;
        // Depths chosen to stay in the linear (un-clamped) region: at the sine
        // peak, offset = depth·(max-min) around centre 0.5, so 0.4 → 0.9 and
        // 0.2 → 0.7, both inside [0,1]. Half the depth = half the swing.
        let big = LfoCurve::new(tutti_nodes::LfoShape::Sine, 4.0, 0.4, 0.0, 0.5, 0.0, 1.0);
        let small = LfoCurve::new(tutti_nodes::LfoShape::Sine, 4.0, 0.2, 0.0, 0.5, 0.0, 1.0);
        let f = big.value_at(Beat::new(1.0)).unwrap() - 0.5;
        let h = small.value_at(Beat::new(1.0)).unwrap() - 0.5;
        assert!(
            (f - 0.4).abs() < 1e-3,
            "0.4 depth peak swing = 0.4, got {f}"
        );
        assert!((f - 2.0 * h).abs() < 1e-3, "half depth = half swing");
    }

    #[test]
    fn lfo_curve_clamps_when_depth_overdrives_the_range() {
        use tutti_core::Beat;
        // Full depth around centre 0.5 swings ±1.0 → [-0.5, 1.5], clamped to
        // [0, 1]. The clamp is deliberate (a plugin param cannot leave range).
        let c = LfoCurve::new(tutti_nodes::LfoShape::Sine, 4.0, 1.0, 0.0, 0.5, 0.0, 1.0);
        assert!((c.value_at(Beat::new(1.0)).unwrap() - 1.0).abs() < 1e-6); // peak clamps hi
        assert!(c.value_at(Beat::new(3.0)).unwrap().abs() < 1e-6); // trough clamps lo
    }

    #[test]
    fn lfo_curve_random_is_stable_within_a_cycle() {
        use tutti_core::Beat;
        // Random shape: same value across a whole cycle, new value next cycle.
        let c = LfoCurve::new(tutti_nodes::LfoShape::Random, 4.0, 1.0, 0.0, 0.5, 0.0, 1.0);
        let a0 = c.value_at(Beat::new(0.5)).unwrap();
        let a1 = c.value_at(Beat::new(3.9)).unwrap();
        assert_eq!(a0, a1, "held stable within the 4-beat cycle");
        let b0 = c.value_at(Beat::new(4.5)).unwrap();
        // Next cycle draws a fresh value (overwhelmingly likely to differ).
        assert!(b0.is_finite());
    }

    /// `RandomSmooth` must ramp between its steps, not hold them.
    ///
    /// Hashing the cycle index is right for `Random` and wrong here: it yields
    /// one value per cycle under a name that promises interpolation, i.e.
    /// `Random`'s behaviour with the wrong label. Covering only `Random` cannot
    /// catch that — its correct behaviour is precisely "holds within a cycle".
    #[test]
    fn lfo_curve_random_smooth_ramps_within_a_cycle() {
        use tutti_core::Beat;
        // Quarter depth: a full-depth swing over `[0, 1]` saturates the clamp
        // and the ramp reads as a flat run at the rail, which would hide the
        // very difference this test exists to see.
        let c = LfoCurve::new(
            tutti_nodes::LfoShape::RandomSmooth,
            4.0,
            0.25,
            0.0,
            0.5,
            0.0,
            1.0,
        );
        let across: Vec<f32> = (0..8)
            .map(|i| c.value_at(Beat::new(4.0 + i as f64 / 2.0)).unwrap())
            .collect();
        let moved = across
            .iter()
            .filter(|v| (*v - across[0]).abs() > 1e-6)
            .count();
        assert!(
            moved >= 6,
            "a smooth shape must interpolate across its cycle, not hold: {across:?}"
        );
    }

    /// The property the threaded stepper cannot offer: evaluating the same beat
    /// twice gives the same value, so a bar sounds the same on every pass.
    #[test]
    fn lfo_curve_random_replays_identically() {
        use tutti_core::Beat;
        for shape in [
            tutti_nodes::LfoShape::Random,
            tutti_nodes::LfoShape::RandomSmooth,
        ] {
            let c = LfoCurve::new(shape, 4.0, 1.0, 0.0, 0.5, 0.0, 1.0);
            let first: Vec<f32> = (0..12)
                .map(|i| c.value_at(Beat::new(i as f64)).unwrap())
                .collect();
            // Evaluate far away, then return — as a transport seek would.
            for i in 0..20 {
                let _ = c.value_at(Beat::new(500.0 + i as f64));
            }
            let replay: Vec<f32> = (0..12)
                .map(|i| c.value_at(Beat::new(i as f64)).unwrap())
                .collect();
            assert_eq!(first, replay, "{shape:?} must be reproducible at a beat");
        }
    }

    // ── Offset layers at PRODUCTION arguments (the review gap: the existing
    //    tests used base=0.5,min=0,max=1; the real wiring uses offset layers on a
    //    base-0.5 target, span 1.0). ──────────────────────────────────────────

    /// A single-value automation envelope (an envelope with one point holds that
    /// value at every beat).
    fn const_env(value: f32) -> AutomationEnvelope<f32> {
        let mut env: AutomationEnvelope<f32> = AutomationEnvelope::new(value);
        env.add_point(AutomationPoint::new(0.0, value));
        env
    }

    #[test]
    fn lfo_offset_matches_the_native_span_scaling_no_double() {
        // Regression: LfoCurve with min=-1,max=1 multiplied the offset by
        // (max-min)=2, so a full-depth LFO was 2× too hot. LfoOffset scales by the
        // *target's* span, matching the native contract `raw · depth · (max-min)`
        // (tutti-mod driver: "no 0.5"). For a [0,1] param (span 1), full depth →
        // ±1.0 offset (clips against the param range, like native); HALF depth →
        // ±0.5 (fills [0,1] exactly, no clip). The linear depth knob is the proof
        // the 2× is gone.
        use tutti_core::Beat;
        use tutti_nodes::automation::Curve;

        // Full depth, span 1 → peak offset +1.0 (native full-span, NOT +2.0).
        let full = LfoOffset::new(tutti_nodes::LfoShape::Sine, 1.0, 1.0, 0.0, 1.0);
        let full_peak = Curve::value_at(&full, Beat::new(0.25)).unwrap(); // sin(π/2)=+1
        assert!(
            (full_peak - 1.0).abs() < 1e-3,
            "full-depth offset ±1.0 (span 1), not ±2.0 (the double-scale bug): {full_peak}"
        );
        // Half depth → half the offset — linear, proving no 2× factor.
        let half = LfoOffset::new(tutti_nodes::LfoShape::Sine, 1.0, 0.5, 0.0, 1.0);
        let half_peak = Curve::value_at(&half, Beat::new(0.25)).unwrap();
        assert!(
            (half_peak - 0.5).abs() < 1e-3,
            "depth 0.5 → ±0.5 offset (linear): {half_peak}"
        );
    }

    #[test]
    fn half_depth_lfo_fills_the_param_range_without_clipping() {
        // A depth-0.5 LFO around base 0.5 swings ±0.5 → exactly [0, 1], no clip.
        use tutti_core::Beat;

        let t = PluginParamTarget::new(0.5, 0.0, 1.0);
        let lfo = LfoOffset::new(tutti_nodes::LfoShape::Sine, 1.0, 0.5, 0.0, 1.0);
        t.set_curve_layer(tutti_nodes::LayerKey(1), std::sync::Arc::new(lfo));
        let samples: Vec<f32> = (0..16)
            .map(|i| t.value_at(Beat::new(i as f64 / 16.0)).unwrap())
            .collect();
        let min = samples.iter().cloned().fold(f32::INFINITY, f32::min);
        let max = samples.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
        // Peaks near 1.0 / troughs near 0.0, none clipped flat at the rails for
        // long (a smooth sine, not a clipped square).
        assert!(max > 0.95 && max <= 1.0 + 1e-6, "peak near 1.0: {max}");
        assert!(
            (-1e-6..0.05).contains(&min),
            "trough near 0.0 and not clipped below the rail: {min}"
        );
    }

    #[test]
    fn offset_curve_makes_automation_land_at_its_absolute_value() {
        // Regression: an automation envelope (ABSOLUTE authored value) was summed
        // straight onto base 0.5, so authored 0.7 gave clamp(0.5+0.7)=1.0.
        // OffsetCurve subtracts the base, so base + (value - base) = value exactly.
        use tutti_core::Beat;
        use tutti_nodes::LayerKey;

        for authored in [0.0_f32, 0.3, 0.5, 0.7, 1.0] {
            let t = PluginParamTarget::new(0.5, 0.0, 1.0);
            let offset = OffsetCurve::new(Arc::new(const_env(authored)), 0.5);
            t.set_curve_layer(LayerKey::AUTOMATION, Arc::new(offset));
            let got = t.value_at(Beat::new(0.0)).unwrap();
            assert!(
                (got - authored).abs() < 1e-6,
                "automation {authored} must land at {authored}, got {got}"
            );
        }
    }

    #[test]
    fn automation_and_lfo_sum_at_production_args() {
        // The whole point of S6: automation (absolute, via OffsetCurve) and an LFO
        // (bipolar offset, via LfoOffset) coexist and SUM. Authored 0.6 automation
        // + an LFO at its zero-crossing (beat 0, sin 0 = 0) → 0.6; at the peak →
        // 0.6 + depth·span·raw.
        use tutti_core::Beat;
        use tutti_nodes::LayerKey;

        let t = PluginParamTarget::new(0.5, 0.0, 1.0);
        t.set_curve_layer(
            LayerKey::AUTOMATION,
            Arc::new(OffsetCurve::new(Arc::new(const_env(0.6)), 0.5)),
        );
        // depth 0.2 sine, span 1: peak offset = 0.2.
        let lfo = LfoOffset::new(tutti_nodes::LfoShape::Sine, 1.0, 0.2, 0.0, 1.0);
        t.set_curve_layer(LayerKey(1), Arc::new(lfo));
        assert!(
            (t.value_at(Beat::new(0.0)).unwrap() - 0.6).abs() < 1e-3,
            "at the LFO zero-crossing, just the automation value (0.6)"
        );
        assert!(
            (t.value_at(Beat::new(0.25)).unwrap() - 0.8).abs() < 1e-3,
            "peak = automation 0.6 + LFO 0.2 = 0.8 (both layers SUM)"
        );
    }
}
