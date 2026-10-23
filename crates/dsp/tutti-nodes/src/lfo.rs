//! The audio-rate modulation adapter.
//!
//! [`ModulatorNode<M>`] is the graph adapter over a pure [`Modulator`]: it
//! owns everything *audio* — the `tutti_graph::Node` impl, the clock
//! position, the sample rate, the beat read from each block's `Env` — and
//! calls the modulator only for the one pure step `cycles -> value`. The
//! modulator itself knows nothing of transport or audio.
//!
//! [`LfoNode`] is `ModulatorNode<Wave>` — the concrete, monomorphized LFO node
//! the graph builds, over the same [`Wave`] a control lane evaluates, so a
//! beat-synced node and lane produce one sequence (a random wave included).
//! A free-running node runs from its insert and does not restart at play.
//! Because `M` is a concrete type param (not `Box<dyn>`), `value()` inlines.
//!
//! `LfoShape` and `Wave` are re-exported so `use tutti_nodes::LfoShape`
//! resolves here.

use tutti_core::Arc;
use tutti_core::AtomicF32;

use tutti_core::{BeatDuration, ChannelLayout, Depth, Hz, Param, PhaseIncrement, SampleRate};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::{Latency, Tail, UnitParam};

// The waveform vocabulary is tutti-mod's floor, re-exported so `use
// tutti_nodes::LfoShape` resolves.
pub use crate::modulator::Modulator;
pub use tutti_mod::{LfoShape, Wave};

/// Where `beat` falls on a clock `beats_per_cycle` beats a cycle, in unwrapped
/// cycles: the cycle index a stepped shape addresses, and the phase.
///
/// [`Beat::cycles_of`] owns the division, in `f64`, so the phase keeps its
/// sub-beat detail however far along the transport is. A non-positive span
/// freezes at cycle zero.
///
/// [`Beat::cycles_of`]: tutti_core::Beat::cycles_of
#[inline]
fn beat_cycles(beat: tutti_core::Beat, beats_per_cycle: BeatDuration) -> f64 {
    beat.cycles_of(beats_per_cycle).unwrap_or(0.0)
}

/// Whether the adapter derives phase from a free-running oscillator or from the
/// transport beat. This is an *adapter* concern (transport wiring), not
/// modulation math — hence it lives here, not in `tutti-mod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LfoMode {
    /// Phase advances from the node's own sample clock at a rate in [`Hz`],
    /// independent of the transport. The node takes no inputs, so it needs no
    /// graph edge, and it keeps running while the transport is stopped.
    FreeRunning,
    /// Phase is derived from the transport beat of each frame, read from the
    /// block's `Env` ([`tutti_graph::Env::for_each_beat`]), at a rate in
    /// [`BeatDuration`] beats per cycle. Sample-accurate and identical
    /// offline, and, like free-running, with no graph edge: the node takes no
    /// inputs in either mode.
    BeatSynced,
}

impl LfoMode {
    /// Returns the human-readable mode name (`"Free Running"` /
    /// `"Beat Synced"`) for a UI mode selector.
    ///
    /// This is display text, not a stable identifier — do not parse or persist
    /// it.
    pub fn name(&self) -> &'static str {
        match self {
            Self::FreeRunning => "Free Running",
            Self::BeatSynced => "Beat Synced",
        }
    }
}

impl core::fmt::Display for LfoMode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
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
/// random wave steps on the cycle index ([`Wave::raw_at`]): beat-synced, it
/// replays after a seek; free-running, a fork (which starts at cycle zero)
/// replays the original's sequence from its start.
///
/// # In a graph
///
/// A graph node ([`IntoNode`]): inserted, its controls are a [`ParamSet`]
/// over the depth ([`UnitParam::Depth`]) and, free-running, the rate
/// ([`UnitParam::Rate`], in [`Hz`]); a beat-synced node's rate cell holds a
/// span in beats, which is not a `Rate`, so it has no address there (set it
/// with [`set_beats_per_cycle`](Self::set_beats_per_cycle)). A fork starts
/// from the values last set through the set and every other cell at its value
/// when forked, at phase zero. It is a generator ([`Tail::Unbounded`]), so
/// the executor never skips it.
///
/// **Arrival.** It has no inputs, so its compiled arrival is zero by
/// construction and the beat it reads is its block's own; a consumer behind a
/// latent path is aligned by the compiler, which delays this node's edge into
/// it like any other source's.
pub struct ModulatorNode<M: Modulator> {
    modulator: M,
    mode: LfoMode,
    /// `FreeRunning`: oscillator frequency in Hz. `BeatSynced`: beats per
    /// cycle, a span — the same atomic, read through `mode`.
    ///
    /// The pun is here because this cell is exposed as a raw `AtomicF32` for
    /// audio-rate modulation, and `Param` is f32-only. Everything that writes
    /// it either sets the mode (`with_*`) or refuses when the mode disagrees
    /// (`set_*`), so no caller can put a frequency in the span reading.
    frequency: Param<Hz>,
    depth: Param<Depth>,
    phase_offset: Param<PhaseIncrement>,
    /// Free-running clock position, in unwrapped cycles (`f64`, so the cycle
    /// index a random wave steps on stays exact for as long as a session
    /// runs; no unit type covers an unwrapped position).
    position: f64,
    sample_rate: SampleRate,
}

/// The concrete LFO node the graph builds — a [`ModulatorNode`] driving a
/// [`Wave`], the one a control lane evaluates. Monomorphized, so `value()`
/// inlines.
///
/// **The per-sample tier.** Reach for this when a control-rate value is too
/// coarse (audio-rate modulation): an audio generator whose output feeds a
/// node's own audio input (doc 014 §11). In `BeatSynced` mode it reads the
/// beat of every frame from its block's `Env`, so it is sample-accurate and
/// renders identically offline.
///
/// The control-rate alternative is a `tutti_mod::Source::Lfo` route, which a
/// control lane evaluates into the param's port (doc 014 §5).
pub type LfoNode = ModulatorNode<Wave>;

impl ModulatorNode<Wave> {
    /// Creates a free-running LFO of `shape` (seed 0) with default frequency
    /// 1.0 Hz.
    ///
    /// Chain `.with_frequency(hz)` or `.with_beat_sync(beats)` to configure
    /// further. Its phase increment takes the rate [`Node::prepare`] hands
    /// it.
    pub fn new(shape: LfoShape) -> Self {
        Self::with_modulator(Wave::Shape { shape, seed: 0 })
    }

    /// The random shapes' seed: seed 0 is the plain sequence, the one a
    /// [`tutti_mod::Lfo`] of the same seed produces in a control lane.
    /// Ignored by a wave that is not a shape.
    pub fn with_seed(mut self, seed: u64) -> Self {
        if let Wave::Shape { seed: s, .. } = &mut self.modulator {
            *s = seed;
        }
        self
    }
}

/// The node for a control-rate LFO's declaration: the same shape, seed,
/// rate and phase. Beat-synced, it produces the values a control lane
/// evaluates from that [`tutti_mod::Lfo`], at audio rate. Free-running
/// (`Hz`), it runs at that frequency from its insert and does not restart
/// at play, where the lane's `Hz` clock is the graph's play clock.
impl From<tutti_mod::Lfo> for LfoNode {
    fn from(lfo: tutti_mod::Lfo) -> Self {
        let node = Self::new(lfo.shape)
            .with_seed(lfo.seed)
            .with_phase_offset(lfo.phase);
        match lfo.rate {
            tutti_mod::LfoRate::Beats(per_cycle) => node.with_beat_sync(per_cycle),
            tutti_mod::LfoRate::Hz(rate) => node.with_frequency(rate),
        }
    }
}

impl<M: Modulator> ModulatorNode<M> {
    /// Builds a modulation node over an arbitrary pure modulator, free-running
    /// at 1 Hz. The generic entry point behind [`LfoNode::new`]; also the seam
    /// any future modulator (envelope, sample & hold, …) wires through.
    ///
    /// Takes no *modulation* rate: the shared frequency cell reads as Hz or as
    /// beats depending on the mode, so that rate is set by the builder which
    /// also selects the clock — [`with_frequency`](Self::with_frequency) or
    /// [`with_beat_sync`](Self::with_beat_sync).
    ///
    /// It takes no *sample* rate either: the graph hands it the device rate
    /// in [`Node::prepare`] before its first block. The two are separate
    /// quantities that meet in one place — the per-sample phase increment is
    /// the modulation rate divided by the sample rate.
    pub fn with_modulator(modulator: M) -> Self {
        Self {
            modulator,
            mode: LfoMode::FreeRunning,
            frequency: Param::new(Hz(1.0)),
            depth: Param::new(Depth::FULL),
            phase_offset: Param::new(PhaseIncrement(0.0)),
            position: 0.0,
            sample_rate: SampleRate::DEFAULT,
        }
    }

    /// Free-running at `hz` cycles per second, switching to
    /// [`LfoMode::FreeRunning`].
    ///
    /// **The mode switch is the point.** Both clocks share one cell, so taking
    /// an `Hz` without selecting the clock would write a frequency into the
    /// span reading with the type system agreeing —
    /// `with_beat_sync(BeatDuration(4.0)).with_frequency(Hz(2.0))` would give a
    /// 2-*beat* cycle, not 2 Hz. Setting a rate in one clock's unit selects
    /// that clock, which is the only reading under which both calls mean what
    /// they say.
    pub fn with_frequency(mut self, hz: impl Into<Hz>) -> Self {
        self.mode = LfoMode::FreeRunning;
        self.frequency.store(hz.into());
        self
    }

    /// Switch to beat-synced mode: the phase follows the transport beat of
    /// every frame, read from the block's `Env`. Per-sample accurate, and
    /// nothing to wire.
    ///
    /// Takes a [`BeatDuration`] — a span, so a larger value is *slower*. The
    /// backing cell is a `Param<Hz>` because it is exposed as a raw `AtomicF32`
    /// for audio-rate modulation ([`frequency`](Self::frequency)); the span is
    /// stored in it and read back as a `BeatDuration`. The mode decides which
    /// of the two readings applies, so every entry point that writes the cell
    /// has to set it — see [`with_frequency`](Self::with_frequency).
    pub fn with_beat_sync(mut self, beats_per_cycle: impl Into<BeatDuration>) -> Self {
        self.mode = LfoMode::BeatSynced;
        self.frequency
            .store(Hz(beats_per_cycle.into().get() as f32));
        self
    }

    /// The stored beat-synced span. Only meaningful in [`LfoMode::BeatSynced`].
    #[inline]
    fn beats_per_cycle(&self) -> BeatDuration {
        BeatDuration(f64::from(self.frequency.load().get()))
    }

    /// Sets the modulation [`Depth`], clamped to `-1.0..=1.0`.
    ///
    /// Bipolar: a negative depth inverts the modulator, so `-1.0` is the same
    /// shape phase-flipped and `0.0` is flat. Values past full scale saturate
    /// rather than wrapping.
    pub fn with_depth(self, depth: impl Into<Depth>) -> Self {
        self.depth.store(Depth::new_clamped(depth.into().get()));
        self
    }

    /// Sets the [`PhaseIncrement`] offset, conventionally `0.0` to `1.0` for one
    /// full cycle.
    ///
    /// Stored as given and added to the unwrapped clock position, so any real
    /// value is valid: the wave takes the phase as the position's
    /// `floor`-based fraction, so a negative offset lags rather than reading
    /// off the front of the shape. An offset past a whole cycle also moves a
    /// random wave to a later step.
    pub fn with_phase_offset(self, offset: impl Into<PhaseIncrement>) -> Self {
        self.phase_offset.store(offset.into());
        self
    }

    /// The shared rate cell, for wiring an audio-rate modulator onto this LFO's
    /// own rate.
    ///
    /// **The reading depends on the mode**, because both clocks share this one
    /// cell: [`Hz`] in [`LfoMode::FreeRunning`], beats per cycle in
    /// [`LfoMode::BeatSynced`]. A writer that ignores the mode sets a rate in
    /// the wrong clock's unit, and nothing refuses it — the typed setters
    /// [`set_frequency`](Self::set_frequency) and
    /// [`set_beats_per_cycle`](Self::set_beats_per_cycle) exist precisely
    /// because they check.
    ///
    /// The handle is shared across clones, so a write reaches the live node.
    pub fn frequency(&self) -> Arc<AtomicF32> {
        self.frequency.as_atomic()
    }

    /// The shared [`Depth`] cell, for modulating modulation depth.
    ///
    /// Bipolar, `-1.0..=1.0`: a negative depth inverts the shape rather than
    /// silencing it, and `0.0` is flat. Unlike
    /// [`set_depth`](Self::set_depth) this writes the raw cell, so out-of-range
    /// values are **not** clamped here — the shape scales past full scale.
    ///
    /// The handle is shared across clones, so a write reaches the live node.
    pub fn depth(&self) -> Arc<AtomicF32> {
        self.depth.as_atomic()
    }

    /// The shared [`PhaseIncrement`] offset cell, for phase-modulating the LFO.
    ///
    /// Read per sample and added to the running phase; the wrap happens at
    /// application, so any real value is valid and a negative offset lags
    /// rather than reading off the front of the shape.
    ///
    /// The handle is shared across clones, so a write reaches the live node.
    pub fn phase_offset(&self) -> Arc<AtomicF32> {
        self.phase_offset.as_atomic()
    }

    /// Sets the free-running rate. **Ignored in [`LfoMode::BeatSynced`]**, where
    /// the cell holds a span in beats and an `Hz` would be a silent reciprocal
    /// — use [`set_beats_per_cycle`](Self::set_beats_per_cycle).
    ///
    /// `&self`, so unlike [`with_frequency`](Self::with_frequency) this cannot
    /// switch the mode: the node is live in the graph. Refusing is the only
    /// remaining option that does not corrupt the other clock's value.
    pub fn set_frequency(&self, freq: impl Into<Hz>) {
        if self.mode == LfoMode::FreeRunning {
            self.frequency.store(freq.into());
        }
    }

    /// Sets the beat-synced span. **Ignored in [`LfoMode::FreeRunning`]** — the
    /// mirror of [`set_frequency`](Self::set_frequency).
    pub fn set_beats_per_cycle(&self, beats_per_cycle: impl Into<BeatDuration>) {
        if self.mode == LfoMode::BeatSynced {
            self.frequency
                .store(Hz(beats_per_cycle.into().get() as f32));
        }
    }

    /// Sets the modulation [`Depth`], clamped to `-1.0..=1.0`.
    ///
    /// Bipolar: `1.0` is the shape at full scale, `0.0` flat, `-1.0` the same
    /// shape phase-flipped. Read once per sample, so a write lands on the next
    /// sample of the block in flight.
    ///
    /// `&self`, and the cell is shared across clones, so this reaches a node
    /// already live in the graph.
    pub fn set_depth(&self, depth: impl Into<Depth>) {
        self.depth.store(Depth::new_clamped(depth.into().get()));
    }

    /// Sets the [`PhaseIncrement`] added to the running phase before the shape
    /// is evaluated.
    ///
    /// Stored as given — the wrap to `[0, 1)` happens where the offset is
    /// applied, so any real value is valid and a negative offset lags the
    /// shape. `0.25` on a sine starts at the peak.
    ///
    /// `&self`, and the cell is shared across clones, so this reaches a node
    /// already live in the graph.
    pub fn set_phase_offset(&self, offset: impl Into<PhaseIncrement>) {
        self.phase_offset.store(offset.into());
    }

    /// The one call into the pure modulator, at `cycles` plus the phase
    /// offset, with the depth applied.
    #[inline]
    fn evaluate(&self, cycles: f64, offset: PhaseIncrement, depth: f32) -> f32 {
        self.modulator.value(cycles + f64::from(offset.get())) * depth
    }
}

/// The params a [`ModulatorNode`] lets the graph modulate, in port order:
/// its depth and, while free-running, its rate (a beat-synced node declares
/// only the depth, as its [`ParamSet`] addresses only the depth). Both are
/// read once per block, so a driven port is read on the block's last frame.
/// The mode is fixed once the node is live (`with_*` only), so its shape
/// never changes.
pub const LFO_PARAMS: [UnitParam; 2] = [UnitParam::Depth, UnitParam::Rate];

impl<M: Modulator + Send + 'static> Node for ModulatorNode<M> {
    fn shape(&self) -> Shape {
        let ports = match self.mode {
            LfoMode::FreeRunning => 2,
            LfoMode::BeatSynced => 1,
        };
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
            .with_cost(tutti_graph::Cost::Moderate)
            .with_tail(Tail::Unbounded)
            .with_params(&LFO_PARAMS[..ports])
    }

    fn prepare(&mut self, p: &Prepare) {
        self.sample_rate = p.sample_rate();
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        debug_assert_eq!(
            cx.arrival,
            Latency::ZERO,
            "a node with no inputs arrives at zero"
        );
        let size = io.frames();
        // A driven depth is clamped as `set_depth` clamps a written one. A
        // non-finite driven value reads the cell instead: a NaN rate would
        // leave the phase NaN for good.
        let finite = |v: &f32| v.is_finite();
        let depth = crate::ramp::block_driven(&io, 0)
            .filter(finite)
            .map_or_else(|| self.depth.load(), Depth::new_clamped)
            .get();
        let driven_rate = match self.mode {
            LfoMode::FreeRunning => crate::ramp::block_driven(&io, 1).filter(finite),
            LfoMode::BeatSynced => None,
        };
        let out = io.output(0);
        let phase_offset = self.phase_offset.load();

        match self.mode {
            LfoMode::FreeRunning => {
                let freq = driven_rate.unwrap_or_else(|| self.frequency.load().get());
                // In f64, not `PhaseIncrement::per_sample` (f32): the step
                // is accumulated for as long as the node runs, and an
                // f32-rounded step drifts the cycle index off the frame.
                let step = f64::from(freq) / self.sample_rate.get();

                for o in &mut out[..size] {
                    *o = self.evaluate(self.position, phase_offset, depth);
                    self.position += step;
                }
            }
            LfoMode::BeatSynced => {
                // Read once per block, not per sample.
                let beats_per_cycle = self.beats_per_cycle();
                cx.env.for_each_beat(|i, beat| {
                    // A non-positive span freezes at the offset — the guard
                    // lives in `Beat::cycles_of`, which `beat_cycles` goes
                    // through.
                    let cycles = beat_cycles(beat, beats_per_cycle);
                    out[i] = self.evaluate(cycles, phase_offset, depth);
                });
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.position = 0.0;
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        match (k, self.mode) {
            (0, _) => Some(self.depth.load().get()),
            (1, LfoMode::FreeRunning) => Some(self.frequency.load().get()),
            _ => None,
        }
    }
}

impl<M: Modulator + Clone + Send + 'static> ParamNode for ModulatorNode<M> {
    /// The depth, and the rate while free-running: a beat-synced node's rate
    /// cell holds beats per cycle, which a `Rate` (in `Hz`) written by
    /// address would silently reinterpret — the pun
    /// [`set_frequency`](Self::set_frequency) refuses too.
    fn param_set(&self) -> ParamSet {
        let set = ParamSet::builder().param(UnitParam::Depth, self.depth.as_atomic());
        match self.mode {
            LfoMode::FreeRunning => set.param(UnitParam::Rate, self.frequency.as_atomic()),
            LfoMode::BeatSynced => set,
        }
        .build()
    }

    /// A clone with every control cell detached (at its value now), at cycle
    /// zero: a free-running fork replays the original's values from its
    /// start.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.frequency.detach();
        fork.depth.detach();
        fork.phase_offset.detach();
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
            mode: self.mode,
            frequency: self.frequency.handle(),
            depth: self.depth.handle(),
            phase_offset: self.phase_offset.handle(),
            position: self.position,
            sample_rate: self.sample_rate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::{Beat, Bpm, Samples};
    use tutti_graph::contract::{assert_param_fork, Direct};
    use tutti_graph::{Solo, Transport};

    /// **The ports and their bases.** A free-running LFO declares its depth
    /// and rate ([`LFO_PARAMS`]), each port's base its cell; a beat-synced
    /// one declares only its depth, as its `ParamSet` addresses only that
    /// (its rate cell holds beats per cycle, not `Hz`).
    ///
    /// Mutation (run): `param_base(1)` answering in either mode (the
    /// mode guard dropped) → the beat-synced node reports a rate base →
    /// fails. Mutation (run): `param_base(0)` answering the rate →
    /// fails.
    #[test]
    fn the_shape_declares_depth_and_a_free_running_rate() {
        let free = LfoNode::new(LfoShape::Sine)
            .with_frequency(Hz(2.0))
            .with_depth(Depth(0.25));
        assert_eq!(free.shape().params.as_slice(), &LFO_PARAMS[..]);
        assert_eq!(free.param_base(0), Some(0.25));
        assert_eq!(free.param_base(1), Some(2.0));
        let synced = LfoNode::new(LfoShape::Sine)
            .with_beat_sync(BeatDuration(4.0))
            .with_depth(Depth(0.5));
        assert_eq!(synced.shape().params.as_slice(), &[UnitParam::Depth][..]);
        assert_eq!(synced.param_base(0), Some(0.5));
        assert_eq!(synced.param_base(1), None);
    }

    // The waveform math is `tutti-mod`'s, and so are the tests that pin it.
    // What belongs here is the adapter around it: phase generation from
    // transport, depth, and the node surface.

    /// `n` frames of `node` at `sr` in one block, transport stopped at beat 0.
    fn render(node: LfoNode, sr: f64, n: usize) -> Vec<f32> {
        let mut d = Direct::new(node, SampleRate(sr), n);
        d.block();
        d.output(0).to_vec()
    }

    /// `n` frames of `node` through a graph at 48 kHz, each block handed
    /// `transport` as its own (advancing, when it rolls, by the block).
    fn render_at(node: LfoNode, transport: Transport, n: usize) -> Vec<f32> {
        let rate = SampleRate(48_000.0);
        let mut solo = Solo::new(node, Prepare::new(rate, Samples(256)));
        solo.renderer_mut().set_transport_fn(move |frame| {
            let mut clock = tutti_core::FrameClock::new(transport.beat(), transport.tempo, rate);
            if transport.playing {
                clock.advance(Samples(frame.get() as usize), None);
            }
            Transport::counted(transport.playing, transport.tempo, clock.origin(), None)
        });
        solo.render(n).remove(0)
    }

    fn stopped_at(beat: f64) -> Transport {
        Transport::new(false, Bpm(120.0), Beat(beat), None)
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

    /// Setting a rate in one clock's unit selects that clock.
    ///
    /// Both clocks share one cell, so a `with_frequency` that left the mode
    /// alone would write an `Hz` into the beat-synced reading: this sequence
    /// would produce a 2-beat cycle where its author asked for 2 Hz.
    #[test]
    fn setting_a_free_rate_leaves_beat_sync_behind() {
        let lfo = LfoNode::new(LfoShape::Sine)
            .with_beat_sync(BeatDuration(4.0))
            .with_frequency(Hz(2.0));
        assert_eq!(lfo.mode, LfoMode::FreeRunning);
        assert_eq!(
            lfo.shape().audio_in.count(),
            0,
            "a free-running LFO has no inputs"
        );
        assert_eq!(lfo.frequency.load(), Hz(2.0));

        // And the other direction.
        let lfo = LfoNode::new(LfoShape::Sine)
            .with_frequency(Hz(2.0))
            .with_beat_sync(BeatDuration(4.0));
        assert_eq!(lfo.mode, LfoMode::BeatSynced);
        assert_eq!(lfo.beats_per_cycle(), BeatDuration(4.0));
    }

    /// The live setters take `&self` and so cannot switch modes. Writing the
    /// wrong one is refused rather than silently reinterpreted.
    #[test]
    fn a_live_setter_for_the_other_clock_is_ignored() {
        let synced = LfoNode::new(LfoShape::Sine).with_beat_sync(BeatDuration(4.0));
        synced.set_frequency(Hz(2.0));
        assert_eq!(
            synced.beats_per_cycle(),
            BeatDuration(4.0),
            "an Hz must not land in the span cell"
        );
        synced.set_beats_per_cycle(BeatDuration(8.0));
        assert_eq!(synced.beats_per_cycle(), BeatDuration(8.0));

        let free = LfoNode::new(LfoShape::Sine).with_frequency(Hz(2.0));
        free.set_beats_per_cycle(BeatDuration(4.0));
        assert_eq!(
            free.frequency.load(),
            Hz(2.0),
            "a span must not land in the frequency cell"
        );
        free.set_frequency(Hz(5.0));
        assert_eq!(free.frequency.load(), Hz(5.0));
    }

    /// A fork starts from the values last set through the node's
    /// `ParamSet` and shares no cell with it (see
    /// `tutti_graph::contract::assert_param_fork`). The rate is addressed
    /// only while free-running: a beat-synced cell holds beats, not `Hz`.
    ///
    /// Mutation (run): drop `fork.depth.detach()` in `fork_fresh` → "a live
    /// write reached the fork" for `Depth`. Address `Rate` in both modes →
    /// the beat-synced list fails.
    #[test]
    fn a_fork_starts_from_the_authored_values_and_shares_nothing() {
        let free = LfoNode::new(LfoShape::Sine).with_frequency(Hz(2.0));
        assert_eq!(
            free.param_set().params().collect::<Vec<_>>(),
            [UnitParam::Depth, UnitParam::Rate]
        );
        assert_param_fork(free);
        let synced = LfoNode::new(LfoShape::Sine).with_beat_sync(BeatDuration(4.0));
        assert_eq!(
            synced.param_set().params().collect::<Vec<_>>(),
            [UnitParam::Depth]
        );
        assert_param_fork(synced);
    }

    /// No inputs, one output, never skipped: a generator's tail is
    /// unbounded, or the executor would stop calling it once its (absent)
    /// inputs were silent.
    ///
    /// Mutation (run): `Shape::audio(..)` without `with_tail(Unbounded)`
    /// (the default `Tail::None`) → fails.
    #[test]
    fn a_generator_with_no_inputs_is_never_skipped() {
        for lfo in [
            LfoNode::new(LfoShape::Sine),
            LfoNode::new(LfoShape::Sine).with_beat_sync(1.0),
        ] {
            let shape = lfo.shape();
            assert_eq!(shape.audio_in.count(), 0);
            assert_eq!(shape.audio_out.count(), 1);
            assert_eq!(shape.tail, Tail::Unbounded);
        }
    }

    #[test]
    fn test_beat_synced_lfo() {
        let lfo = || LfoNode::new(LfoShape::Sine).with_beat_sync(4.0);
        // Beat 1.0 of a 4-beat cycle = quarter phase = sine peak.
        let out = render_at(lfo(), stopped_at(1.0), 1);
        assert!((out[0] - 1.0).abs() < 0.01, "Expected 1.0, got {}", out[0]);
        let out = render_at(lfo(), stopped_at(2.0), 1);
        assert!((out[0] - 0.0).abs() < 0.01, "Expected 0.0, got {}", out[0]);
    }

    /// While the transport rolls, each frame is evaluated at its own beat,
    /// not the block's first: a sine over one 4-beat cycle, frame by frame,
    /// across blocks.
    ///
    /// Mutation (run): evaluate every frame at `cx.env.transport.beat()`
    /// (the block's first beat) → the frames inside a block hold → fails.
    #[test]
    fn a_rolling_transport_is_read_frame_by_frame() {
        let rolling = Transport::new(true, Bpm(120.0), Beat(0.0), None);
        // 120 BPM at 48 kHz: 24 000 frames a beat, 96 000 a cycle.
        let out = render_at(
            LfoNode::new(LfoShape::Sine).with_beat_sync(4.0),
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
        let lfo = || LfoNode::new(LfoShape::Sine).with_beat_sync(4.0);
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
        let lfo = LfoNode::new(LfoShape::Square);
        lfo.set_depth(0.5);
        let out = render(lfo, 48_000.0, 1);
        assert!((out[0] - 0.5).abs() < 0.01);
    }

    /// Negative depth inverts the modulator rather than silencing it.
    ///
    /// `Depth` is bipolar: `set_depth(-1.0)` stores `-1.0` and phase-flips the
    /// shape. Clamping to `0.0..=1.0` instead would store `0.0` and take the
    /// LFO flat, which is silent in both senses.
    #[test]
    fn negative_depth_inverts_instead_of_silencing() {
        let first = |depth: f32| {
            let lfo = LfoNode::new(LfoShape::Square);
            lfo.set_depth(depth);
            render(lfo, 48_000.0, 1)[0]
        };
        let (a, b) = (first(1.0), first(-1.0));

        assert!(a.abs() > 0.01, "the reference sample must be audible");
        assert!((b + a).abs() < 1e-6, "expected {b} to invert to {}", -a);

        // And the range still saturates past full scale.
        let c = first(-5.0);
        assert!((c - b).abs() < 1e-6);
    }

    #[test]
    fn test_phase_offset() {
        let lfo = LfoNode::new(LfoShape::Sine);
        lfo.set_phase_offset(0.25);
        let out = render(lfo, 100.0, 1);
        assert!((out[0] - 1.0).abs() < 0.1, "Expected ~1.0, got {}", out[0]);
    }

    #[test]
    fn test_lfo_reset() {
        let mut d = Direct::new(LfoNode::new(LfoShape::Sine), SampleRate(100.0), 50);
        d.block();
        Node::reset(&mut d.node);
        d.block();
        assert!(
            d.output(0)[0].abs() < 0.1,
            "After reset, LFO should start near zero"
        );
    }

    fn random(seed: u64) -> Wave {
        Wave::Shape {
            shape: LfoShape::Random,
            seed,
        }
    }

    /// **Seek.** A beat-synced random LFO's value at a beat is the wave's at
    /// that beat's cycle, whether the transport rolled there from zero or
    /// was placed there: the step is addressed by the cycle index, not
    /// counted from wraps the node happened to see.
    ///
    /// Mutation (run): evaluate the beat-synced mode at the free-running
    /// `self.position` instead of the beat's cycles → fails. Mutation (run):
    /// `beat_cycles` wrapped to `[0, 1)` (`rem_euclid(1.0)`, the old phase)
    /// → every cycle reads step 0 → fails.
    #[test]
    fn a_random_value_is_the_same_after_a_seek() {
        let seed = 11;
        let lfo = || {
            LfoNode::new(LfoShape::Random)
                .with_seed(seed)
                .with_beat_sync(1.0)
        };
        let rolling = Transport::new(true, Bpm(120.0), Beat(0.0), None);
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
    ///
    /// Mutation (run): `reset` leaving `position` alone → the fork resumes
    /// at the original's cycle 16 → fails. Mutation (run): `with_seed`
    /// ignored → the values are seed 0's → fails.
    #[test]
    fn a_random_value_is_the_same_in_a_fork() {
        let rate = SampleRate(48_000.0);
        let mut d = Direct::new(
            LfoNode::new(LfoShape::Random)
                .with_seed(5)
                .with_frequency(Hz(187.5)),
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
    ///
    /// Mutation (run): the beat-synced span read as a rate (`beat * span`
    /// instead of `beat / span`, in `Beat::cycles_of`) → steps every 48 000
    /// frames → fails. Mutation (run): the free-running step doubled → steps
    /// every 128 frames → fails.
    #[test]
    fn random_steps_fall_where_the_rate_says() {
        let w = random(0);
        let free = render(
            LfoNode::new(LfoShape::Random).with_frequency(Hz(187.5)),
            48_000.0,
            2048,
        );
        for (i, &v) in free.iter().enumerate() {
            assert_eq!(v, w.raw_at((i / 256) as f64, false), "free frame {i}");
        }
        let rolling = Transport::new(true, Bpm(120.0), Beat(0.0), None);
        let synced = render_at(
            LfoNode::new(LfoShape::Random).with_beat_sync(0.5),
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
            LfoNode::new(LfoShape::RandomSmooth).with_frequency(Hz(187.5)),
            48_000.0,
            2048,
        );
        for k in 1..8 {
            assert!((smooth[k * 256] - w.raw_at(k as f64 - 1.0, false)).abs() < 1e-6);
        }
    }

    /// The node built from a control-rate [`tutti_mod::Lfo`] evaluates the
    /// same wave: seed, phase and rate carried over. Beat-synced, on the same
    /// clock (the beat); free-running, at the same frequency on the node's
    /// own clock, which counts from its insert (frame 0 here).
    ///
    /// Mutation (run): `From` dropping `with_phase_offset` → reads the wave
    /// a quarter cycle early → fails. Dropping `with_seed` → fails.
    /// Dropping `with_frequency` in the `Hz` arm → runs at the default 1 Hz
    /// → the free-running case fails.
    #[test]
    fn a_node_from_a_control_lfo_reads_its_wave() {
        let lfo = tutti_mod::Lfo::new(LfoShape::RandomSmooth)
            .seed(3)
            .rate(BeatDuration(2.0))
            .phase(PhaseIncrement(0.25));
        let wave = Wave::Shape {
            shape: LfoShape::RandomSmooth,
            seed: 3,
        };
        for beat in [0.0, 1.5, 9.0, 33.75] {
            let got = render_at(LfoNode::from(lfo), stopped_at(beat), 1)[0];
            let want = wave.raw_at(beat / 2.0 + 0.25, false);
            assert!((got - want).abs() < 1e-6, "beat {beat}: {got} vs {want}");
        }
        // 187.5 Hz at 48 kHz: 256 frames a cycle.
        let free = render(LfoNode::from(lfo.hz(Hz(187.5))), 48_000.0, 2048);
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
