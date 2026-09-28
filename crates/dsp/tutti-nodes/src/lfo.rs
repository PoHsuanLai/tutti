//! The audio-rate modulation adapter.
//!
//! [`ModulatorNode<M>`] is the graph adapter over a pure
//! [`tutti_mod::Modulator`]: it owns everything *audio* — the
//! `tutti_graph::Node` impl, the phase accumulator, the sample rate, the beat
//! read from each block's `Env` — and calls the modulator only for the one pure
//! step `phase -> value`. The modulator itself (`tutti_mod::Lfo`, `SampleHold`,
//! …) knows nothing of transport or audio.
//!
//! [`LfoNode`] is `ModulatorNode<Lfo>` — the concrete, monomorphized LFO node
//! the graph builds. Because `M` is a concrete type param (not `Box<dyn>`),
//! `value()` inlines, so the adapter is RT-cost-free.
//!
//! The waveform math (`LfoShape::evaluate_periodic`), the random stepper
//! (`RandomState`), and the `Lfo` modulator live in `tutti-mod`; this file is
//! purely the adapter. `LfoShape` is re-exported so `use
//! tutti_nodes::LfoShape` resolves here.

use tutti_core::Arc;
use tutti_core::AtomicF32;

use tutti_core::{
    BeatDuration, ChannelLayout, Depth, Hz, Param, Phase, PhaseIncrement, SampleRate,
};
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::{Latency, Tail, UnitParam};

// The waveform vocabulary + the pure LFO modulator live in tutti-mod now. Re-
// exported so existing `use tutti_nodes::LfoShape` / `Lfo` sites are untouched.
pub use tutti_mod::{Lfo, LfoShape, Modulator};

/// Where `beat` falls within a cycle `beats_per_cycle` beats long.
///
/// Wraps [`Beat::cycles_of`], which owns the division; this adds only the wrap
/// to a single cycle, which is what a phase is. Narrowing after that wrap keeps
/// the result inside `[0, 1)`, where `f32` has resolution to spare no matter
/// how far along the transport is.
#[inline]
fn beat_phase(beat: tutti_core::Beat, beats_per_cycle: BeatDuration) -> Phase {
    beat.cycles_of(beats_per_cycle)
        .map_or(Phase::START, |c| Phase::wrapped(c.rem_euclid(1.0) as f32))
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
/// no inputs, one output) that computes a phase — free-running, or from the
/// transport beat of its block's `Env` — and drives a pure [`Modulator`] `M`.
///
/// Owns everything audio; `M` owns the pure `phase -> value`. `depth` is held
/// here (a live atomic the UI can write) and multiplied onto the modulator's
/// output — the modulator itself stays at unit depth. That keeps the depth
/// setter path unchanged and lets a modulator be shared across backends without
/// carrying a per-node depth.
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
    /// The modulator's threaded state — the node owns it (it has exclusive
    /// access during `process`), threading it through `Modulator::value` each
    /// sample. This is where a stateful modulator's state lives: in the node,
    /// not in the (stateless, `Sync`) modulator.
    mod_state: M::State,
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
    phase: Phase,
    sample_rate: SampleRate,
}

/// The concrete LFO node the graph builds — a [`ModulatorNode`] driving a pure
/// [`Lfo`]. Monomorphized, so `value()` inlines.
///
/// **The per-sample tier.** This is not a different LFO from the one the
/// modulation matrix drives — it is the same [`Lfo`] under a different adapter.
/// Reach for this when a frame-rate scalar is too coarse: in `BeatSynced` mode
/// it reads the beat of every frame from its block's `Env`, so it is
/// sample-accurate and renders identically offline, with no edge to wire.
///
/// The frame-rate alternative is `tutti_mod::ModPreFrame` sampling the same
/// `Lfo` and writing a scalar, which is what `bevy_tutti`'s `ModSource` builds.
/// See tutti-mod's crate docs for the full rate comparison.
pub type LfoNode = ModulatorNode<Lfo>;

impl ModulatorNode<Lfo> {
    /// Creates a free-running LFO with default frequency 1.0 Hz.
    ///
    /// Chain `.with_frequency(hz)` or `.with_beat_sync(beats)` to configure
    /// further. Its phase increment takes the rate [`Node::prepare`] hands
    /// it.
    pub fn new(shape: LfoShape) -> Self {
        Self::with_modulator(Lfo::new(shape))
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
            mod_state: M::State::default(),
            mode: LfoMode::FreeRunning,
            frequency: Param::new(Hz(1.0)),
            depth: Param::new(Depth::FULL),
            phase_offset: Param::new(PhaseIncrement(0.0)),
            phase: Phase::START,
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
    /// Stored as given; the wrap happens where the offset is *applied*, via
    /// `Phase::advance`, so any real value is valid. Wrapping here as well
    /// would be redundant, and a bare `% 1.0` is actively wrong for a negative
    /// offset — it leaves the value negative, which then reads off the front of
    /// the shape table.
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

    /// The one call into the pure modulator: thread the node-owned state through
    /// `value`, store it back, and apply depth. `&mut self` here mutates only
    /// the node's own `mod_state`/params — the modulator stays `&self`.
    #[inline]
    fn evaluate(&mut self, phase: Phase) -> f32 {
        let depth = self.depth.load().get();
        let (next, v) = self.modulator.value(self.mod_state, phase);
        self.mod_state = next;
        v * depth
    }
}

impl<M: Modulator + Send + 'static> Node for ModulatorNode<M> {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
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
        let out = io.output(0);
        let phase_offset = self.phase_offset.load();

        match self.mode {
            LfoMode::FreeRunning => {
                let freq = self.frequency.load().get();
                let phase_increment = PhaseIncrement::per_sample(Hz(freq), self.sample_rate);

                for o in &mut out[..size] {
                    let phase = self.phase.offset_by(phase_offset);
                    *o = self.evaluate(phase);
                    self.phase = self.phase.advance(phase_increment);
                }
            }
            LfoMode::BeatSynced => {
                // Read once per block, not per sample.
                let beats_per_cycle = self.beats_per_cycle();
                cx.env.for_each_beat(|i, beat| {
                    // A non-positive span freezes at the offset — the guard
                    // lives in `Beat::cycles_of`, which `beat_phase` goes
                    // through.
                    let phase = beat_phase(beat, beats_per_cycle).offset_by(phase_offset);
                    out[i] = self.evaluate(phase);
                });
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.phase = Phase::START;
        // The node owns the modulator's threaded state, so it resets it here to
        // the seed — this is what reaches a stateful modulator's RNG.
        // Stateless modulators reset a `()`.
        self.mod_state = M::State::default();
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

    /// A clone with every control cell detached (at its value now), at phase
    /// zero with the modulator's state at its seed.
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
            mod_state: self.mod_state,
            mode: self.mode,
            frequency: self.frequency.handle(),
            depth: self.depth.handle(),
            phase_offset: self.phase_offset.handle(),
            phase: self.phase,
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
