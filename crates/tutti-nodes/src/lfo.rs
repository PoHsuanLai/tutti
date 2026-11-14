//! The audio-rate modulation adapter.
//!
//! [`ModulatorNode<M>`] is the graph adapter over a pure [`Modulator`]: it
//! owns everything *audio* — the `tutti_graph::Node` impl, the clock
//! position, the sample rate, the beat read from each block's `Env` — and
//! calls the modulator only for the one pure step `cycles -> value`. The
//! modulator itself knows nothing of transport or audio.
//!
//! [`LfoNode`] is `ModulatorNode<Waveform>` — the concrete, monomorphized LFO node
//! the graph builds, over the same [`Waveform`] a control lane evaluates, so a
//! beat-synced node and lane produce one sequence (a random wave included).
//! A free-running node runs from its insert and does not restart at play.
//! Because `M` is a concrete type param (not `Box<dyn>`), `value()` inlines.
//!
//! `LfoShape` and `Waveform` are re-exported so `use tutti_nodes::LfoShape`
//! resolves here.

use tutti_types::UnitParam;

use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::{BeatsPerCycle, ChannelLayout, Depth, Hz, Param, PhaseIncrement, SampleRate};
use tutti_types::{Latency, Tail};

// The waveform vocabulary is tutti-modulation's floor, re-exported so `use
// tutti_nodes::LfoShape` resolves.
pub use crate::modulator::Modulator;
pub use tutti_modulation::{LfoShape, Waveform};

/// What an LFO's phase follows: its own frame count, or the transport beat.
///
/// A beat-synced clock's span is part of the clock, set when the node is
/// built and checked there, so no cell holds beats where a rate in [`Hz`]
/// could be written, and a cycle position is always the beat divided by it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LfoClock {
    /// The phase advances from the node's own frame count at the rate in its
    /// rate param ([`UnitParam::Rate`], in [`Hz`]), independent of the
    /// transport, and keeps running while the transport is stopped. A negative
    /// rate runs the phase backwards, and 0 holds it: the value at a position
    /// is a pure function of it, so either is well defined.
    Free,
    /// The phase is read from the transport beat of each frame of the block
    /// ([`tutti_graph::Env::for_each_beat`]), this many beats a cycle: a
    /// larger span is slower. Sample-accurate, and the same offline.
    Synced(BeatsPerCycle),
}

/// The clock's name (`Free Running` or `Beat Synced`), for a UI's selector:
/// display text, not a stable identifier, so do not parse or persist it.
impl core::fmt::Display for LfoClock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Free => "Free Running",
            Self::Synced(_) => "Beat Synced",
        })
    }
}

/// The audio-rate modulation node: a graph node (`tutti_graph::Node`,
/// no inputs, one output) that computes a clock position in unwrapped cycles
/// — free-running, or from the transport beat of its block's `Env` — and
/// drives a pure [`Modulator`] `M`.
///
/// Owns everything audio; `M` owns the pure `cycles -> value`. The depth is
/// held here and only here (a live atomic the UI can write), multiplied onto
/// the modulator's output.
///
/// **Addressable.** The value at a position is a pure function of it, so a
/// random wave steps on the cycle index ([`Waveform::raw_at`]): beat-synced, it
/// replays after a seek; free-running, a fork (which starts at cycle zero)
/// replays the original's sequence from its start.
///
/// # In a graph
///
/// A graph node ([`IntoNode`]): inserted, its controls are a [`ParamSet`]
/// over the depth ([`UnitParam::Depth`]) and, free-running, the rate
/// ([`UnitParam::Rate`], in [`Hz`]), the one route to them. Both are read
/// per frame, so a driven port's every frame is heard. A beat-synced node's
/// span is its [`LfoClock`], fixed at build. A fork starts from the values
/// last set through the set, at phase zero. It is a source with no ring-out
/// ([`Tail::None`]): what it emits is a function of the clock, so a render's end
/// ends it, and the executor never skips a node with no inputs.
///
/// **Arrival.** It has no inputs, so its compiled arrival is zero by
/// construction and the beat it reads is its block's own; a consumer behind a
/// latent path is aligned by the compiler, which delays this node's edge into
/// it like any other source's.
#[derive(Debug)]
pub struct ModulatorNode<M: Modulator> {
    modulator: M,
    clock: LfoClock,
    /// The free-running rate: read only by [`LfoClock::Free`], and declared as
    /// a param only then.
    rate: Param<Hz>,
    depth: Param<Depth>,
    /// Added to the clock position before the modulator reads it.
    phase_offset: PhaseIncrement,
    /// Free-running clock position, in unwrapped cycles (`f64`, so the cycle
    /// index a random wave steps on stays exact for as long as a session
    /// runs; no unit type covers an unwrapped position).
    position: f64,
    /// The rate [`Node::prepare`] set; `None` until then, when a free-running
    /// clock has no step and the node renders silence.
    sample_rate: Option<SampleRate>,
}

/// The concrete LFO node the graph builds — a [`ModulatorNode`] driving a
/// [`Waveform`], the one a control lane evaluates. Monomorphized, so `value()`
/// inlines.
///
/// **The per-sample tier.** Reach for this when a control-rate value is too
/// coarse (audio-rate modulation): an audio generator whose output feeds a
/// node's own audio input. Beat-synced it reads the
/// beat of every frame from its block's `Env`, so it is sample-accurate and
/// renders identically offline.
///
/// The control-rate alternative is a `tutti_modulation::Source::Lfo` route, which a
/// control lane evaluates into the param's port.
pub type LfoNode = ModulatorNode<Waveform>;

impl ModulatorNode<Waveform> {
    /// Creates a free-running LFO of `shape` (seed 0) at a rate of
    /// 1.0 Hz.
    ///
    /// Chain `.with_rate(hz)` or `.with_beat_sync(beats)` to configure
    /// further. Its phase increment takes the rate [`Node::prepare`] hands
    /// it.
    #[must_use]
    pub fn new(shape: LfoShape) -> Self {
        Self::with_modulator(Waveform::Shape { shape, seed: 0 })
    }

    /// The random shapes' seed: seed 0 is the plain sequence, the one a
    /// [`tutti_modulation::Lfo`] of the same seed produces in a control lane.
    /// Ignored by a wave that is not a shape.
    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        if let Waveform::Shape { seed: s, .. } = &mut self.modulator {
            *s = seed;
        }
        self
    }
}

/// The node for a control-rate LFO's declaration: the same shape, seed,
/// rate and phase. Beat-synced, it produces the values a control lane
/// evaluates from that [`tutti_modulation::Lfo`], at audio rate. Free-running
/// (`Hz`), it runs at that rate from its insert and does not restart
/// at play, where the lane's `Hz` clock is the graph's play clock.
impl From<tutti_modulation::Lfo> for LfoNode {
    fn from(lfo: tutti_modulation::Lfo) -> Self {
        let node = Self::new(lfo.shape)
            .with_seed(lfo.seed)
            .with_phase_offset(lfo.phase);
        match lfo.rate {
            tutti_modulation::LfoRate::Beats(per_cycle) => node.with_beat_sync(per_cycle),
            tutti_modulation::LfoRate::Hz(rate) => node.with_rate(rate),
        }
    }
}

impl<M: Modulator> ModulatorNode<M> {
    /// Builds a modulation node over an arbitrary pure modulator, free-running
    /// at 1 Hz. The generic entry point behind [`LfoNode::new`].
    ///
    /// It takes no sample rate: the graph hands it the device rate in
    /// [`Node::prepare`] before its first block.
    pub fn with_modulator(modulator: M) -> Self {
        Self {
            modulator,
            clock: LfoClock::Free,
            rate: Param::new(Hz(1.0)),
            depth: Param::new(Depth::FULL),
            phase_offset: PhaseIncrement(0.0),
            position: 0.0,
            sample_rate: None,
        }
    }

    /// Runs free at `rate` cycles a second ([`LfoClock::Free`]).
    #[must_use]
    pub fn with_rate(mut self, rate: impl Into<Hz>) -> Self {
        self.clock = LfoClock::Free;
        self.rate.store(rate.into());
        self
    }

    /// Follows the transport beat, `beats_per_cycle` beats a cycle
    /// ([`LfoClock::Synced`]): a larger span is slower.
    #[must_use]
    pub fn with_beat_sync(mut self, beats_per_cycle: BeatsPerCycle) -> Self {
        self.clock = LfoClock::Synced(beats_per_cycle);
        self
    }

    /// Sets the modulation [`Depth`].
    ///
    /// Bipolar: a negative depth inverts the modulator, so `-1.0` is the same
    /// shape phase-flipped and `0.0` is flat.
    #[must_use]
    pub fn with_depth(self, depth: impl Into<Depth>) -> Self {
        self.depth.store(depth.into());
        self
    }

    /// Sets the [`PhaseIncrement`] offset, conventionally `0.0` to `1.0` for one
    /// full cycle.
    ///
    /// Added to the unwrapped clock position, so any real value is valid: the
    /// wave takes the phase as the position's `floor`-based fraction, so a
    /// negative offset lags rather than reading off the front of the shape. An
    /// offset past a whole cycle also moves a random wave to a later step.
    #[must_use]
    pub fn with_phase_offset(mut self, offset: impl Into<PhaseIncrement>) -> Self {
        self.phase_offset = offset.into();
        self
    }

    /// Returns what the phase follows.
    #[must_use]
    pub fn clock(&self) -> LfoClock {
        self.clock
    }

    /// The one call into the pure modulator, at `cycles` plus the phase
    /// offset, with the depth applied.
    #[inline]
    fn evaluate(&self, cycles: f64, offset: PhaseIncrement, depth: f32) -> f32 {
        self.modulator.value(cycles + f64::from(offset.get())) * depth
    }
}

// The params a [`ModulatorNode`] lets the graph modulate, in port order: its
// depth and, free-running, its rate (a beat-synced node's span is its clock).
// Both are read per frame. The clock is fixed once the node is live (`with_*`
// only), so its shape never changes.
tutti_graph::declare_params!([M: Modulator + Send + 'static] ModulatorNode<M>, n {
    Depth => Depth = &n.depth,
    Rate => Hz = matches!(n.clock, LfoClock::Free).then_some(&n.rate),
});

impl<M: Modulator + Send + 'static> Node for ModulatorNode<M> {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
            .with_cost(tutti_graph::Cost::Moderate)
            .with_tail(Tail::None)
            .with_param_addrs(self.declared_ports().as_slice())
    }

    fn prepare(&mut self, p: &Prepare) {
        self.sample_rate = Some(p.sample_rate());
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        debug_assert_eq!(
            cx.arrival,
            Latency::ZERO,
            "a node with no inputs arrives at zero"
        );
        let size = io.frames();
        // A driven depth is clamped into a `Depth` (a written one is one). A
        // non-finite driven frame reads the cell instead: a NaN rate would
        // leave the phase NaN for good.
        let depth_cell = self.depth.load();
        let depth_lane = self
            .declared_param::<{ UnitParam::Depth as u16 }>(&io)
            .frames();
        let depth_at = |i: usize| {
            depth_lane
                .map(|l| l[i])
                .filter(|v| v.is_finite())
                .map_or(depth_cell, Depth::clamped)
                .get()
        };
        let offset = self.phase_offset;
        match self.clock {
            LfoClock::Free => {
                let rate_cell = self.rate.load().get();
                let rate_lane = self
                    .declared_param::<{ UnitParam::Rate as u16 }>(&io)
                    .frames();
                // In f64, not `PhaseIncrement::per_sample` (f32): the step is
                // accumulated for as long as the node runs, and an f32-rounded
                // step drifts the cycle index off the frame.
                let out = io.output(0);
                let Some(sample_rate) = self.sample_rate else {
                    out[..size].fill(0.0);
                    return Status::Modified;
                };
                let per_hz = 1.0 / sample_rate.get();
                for (i, o) in out[..size].iter_mut().enumerate() {
                    let rate = rate_lane
                        .map(|l| l[i])
                        .filter(|v| v.is_finite())
                        .unwrap_or(rate_cell);
                    *o = self.evaluate(self.position, offset, depth_at(i));
                    self.position += f64::from(rate) * per_hz;
                }
            }
            LfoClock::Synced(beats_per_cycle) => {
                let out = io.output(0);
                cx.env.for_each_beat(|i, beat| {
                    let cycles = beat.cycles_in(beats_per_cycle);
                    out[i] = self.evaluate(cycles, offset, depth_at(i));
                });
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.position = 0.0;
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        self.declared_base(k)
    }
}

impl<M: Modulator + Clone + Send + 'static> ParamNode for ModulatorNode<M> {
    /// The depth, and the rate while free-running (a beat-synced node's span is
    /// its clock).
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }

    /// A clone with every control cell detached (at its value now), at cycle
    /// zero: a free-running fork replays the original's values from its
    /// start.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.rate.detach();
        fork.depth.detach();
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork that starts
/// from the values last set through it ([`tutti_graph::param_parts`]).
impl<M: Modulator + Clone + Send + 'static> IntoNode for ModulatorNode<M> {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

impl<M: Modulator + Clone> Clone for ModulatorNode<M> {
    fn clone(&self) -> Self {
        Self {
            modulator: self.modulator.clone(),
            clock: self.clock,
            rate: self.rate.handle(),
            depth: self.depth.handle(),
            phase_offset: self.phase_offset,
            position: self.position,
            sample_rate: self.sample_rate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_graph::contract::{Direct, assert_param_fork};
    use tutti_graph::{Solo, TransportPosition};
    use tutti_types::MaxBlock;
    use tutti_types::UnitParam;
    use tutti_types::{Beat, Bpm, Samples};

    /// A free-running LFO declares its depth and rate, each port's base its
    /// cell; a beat-synced one declares only its depth, its span being its
    /// clock.
    #[test]
    fn the_shape_declares_depth_and_a_free_running_rate() {
        let free = LfoNode::new(LfoShape::Sine)
            .with_rate(Hz(2.0))
            .with_depth(Depth::of(0.25));
        assert_eq!(
            free.shape().params.as_slice(),
            &[UnitParam::Depth, UnitParam::Rate][..]
        );
        assert_eq!(free.param_base(0), Some(0.25));
        assert_eq!(free.param_base(1), Some(2.0));
        let synced = LfoNode::new(LfoShape::Sine)
            .with_beat_sync(BeatsPerCycle::of(4.0))
            .with_depth(Depth::of(0.5));
        assert_eq!(synced.shape().params.as_slice(), &[UnitParam::Depth][..]);
        assert_eq!(synced.param_base(0), Some(0.5));
        assert_eq!(synced.param_base(1), None);
    }

    // The waveform math is `tutti-modulation`'s, and so are the tests that pin it.
    // What belongs here is the adapter around it: phase generation from
    // transport, depth, and the node surface.

    /// `n` frames of `node` at `sr` in one block, transport stopped at beat 0.
    fn render(node: LfoNode, sr: f64, n: usize) -> Vec<f32> {
        let mut d = Direct::new(node, SampleRate::of(sr), n);
        d.block();
        d.output(0).to_vec()
    }

    /// `n` frames of `node` through a graph at 48 kHz, each block handed
    /// `transport` as its own (advancing, when it rolls, by the block).
    fn render_at(node: LfoNode, transport: TransportPosition, n: usize) -> Vec<f32> {
        let rate = SampleRate::SR_48K;
        let mut solo = Solo::new(node, Prepare::new(rate, MaxBlock::of(Samples(256))));
        solo.renderer_mut().set_transport_fn(move |frame| {
            let mut clock = tutti_types::FrameClock::new(transport.beat(), transport.tempo, rate);
            if transport.playing {
                clock.advance(Samples(frame.get() as usize), None);
            }
            TransportPosition::counted(transport.playing, transport.tempo, clock.origin(), None)
        });
        solo.render(n).remove(0)
    }

    fn stopped_at(beat: f64) -> TransportPosition {
        TransportPosition::new(false, Bpm::of(120.0), Beat(beat), None)
    }

    #[test]
    fn test_free_running_lfo() {
        let out = render(LfoNode::new(LfoShape::Sine), 100.0, 25);
        // The 25th sample: a quarter of a 1 Hz cycle at 100 Hz, the peak.
        assert!(
            (out[24] - 1.0).abs() < 0.1,
            "Expected ~1.0, got {}",
            out[24]
        );
    }

    /// Setting a rate in one clock's unit selects that clock, the last one
    /// set winning: a free rate leaves beat sync behind and a span replaces a
    /// free rate.
    #[test]
    fn setting_a_rate_selects_its_clock() {
        let lfo = LfoNode::new(LfoShape::Sine)
            .with_beat_sync(BeatsPerCycle::of(4.0))
            .with_rate(Hz(2.0));
        assert_eq!(lfo.clock(), LfoClock::Free);
        assert_eq!(lfo.rate.load(), Hz(2.0));
        let lfo = LfoNode::new(LfoShape::Sine)
            .with_rate(Hz(2.0))
            .with_beat_sync(BeatsPerCycle::of(4.0));
        assert_eq!(lfo.clock(), LfoClock::Synced(BeatsPerCycle::of(4.0)));
    }

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it (see
    /// `tutti_graph::contract::assert_param_fork`). The rate is addressed
    /// only while free-running.
    #[test]
    fn a_fork_starts_from_the_authored_values_and_shares_nothing() {
        let free = LfoNode::new(LfoShape::Sine).with_rate(Hz(2.0));
        assert_eq!(
            free.param_set().params().collect::<Vec<_>>(),
            [UnitParam::Depth, UnitParam::Rate]
        );
        assert_param_fork(free);
        let synced = LfoNode::new(LfoShape::Sine).with_beat_sync(BeatsPerCycle::of(4.0));
        assert_eq!(
            synced.param_set().params().collect::<Vec<_>>(),
            [UnitParam::Depth]
        );
        assert_param_fork(synced);
    }

    /// No inputs, one output, no ring-out: what a clock-driven source emits
    /// ends with the render, so a bounce adds nothing past its span for it.
    #[test]
    fn a_source_has_no_ring_out() {
        for lfo in [
            LfoNode::new(LfoShape::Sine),
            LfoNode::new(LfoShape::Sine).with_beat_sync(BeatsPerCycle::of(1.0)),
        ] {
            let shape = lfo.shape();
            assert_eq!(shape.audio_in().count(), 0);
            assert_eq!(shape.audio_out().count(), 1);
            assert_eq!(shape.tail, Tail::None);
        }
    }

    #[test]
    fn test_beat_synced_lfo() {
        let lfo = || LfoNode::new(LfoShape::Sine).with_beat_sync(BeatsPerCycle::of(4.0));
        // Beat 1.0 of a 4-beat cycle = quarter phase = sine peak.
        let out = render_at(lfo(), stopped_at(1.0), 1);
        assert!((out[0] - 1.0).abs() < 0.01, "Expected 1.0, got {}", out[0]);
        let out = render_at(lfo(), stopped_at(2.0), 1);
        assert!((out[0] - 0.0).abs() < 0.01, "Expected 0.0, got {}", out[0]);
    }

    /// While the transport rolls, each frame is evaluated at its own beat,
    /// not the block's first: a sine over one 4-beat cycle, frame by frame,
    /// across blocks.
    #[test]
    fn a_rolling_transport_is_read_frame_by_frame() {
        let rolling = TransportPosition::new(true, Bpm::of(120.0), Beat(0.0), None);
        // 120 BPM at 48 kHz: 24 000 frames a beat, 96 000 a cycle.
        let out = render_at(
            LfoNode::new(LfoShape::Sine).with_beat_sync(BeatsPerCycle::of(4.0)),
            rolling,
            3_000,
        );
        for (i, &v) in out.iter().enumerate() {
            let beat = i as f64 / 24_000.0;
            let want = (core::f64::consts::TAU * beat / 4.0).sin() as f32;
            assert!((v - want).abs() < 1e-3, "frame {i}: {v} vs {want}");
        }
    }

    /// The beat is `f64` in the block's `Env`, so a phase far into a session
    /// keeps its sub-beat detail.
    #[test]
    fn beat_synced_lfo_keeps_sub_beat_precision_at_high_beats() {
        let lfo = || LfoNode::new(LfoShape::Sine).with_beat_sync(BeatsPerCycle::of(4.0));
        // Same fractional offset (0.5 beat) at beat 0 and at beat 20000.
        let at_edge = render_at(lfo(), stopped_at(0.5), 1)[0];
        let past_edge = render_at(lfo(), stopped_at(20_000.5), 1)[0];

        // 20000 is a multiple of the 4-beat cycle, so both are the same phase.
        assert!(
            (at_edge - past_edge).abs() < 0.01,
            "sub-beat precision lost at high beat count: {at_edge} vs {past_edge}"
        );
    }

    #[test]
    fn test_depth_control() {
        let lfo = LfoNode::new(LfoShape::Square).with_depth(Depth::of(0.5));
        let out = render(lfo, 48_000.0, 1);
        assert!((out[0] - 0.5).abs() < 0.01);
    }

    /// Negative depth inverts the modulator rather than silencing it.
    ///
    /// `Depth` is bipolar: `with_depth(-1.0)` stores `-1.0` and phase-flips the
    /// shape. Clamping to `0.0..=1.0` instead would store `0.0` and take the
    /// LFO flat, which is silent in both senses.
    #[test]
    fn negative_depth_inverts_instead_of_silencing() {
        let first = |depth: f32| {
            let lfo = LfoNode::new(LfoShape::Square).with_depth(Depth::of(depth));
            render(lfo, 48_000.0, 1)[0]
        };
        let (a, b) = (first(1.0), first(-1.0));

        assert!(a.abs() > 0.01, "the reference sample must be audible");
        assert!((b + a).abs() < 1e-6, "expected {b} to invert to {}", -a);
    }

    #[test]
    fn test_phase_offset() {
        let lfo = LfoNode::new(LfoShape::Sine).with_phase_offset(PhaseIncrement::new(0.25));
        let out = render(lfo, 100.0, 1);
        assert!((out[0] - 1.0).abs() < 0.1, "Expected ~1.0, got {}", out[0]);
    }

    #[test]
    fn test_lfo_reset() {
        let mut d = Direct::new(LfoNode::new(LfoShape::Sine), SampleRate::of(100.0), 50);
        d.block();
        Node::reset(&mut *d.node);
        d.block();
        assert!(
            d.output(0)[0].abs() < 0.1,
            "After reset, LFO should start near zero"
        );
    }

    fn random(seed: u64) -> Waveform {
        Waveform::Shape {
            shape: LfoShape::Random,
            seed,
        }
    }

    /// **Seek.** A beat-synced random LFO's value at a beat is the wave's at
    /// that beat's cycle, whether the transport rolled there from zero or
    /// was placed there: the step is addressed by the cycle index, not
    /// counted from wraps the node happened to see.
    #[test]
    fn a_random_value_is_the_same_after_a_seek() {
        let seed = 11;
        let lfo = || {
            LfoNode::new(LfoShape::Random)
                .with_seed(seed)
                .with_beat_sync(BeatsPerCycle::of(1.0))
        };
        let rolling = TransportPosition::new(true, Bpm::of(120.0), Beat(0.0), None);
        // 7.5 beats at 120 BPM and 48 kHz.
        let rolled = render_at(lfo(), rolling, 180_001);
        let want = random(seed).raw_at(7.5, false);
        assert_eq!(rolled[180_000], want);
        for beat in [7.5, 7.25, 3.5, 1_000.5] {
            let sought = render_at(lfo(), stopped_at(beat), 1)[0];
            assert_eq!(sought, random(seed).raw_at(beat, false), "beat {beat}");
        }
        assert_eq!(render_at(lfo(), stopped_at(7.25), 1)[0], want);
    }

    /// **Fork.** A free-running fork starts at cycle zero and replays the
    /// wave's values from there, as the original did: both read the value
    /// at each frame's cycle, `i / 256` at 187.5 Hz and 48 kHz (an exact
    /// binary step, so the position is exact).
    #[test]
    fn a_random_value_is_the_same_in_a_fork() {
        let rate = SampleRate::SR_48K;
        let mut d = Direct::new(
            LfoNode::new(LfoShape::Random)
                .with_seed(5)
                .with_rate(Hz(187.5)),
            rate,
            4096,
        );
        let check = |out: &[f32], who: &str| {
            for (i, &v) in out.iter().enumerate() {
                assert_eq!(
                    v,
                    random(5).raw_at(i as f64 / 256.0, false),
                    "{who} frame {i}"
                );
            }
        };
        d.block();
        check(d.output(0), "original");
        let mut f = Direct::new(d.node.fork_fresh(), rate, 4096);
        f.block();
        check(f.output(0), "fork");
    }

    /// **Step boundaries.** The step changes exactly where the rate puts a
    /// cycle boundary, and nowhere else: every 256 frames free-running at
    /// 187.5 Hz, and every 12 000 frames beat-synced at half a beat per cycle
    /// (120 BPM, 48 kHz), each step the wave's value for its index. The
    /// smooth wave reaches each step at its boundary.
    #[test]
    fn random_steps_fall_where_the_rate_says() {
        let w = random(0);
        let free = render(
            LfoNode::new(LfoShape::Random).with_rate(Hz(187.5)),
            48_000.0,
            2048,
        );
        for (i, &v) in free.iter().enumerate() {
            assert_eq!(v, w.raw_at((i / 256) as f64, false), "free frame {i}");
        }
        let rolling = TransportPosition::new(true, Bpm::of(120.0), Beat(0.0), None);
        let synced = render_at(
            LfoNode::new(LfoShape::Random).with_beat_sync(BeatsPerCycle::of(0.5)),
            rolling,
            60_000,
        );
        for (i, &v) in synced.iter().enumerate() {
            assert_eq!(v, w.raw_at((i / 12_000) as f64, false), "synced frame {i}");
        }
        // Consecutive steps differ here, so each boundary is visible.
        for k in 1..5 {
            assert_ne!(synced[k * 12_000 - 1], synced[k * 12_000], "boundary {k}");
        }
        let smooth = render(
            LfoNode::new(LfoShape::RandomSmooth).with_rate(Hz(187.5)),
            48_000.0,
            2048,
        );
        for k in 1..8 {
            assert!((smooth[k * 256] - w.raw_at(k as f64 - 1.0, false)).abs() < 1e-6);
        }
    }

    /// The node built from a control-rate [`tutti_modulation::Lfo`] evaluates the
    /// same wave: seed, phase and rate carried over. Beat-synced, on the same
    /// clock (the beat); free-running, at the same rate on the node's
    /// own clock, which counts from its insert (frame 0 here).
    #[test]
    fn a_node_from_a_control_lfo_reads_its_wave() {
        let lfo = tutti_modulation::Lfo::new(LfoShape::RandomSmooth)
            .seed(3)
            .rate(BeatsPerCycle::of(2.0))
            .phase(PhaseIncrement(0.25));
        let wave = Waveform::Shape {
            shape: LfoShape::RandomSmooth,
            seed: 3,
        };
        for beat in [0.0, 1.5, 9.0, 33.75] {
            let got = render_at(LfoNode::from(lfo), stopped_at(beat), 1)[0];
            let want = wave.raw_at(beat / 2.0 + 0.25, false);
            assert!((got - want).abs() < 1e-6, "beat {beat}: {got} vs {want}");
        }
        // 187.5 Hz at 48 kHz: 256 frames a cycle.
        let free = render(LfoNode::from(lfo.rate(Hz(187.5))), 48_000.0, 2048);
        for i in [0, 100, 256, 777, 2047] {
            let want = wave.raw_at(i as f64 / 256.0 + 0.25, false);
            let got = free[i];
            assert!((got - want).abs() < 1e-6, "frame {i}: {got} vs {want}");
        }
    }

    #[test]
    fn test_random_produces_different_values() {
        let values = render(LfoNode::new(LfoShape::Random), 100.0, 500);

        let unique: std::collections::HashSet<u32> =
            values.iter().map(|v| (v * 1000.0) as u32).collect();

        assert!(
            unique.len() > 1,
            "Random LFO should produce different values, got {}",
            unique.len()
        );
    }
}
