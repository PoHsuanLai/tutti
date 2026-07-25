//! Beat-scheduled parameter automation as a sample-accurate producer.
//!
//! The parameter-automation counterpart of [`super::harmony_source::HarmonySource`]:
//! a [`ParamAutomationSource`] holds one [`AutomationEnvelope`] per plugin
//! parameter id plus a [`TransportState`]. Each block it reads the transport
//! beat, walks the block sample-by-sample stepping the beat cursor, and fills a
//! reused [`ParameterChanges`] with one [`ParameterPoint`] per parameter at
//! sample-accurate offsets across the block window.
//!
//! Unlike chord/scale (stepwise *context* emitted only at change boundaries) an
//! automation envelope is a *continuous* signal, so we densely sample it: one
//! point per `stride` samples, matching how `AutomationLane::process` fills an
//! audio block. Plugins receive real per-block parameter ramps instead of the
//! single frame-rate `set_parameter` value the old `PluginParam` ECS path sent.
//!
//! This is deliberately the *only* automation path for hosted plugins — the
//! frame-rate `set_parameter` route was never wired for hosted-plugin params
//! (no `AutomationTarget` fed it), so there is no legacy behaviour to preserve;
//! automation reaches plugins sample-accurate or not at all.

use std::sync::Arc;

use tutti_core::transport::TransportState;
use tutti_core::Beat;
use tutti_units::automation::Curve;

use crate::host::node::input_slot::{BlockCtx, BlockInput, BlockReset};
use crate::protocol::ParameterChanges;

/// One plugin parameter's automation curve, keyed by the plugin's numeric
/// parameter id. The [`Curve`] is evaluated against the transport beat — any
/// beat-keyed curve (breakpoint envelope, constant, LFO), not just an envelope.
#[derive(Clone)]
pub struct TimedParam {
    pub param_id: u32,
    pub curve: Arc<dyn Curve>,
}

/// A pure [`tutti_units::Lfo`] as a beat-keyed [`Curve`], for driving a hosted
/// plugin parameter with an LFO through the same sample-accurate producer that
/// carries automation envelopes.
///
/// This is the plugin *adapter* over the pure modulator: it owns the transport
/// mapping (beat → phase) and the output mapping (`base + value·depth·range`,
/// clamped to `[min, max]`), while the waveform math stays in `tutti-mod`. It
/// closes the LFO→plugin gap without any fundsp node — a plugin param never
/// enters the graph, so the value is IPC-encoded like any other automation.
///
/// `Curve::value_at` is `&self` and stateless, so the deterministic shapes go
/// through the pure scan-shaped [`tutti_units::Lfo`] with a throwaway state
/// (they never read it). The random shapes can't thread the modulator's stepper
/// through a `&self` curve, so they derive a stable value from the integer beat
/// index instead — a beat-synced sample & hold, one new value per cycle.
#[derive(Clone)]
pub struct LfoCurve {
    /// The pure modulator (config only). Deterministic shapes are sampled from
    /// it; random shapes use the per-cycle hash below.
    lfo: tutti_units::Lfo,
    /// Beats per LFO cycle (beat-synced). `<= 0` freezes at the phase offset.
    beats_per_cycle: f32,
    depth: f32,
    phase_offset: f32,
    /// Output mapping: `base + shaped·(max-min)`, clamped to `[min, max]`.
    base: f32,
    min: f32,
    max: f32,
}

impl LfoCurve {
    /// Build an LFO curve. `base` is the param's un-modulated value; `depth`
    /// scales the oscillation; `[min, max]` is the param's range (the offset is
    /// `value·depth·(max-min)`, matching the control-rate path).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        shape: tutti_units::LfoShape,
        beats_per_cycle: f32,
        depth: f32,
        phase_offset: f32,
        base: f32,
        min: f32,
        max: f32,
    ) -> Self {
        Self {
            // Unit depth — `depth` is applied by `value_at`, so the modulator's
            // raw `[-1, 1]` output is what we sample here.
            lfo: tutti_units::Lfo::new(shape),
            beats_per_cycle,
            depth,
            phase_offset,
            base,
            min,
            max,
        }
    }

    /// The raw modulator value in `[-1, 1]` at a given beat, phase-deterministic.
    #[inline]
    fn raw_value(&self, beat: f64) -> f32 {
        use tutti_units::Modulator;

        let phase = if self.beats_per_cycle > 0.0 {
            ((beat as f32 / self.beats_per_cycle) + self.phase_offset).rem_euclid(1.0)
        } else {
            self.phase_offset.rem_euclid(1.0)
        };
        if self.lfo.shape.is_random() {
            // Random shapes need the stateful stepper, which we can't thread
            // through a `&self` curve. Instead derive a stable per-cycle value:
            // hash the cycle index so the value is constant within a cycle and
            // jumps at each boundary — a beat-synced sample & hold.
            let cycle = if self.beats_per_cycle > 0.0 {
                (beat as f32 / self.beats_per_cycle).floor() as i64
            } else {
                0
            };
            hash_unit_bipolar(cycle)
        } else {
            // Deterministic shape: sample the pure scan-shaped modulator with a
            // throwaway state (it never reads it) — the waveform math lives in
            // `tutti-mod`, not here.
            let seed = <tutti_units::Lfo as Modulator>::State::default();
            self.lfo.value(seed, phase).1
        }
    }
}

/// Map a cycle index to a stable pseudo-random value in `[-1, 1]` (splitmix64).
#[inline]
fn hash_unit_bipolar(n: i64) -> f32 {
    let mut z = (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    // Top 24 bits → [0, 1) → [-1, 1].
    ((z >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0
}

impl Curve for LfoCurve {
    fn value_at(&self, beat: tutti_core::Beat) -> Option<f32> {
        let shaped = self.raw_value(beat.get()) * self.depth * (self.max - self.min);
        Some((self.base + shaped).clamp(self.min, self.max))
    }
}

/// A hosted-plugin parameter as a modulation **target**.
///
/// The accumulator-based sibling of [`LfoCurve`]: where `LfoCurve` is a *source*
/// curve installed on a plugin, `PluginParamTarget` is a keyed-accumulator
/// *target* (a real [`ModTarget`]) whose modulated value reaches the plugin over
/// the SAME per-block [`ParameterChanges`] path — it implements [`Curve`] by
/// returning its current `final_value()` regardless of beat, so installing it as
/// a [`TimedParam`] streams the modulated value each block.
///
/// This is how a plugin fits the one [`tutti_units::ModParams`] trait: a native
/// node returns an [`AtomicTarget`] over its own atomic; a plugin returns a
/// `PluginParamTarget` that accumulates locally and flushes over IPC. The router
/// routes to both identically.
///
/// Plugin params are normalized `0..1` (see the format-boundary convention), so
/// the caller passes that range; the inner [`AtomicTarget`] clamps to it.
pub struct PluginParamTarget {
    inner: tutti_units::AtomicTarget,
}

impl PluginParamTarget {
    pub fn new(base: f32, min: f32, max: f32) -> Self {
        Self {
            inner: tutti_units::AtomicTarget::new(base, min, max),
        }
    }
}

impl tutti_units::ModTarget for PluginParamTarget {
    #[inline]
    fn range(&self) -> (f32, f32) {
        self.inner.range()
    }
    #[inline]
    fn base(&self) -> f32 {
        self.inner.base()
    }
    #[inline]
    fn set_base(&self, value: f32) {
        self.inner.set_base(value);
    }
    #[inline]
    fn accumulate(&self, key: tutti_units::LayerKey, offset: f32) {
        self.inner.accumulate(key, offset);
    }
    #[inline]
    fn clear(&self, key: tutti_units::LayerKey) {
        self.inner.clear(key);
    }
    #[inline]
    fn final_value(&self) -> f32 {
        self.inner.final_value()
    }
}

impl Curve for PluginParamTarget {
    /// The target's current modulated value, independent of beat — installing
    /// this as a [`TimedParam`] ships `final_value()` into the plugin's per-block
    /// [`ParameterChanges`] stream, so accumulation done by the router this frame
    /// reaches the plugin next block.
    fn value_at(&self, _beat: tutti_core::Beat) -> Option<f32> {
        Some(tutti_units::ModTarget::final_value(self))
    }
}

/// How many samples between successive automation points within one block. A
/// block is at most `fundsp::MAX_BUFFER_SIZE` (64) samples, so a stride of 8
/// yields up to 8 points per parameter per block — dense enough for smooth
/// ramps, cheap enough to stay allocation-light. The block boundaries
/// themselves are always sampled (offset 0 and the last sample).
const SAMPLE_STRIDE: usize = 8;

/// Beat-scheduled parameter-automation producer. Cheap to clone (envelopes
/// shared via `Arc`, transport shared via `Arc`) so the fundsp graph-commit
/// clone of the parent node doesn't reallocate the curves.
#[derive(Clone)]
pub struct ParamAutomationSource {
    params: Arc<[TimedParam]>,
    transport: Arc<dyn TransportState>,
    sample_rate: f64,
}

impl ParamAutomationSource {
    /// Build a parameter-automation source from one envelope per parameter id.
    ///
    /// Takes a [`TransportState`], not a bare [`Timeline`](tutti_core::transport::Timeline):
    /// `fill` reads `loop_range()` to wrap the beat inside the active cycle, and
    /// looping lives on the live supertrait. An offline render never drives this
    /// source.
    pub fn new(
        params: impl IntoIterator<Item = TimedParam>,
        transport: Arc<dyn TransportState>,
        sample_rate: f64,
    ) -> Self {
        Self {
            params: params.into_iter().collect::<Vec<_>>().into(),
            transport,
            sample_rate,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.params.is_empty()
    }

    /// Fill `out` (cleared first) with one [`ParameterQueue`] per parameter,
    /// densely sampled across this block's beat window. No points are emitted
    /// while the transport is stopped or at non-positive tempo/rate — the
    /// plugin keeps its last value, matching how a paused transport freezes the
    /// playhead.
    ///
    /// RT-alloc-free: reuses the queue storage already in `out` across blocks
    /// (points are reset in place, queues are grown only when the param count
    /// rises), and each queue's inline capacity holds a full block's points
    /// without spilling (see `ParameterQueue`).
    ///
    /// [`ParameterQueue`]: crate::protocol::ParameterQueue
    pub fn fill(&self, block_size: usize, out: &mut ParameterChanges) {
        // Retain `out.queues`' capacity across blocks; reset each queue's points
        // in place below rather than dropping and reallocating the queue.
        for q in out.queues.iter_mut() {
            q.points.clear();
        }
        if block_size == 0
            || self.params.is_empty()
            || !self.transport.is_rolling()
        {
            out.queues.clear();
            return;
        }
        let start_beat = self.transport.beat().get();
        let tempo_bpm = self.transport.tempo().get();
        if tempo_bpm <= 0.0 || self.sample_rate <= 0.0 {
            out.queues.clear();
            return;
        }
        let beats_per_sample = tempo_bpm / 60.0 / self.sample_rate;
        let loop_range = self.transport.loop_range();
        let last = block_size - 1;

        // Ensure `out.queues` has exactly one slot per parameter, reusing the
        // slots (and their inline point storage) that already exist. Growing
        // pushes; shrinking truncates — both retain capacity for next block.
        for (i, param) in self.params.iter().enumerate() {
            if i < out.queues.len() {
                out.queues[i].param_id = param.param_id;
            } else {
                out.queues
                    .push(crate::protocol::ParameterQueue::new(param.param_id));
            }
            let queue = &mut out.queues[i];
            // Sample at 0, every `SAMPLE_STRIDE`, and always the final sample so
            // the block's end value is exact (the next block starts from here).
            let mut offset = 0usize;
            loop {
                let beat = Beat::new(start_beat + offset as f64 * beats_per_sample);
                // `LoopRange::wrap` clamps a beat into the loop region; it is a
                // no-op when there is no region or the beat is already inside.
                let eff_beat = match loop_range {
                    Some(region) => region.wrap(beat),
                    None => beat,
                };
                // A curve with no value here (empty / disabled) yields `None`,
                // so an empty curve simply contributes no points — the queue
                // stays empty (dropped by `is_empty` consumers), leaving the
                // plugin at its last value.
                if let Some(v) = param.curve.value_at(eff_beat) {
                    queue.add_point(offset as i32, v as f64);
                }
                if offset == last {
                    break;
                }
                offset = (offset + SAMPLE_STRIDE).min(last);
            }
        }
        // Drop any stale trailing queues from a previous, larger param set
        // (retains their capacity for a future block that grows again).
        out.queues.truncate(self.params.len());
    }
}

impl BlockInput for ParamAutomationSource {
    type Out = ParameterChanges;
    fn fill(&self, ctx: BlockCtx, out: &mut ParameterChanges) {
        // Inherent `fill` self-clears, so it satisfies the "fully overwrite
        // `out`" contract.
        ParamAutomationSource::fill(self, ctx.block_size, out);
    }
}

impl BlockReset for ParameterChanges {
    fn reset(&mut self) {
        self.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_float::AtomicF64;
    use audio_automation::{AutomationEnvelope, AutomationPoint};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tutti_core::params::Bpm;
    use tutti_core::transport::Timeline;

    // ── PluginParamTarget: a ModTarget whose value reaches the Curve path ──

    #[test]
    fn plugin_param_target_accumulates_and_exposes_via_curve() {
        use tutti_core::Beat;
        use tutti_units::{LayerKey, ModTarget};

        // A plugin param normalized to [0, 1], centred at 0.5.
        let t = PluginParamTarget::new(0.5, 0.0, 1.0);
        assert_eq!(ModTarget::range(&t), (0.0, 1.0));
        assert_eq!(ModTarget::base(&t), 0.5);

        // Route accumulates an offset → the Curve read (per block) reflects it.
        t.accumulate(LayerKey(1), 0.3);
        assert!((t.value_at(Beat::new(0.0)).unwrap() - 0.8).abs() < 1e-6);
        // Beat is ignored — a mod target's value is beat-independent.
        assert!((t.value_at(Beat::new(123.4)).unwrap() - 0.8).abs() < 1e-6);

        // Clearing drops back to base.
        t.clear(LayerKey(1));
        assert!((t.value_at(Beat::new(0.0)).unwrap() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn plugin_param_target_clamps_to_range() {
        use tutti_core::Beat;
        use tutti_units::{LayerKey, ModTarget};
        let t = PluginParamTarget::new(0.5, 0.0, 1.0);
        t.accumulate(LayerKey(1), 5.0); // overdrive
        assert!((t.value_at(Beat::new(0.0)).unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn plugin_param_target_installs_as_a_timed_param() {
        // The same Arc is both the routed target and the installed Curve, so
        // accumulation is visible to the per-block ParameterChanges fill.
        use tutti_units::{LayerKey, ModTarget};
        let target = std::sync::Arc::new(PluginParamTarget::new(0.5, 0.0, 1.0));
        let timed = TimedParam {
            param_id: 7,
            curve: target.clone() as std::sync::Arc<dyn Curve>,
        };
        // Route accumulates through the ModTarget handle …
        ModTarget::accumulate(&*target, LayerKey(1), 0.25);
        // … and the installed curve sees it.
        assert!((timed.curve.value_at(tutti_core::Beat::new(0.0)).unwrap() - 0.75).abs() < 1e-6);
    }

    struct TestTransport {
        beat: AtomicF64,
        tempo: f64,
        playing: AtomicBool,
    }
    impl TestTransport {
        fn new(tempo: f64) -> Self {
            Self {
                beat: AtomicF64::new(0.0),
                tempo,
                playing: AtomicBool::new(true),
            }
        }
        fn set_beat(&self, b: f64) {
            self.beat.store(b, Ordering::Release);
        }
    }
    impl Timeline for TestTransport {
        fn beat(&self) -> tutti_core::Beat {
            tutti_core::Beat(self.beat.load(Ordering::Acquire))
        }
        fn is_rolling(&self) -> bool {
            self.playing.load(Ordering::Acquire)
        }
        fn tempo(&self) -> Bpm {
            Bpm(self.tempo)
        }
    }
    impl TransportState for TestTransport {
        fn is_recording(&self) -> bool {
            false
        }
        fn loop_range(&self) -> Option<tutti_core::LoopRange> {
            None
        }
    }

    /// A 0→1 ramp over 4 beats, labelled with parameter id `7`.
    fn ramp(param_id: u32) -> TimedParam {
        let mut env: AutomationEnvelope<f32> = AutomationEnvelope::new(0.0f32);
        env.add_point(AutomationPoint::new(0.0, 0.0));
        env.add_point(AutomationPoint::new(4.0, 1.0));
        TimedParam {
            param_id,
            curve: Arc::new(env),
        }
    }

    #[test]
    fn fills_one_queue_per_param_with_ramp() {
        let transport = Arc::new(TestTransport::new(120.0)); // 22050 samples/beat @ 44.1k
        let src = ParamAutomationSource::new(
            vec![ramp(7)],
            Arc::clone(&transport) as Arc<dyn TransportState>,
            44100.0,
        );
        let mut out = ParameterChanges::new();
        src.fill(64, &mut out);
        assert_eq!(out.queues.len(), 1);
        let q = &out.queues[0];
        assert_eq!(q.param_id, 7);
        // First point at offset 0, beat 0 → value 0.
        assert_eq!(q.points[0].sample_offset, 0);
        assert!(q.points[0].value.abs() < 1e-6);
        // Points ascend in offset and the last is the block's final sample.
        assert_eq!(q.points.last().unwrap().sample_offset, 63);
        for w in q.points.windows(2) {
            assert!(w[1].sample_offset > w[0].sample_offset);
            assert!(w[1].value >= w[0].value); // ramp is monotonic up
        }
    }

    #[test]
    fn paused_emits_nothing() {
        let transport = Arc::new(TestTransport::new(120.0));
        transport.playing.store(false, Ordering::Release);
        let src = ParamAutomationSource::new(
            vec![ramp(7)],
            Arc::clone(&transport) as Arc<dyn TransportState>,
            44100.0,
        );
        let mut out = ParameterChanges::new();
        src.fill(64, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn advancing_transport_moves_the_value() {
        let transport = Arc::new(TestTransport::new(120.0));
        let src = ParamAutomationSource::new(
            vec![ramp(7)],
            Arc::clone(&transport) as Arc<dyn TransportState>,
            44100.0,
        );
        let mut out = ParameterChanges::new();
        src.fill(64, &mut out);
        let start_v = out.queues[0].points[0].value;
        // Jump to beat 4 (envelope top) — the first point should now read ~1.0.
        transport.set_beat(4.0);
        src.fill(64, &mut out);
        let later_v = out.queues[0].points[0].value;
        assert!(later_v > start_v);
        assert!((later_v - 1.0).abs() < 1e-3);
    }

    #[test]
    fn lfo_curve_oscillates_around_center_within_range() {
        use tutti_core::Beat;
        // Sine, 4 beats/cycle, full depth, centred at 0.5 in [0,1].
        let c = LfoCurve::new(tutti_units::LfoShape::Sine, 4.0, 1.0, 0.0, 0.5, 0.0, 1.0);
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
        let big = LfoCurve::new(tutti_units::LfoShape::Sine, 4.0, 0.4, 0.0, 0.5, 0.0, 1.0);
        let small = LfoCurve::new(tutti_units::LfoShape::Sine, 4.0, 0.2, 0.0, 0.5, 0.0, 1.0);
        let f = big.value_at(Beat::new(1.0)).unwrap() - 0.5;
        let h = small.value_at(Beat::new(1.0)).unwrap() - 0.5;
        assert!((f - 0.4).abs() < 1e-3, "0.4 depth peak swing = 0.4, got {f}");
        assert!((f - 2.0 * h).abs() < 1e-3, "half depth = half swing");
    }

    #[test]
    fn lfo_curve_clamps_when_depth_overdrives_the_range() {
        use tutti_core::Beat;
        // Full depth around centre 0.5 swings ±1.0 → [-0.5, 1.5], clamped to
        // [0, 1]. The clamp is deliberate (a plugin param cannot leave range).
        let c = LfoCurve::new(tutti_units::LfoShape::Sine, 4.0, 1.0, 0.0, 0.5, 0.0, 1.0);
        assert!((c.value_at(Beat::new(1.0)).unwrap() - 1.0).abs() < 1e-6); // peak clamps hi
        assert!(c.value_at(Beat::new(3.0)).unwrap().abs() < 1e-6); // trough clamps lo
    }

    #[test]
    fn lfo_curve_random_is_stable_within_a_cycle() {
        use tutti_core::Beat;
        // Random shape: same value across a whole cycle, new value next cycle.
        let c = LfoCurve::new(tutti_units::LfoShape::Random, 4.0, 1.0, 0.0, 0.5, 0.0, 1.0);
        let a0 = c.value_at(Beat::new(0.5)).unwrap();
        let a1 = c.value_at(Beat::new(3.9)).unwrap();
        assert_eq!(a0, a1, "held stable within the 4-beat cycle");
        let b0 = c.value_at(Beat::new(4.5)).unwrap();
        // Next cycle draws a fresh value (overwhelmingly likely to differ).
        assert!(b0.is_finite());
    }

    #[test]
    fn full_block_fill_does_not_spill_the_point_smallvec() {
        // A stride-8 sample over a full 64-sample block emits offsets
        // 0,8,16,24,32,40,48,56,63 = 9 points. The queue's inline capacity (10)
        // must hold them without spilling to the heap — the RT-alloc guarantee.
        let transport = Arc::new(TestTransport::new(120.0));
        let src = ParamAutomationSource::new(
            vec![ramp(7)],
            Arc::clone(&transport) as Arc<dyn Timeline>,
            44100.0,
        );
        let mut out = ParameterChanges::new();
        src.fill(64, &mut out);
        let q = &out.queues[0];
        assert_eq!(q.points.len(), 9, "stride-8 over 64 samples = 9 points");
        assert!(
            !q.points.spilled(),
            "a full block's points must stay inline (no heap spill)"
        );
    }

    #[test]
    fn refill_reuses_queue_storage_without_reallocating() {
        // Second fill into the same `out` must reuse the existing queue slot and
        // its inline point buffer — no new queue is pushed, so a plugin under a
        // rolling transport allocates nothing per block after the first.
        let transport = Arc::new(TestTransport::new(120.0));
        let src = ParamAutomationSource::new(
            vec![ramp(7)],
            Arc::clone(&transport) as Arc<dyn Timeline>,
            44100.0,
        );
        let mut out = ParameterChanges::new();
        src.fill(64, &mut out);
        assert_eq!(out.queues.len(), 1);
        let ptr_before = out.queues[0].points.as_ptr();
        transport.set_beat(1.0);
        src.fill(64, &mut out);
        assert_eq!(out.queues.len(), 1, "no extra queue pushed on refill");
        assert_eq!(
            out.queues[0].points.as_ptr(),
            ptr_before,
            "inline point buffer reused in place (no realloc)"
        );
    }

    #[test]
    fn empty_source_and_empty_envelope_emit_nothing() {
        let transport = Arc::new(TestTransport::new(120.0)) as Arc<dyn TransportState>;
        let empty_src = ParamAutomationSource::new(Vec::new(), Arc::clone(&transport), 44100.0);
        let mut out = ParameterChanges::new();
        empty_src.fill(64, &mut out);
        assert!(out.is_empty());

        let empty_env: AutomationEnvelope<f32> = AutomationEnvelope::new(0.0f32);
        let src = ParamAutomationSource::new(
            vec![TimedParam {
                param_id: 3,
                curve: Arc::new(empty_env),
            }],
            transport,
            44100.0,
        );
        src.fill(64, &mut out);
        assert!(out.is_empty());
    }
}
