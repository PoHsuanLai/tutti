//! The native audio-rate modulation adapter.
//!
//! [`ModulatorNode<M>`] is the fundsp adapter over a pure
//! [`tutti_mod::Modulator`]: it owns everything *audio* — the `impl AudioUnit`,
//! the phase accumulator, the sample rate, the beat ports, `route`/PDC — and
//! calls the modulator only for the one pure step `phase -> value`. The
//! modulator itself (`tutti_mod::Lfo`, `SampleHold`, …) knows nothing of
//! transport or audio.
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
use tutti_core::{beat_from_ports, AudioUnit, BufferMut, BufferRef, SignalFrame, BEAT_PORTS};

use tutti_core::{BeatDuration, Depth, Hz, Param, Phase, PhaseIncrement, SampleRate};

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
    /// Phase is derived from the transport beat arriving on [`BEAT_PORTS`]
    /// inputs, at a rate in [`BeatDuration`] beats per cycle. Sample-accurate
    /// and identical offline, at the cost of an edge from the transport clock.
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

/// The native audio-rate modulation node: a fundsp `AudioUnit` that computes a
/// phase from transport (free-running or beat-synced) and drives a pure
/// [`Modulator`] `M`.
///
/// Owns everything audio; `M` owns the pure `phase -> value`. `depth` is held
/// here (a live atomic the UI can write) and multiplied onto the modulator's
/// output — the modulator itself stays at unit depth. That keeps the depth
/// setter path unchanged and lets a modulator be shared across backends without
/// carrying a per-node depth.
pub struct ModulatorNode<M: Modulator> {
    modulator: M,
    /// The modulator's threaded state — the node owns it (it has exclusive
    /// access during `process`), threading it through `Modulator::value` each
    /// sample. This is where a stateful modulator's state lives on the native
    /// path: in the node, not in the (stateless, `Sync`) modulator.
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
/// [`Lfo`]. Monomorphized, so `value()` inlines to the old codegen.
///
/// **The per-sample tier.** This is not a different LFO from the one the
/// modulation matrix drives — it is the same [`Lfo`] under a different adapter.
/// Reach for this when a frame-rate scalar is too coarse: in `BeatSynced` mode
/// it reads the beat as a *signal* on its input ports, so it is sample-accurate
/// and renders identically offline. The cost is a graph edge — the node must be
/// wired to the transport clock (`bevy_tutti::EngineNodes::clock` names it).
///
/// The frame-rate alternative is `tutti_mod::ModPreFrame` sampling the same
/// `Lfo` and writing a scalar, which is what `bevy_tutti`'s `ModSource` builds.
/// See tutti-mod's crate docs for the full rate comparison.
pub type LfoNode = ModulatorNode<Lfo>;

impl ModulatorNode<Lfo> {
    /// Create a free-running LFO with default frequency 1.0 Hz.
    ///
    /// Chain `.with_frequency(hz)` or `.with_beat_sync(beats)` to configure
    /// further.
    ///
    /// **Starts at the placeholder [`SampleRate::DEFAULT`]**, as
    /// [`with_modulator`](Self::with_modulator) explains — call
    /// [`AudioUnit::set_sample_rate`] before the first `process` or the LFO
    /// cycles 8.8% slow at 48 kHz.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
    pub fn new(shape: LfoShape) -> Self {
        Self::with_modulator(Lfo::new(shape))
    }
}

impl<M: Modulator> ModulatorNode<M> {
    /// Build a modulation node over an arbitrary pure modulator, free-running
    /// at 1 Hz. The generic entry point behind [`LfoNode::new`]; also the seam
    /// any future modulator (envelope, sample & hold, …) wires through.
    ///
    /// Takes no *modulation* rate: the shared frequency cell reads as Hz or as
    /// beats depending on the mode, so that rate is set by the builder which
    /// also selects the clock — [`with_frequency`](Self::with_frequency) or
    /// [`with_beat_sync`](Self::with_beat_sync).
    ///
    /// It takes no *sample* rate either, and that one is not a choice: the node
    /// **starts at the placeholder [`SampleRate::DEFAULT`]** and must be given
    /// the device rate through [`AudioUnit::set_sample_rate`] before the first
    /// `process`. The two are separate quantities that meet in one place — the
    /// per-sample phase increment is the modulation rate divided by the sample
    /// rate — so a wrong sample rate misreports the modulation rate by the same
    /// ratio. At 48 kHz an uncorrected node runs 8.8% slow: a 2 Hz LFO cycles at
    /// 1.84 Hz, and a beat-synced one drifts against the transport it is
    /// supposed to lock to. See the crate-level "born at a placeholder rate"
    /// section.
    ///
    /// [`SampleRate::DEFAULT`]: tutti_core::SampleRate::DEFAULT
    /// [`AudioUnit::set_sample_rate`]: tutti_core::AudioUnit::set_sample_rate
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

    /// Switch to beat-synced mode, taking the beat on the input ports.
    ///
    /// The node gains [`BEAT_PORTS`] inputs, wired from `TransportClock`:
    /// port 0 whole beats, port 1 the fraction. This is per-sample accurate.
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

    /// Set the modulation [`Depth`], clamped to `-1.0..=1.0`.
    ///
    /// Bipolar: a negative depth inverts the modulator, so `-1.0` is the same
    /// shape phase-flipped and `0.0` is flat. Values past full scale saturate
    /// rather than wrapping.
    pub fn with_depth(self, depth: impl Into<Depth>) -> Self {
        self.depth.store(Depth::new_clamped(depth.into().get()));
        self
    }

    /// Set the [`PhaseIncrement`] offset, conventionally `0.0` to `1.0` for one
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

    /// Set the free-running rate. **Ignored in [`LfoMode::BeatSynced`]**, where
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

    /// Set the beat-synced span. **Ignored in [`LfoMode::FreeRunning`]** — the
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

// The `AudioUnit` trait itself requires `Send + Sync + Clone + 'static` (a
// fundsp `Net` node must be movable across the RT boundary and cloneable for
// backend swaps). Those bounds live on the *adapter* impl, not on `Modulator`,
// so `tutti-mod` stays usable by non-audio consumers with no such constraint.
impl<M: Modulator + Clone + Send + Sync + 'static> AudioUnit for ModulatorNode<M> {
    fn inputs(&self) -> usize {
        match self.mode {
            LfoMode::FreeRunning => 0,
            LfoMode::BeatSynced => BEAT_PORTS,
        }
    }

    fn outputs(&self) -> usize {
        1
    }

    /// Detach every control cell this node reads (see `Param::detach`), so
    /// a fork renders the controls as they were when it was taken, not the
    /// live knob moves made while it runs. Values are kept.
    fn isolate(&mut self) {
        self.frequency.detach();
        self.depth.detach();
        self.phase_offset.detach();
    }

    fn reset(&mut self) {
        self.phase = Phase::START;
        // The node owns the modulator's threaded state, so it resets it here to
        // the seed — this is what reaches a stateful modulator's RNG.
        // Stateless modulators reset a `()`.
        self.mod_state = M::State::default();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let phase_offset = self.phase_offset.load();

        let phase = match self.mode {
            LfoMode::FreeRunning => {
                // Evaluate the current phase, then advance — must match the
                // ordering in `process` and `route` so one sample through
                // `tick` equals the same sample through `process`.
                let freq = self.frequency.load().get();
                let phase = self.phase.offset_by(phase_offset);
                self.phase = self
                    .phase
                    .advance(PhaseIncrement::per_sample(Hz(freq), self.sample_rate));
                phase
            }
            LfoMode::BeatSynced => {
                let beat = beat_from_ports(input[0], input[1]);
                // A non-positive span freezes at the offset — the guard lives in
                // `Beat::cycles_of`, which `beat_phase` goes through.
                beat_phase(beat, self.beats_per_cycle()).offset_by(phase_offset)
            }
        };

        output[0] = self.evaluate(phase);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let phase_offset = self.phase_offset.load();

        match self.mode {
            LfoMode::FreeRunning => {
                let freq = self.frequency.load().get();
                let phase_increment = PhaseIncrement::per_sample(Hz(freq), self.sample_rate);

                for i in 0..size {
                    let phase = self.phase.offset_by(phase_offset);
                    output.set_f32(0, i, self.evaluate(phase));

                    self.phase = self.phase.advance(phase_increment);
                }
            }
            LfoMode::BeatSynced => {
                // Read once per block, not per sample.
                let beats_per_cycle = self.beats_per_cycle();

                for i in 0..size {
                    let beat = beat_from_ports(input.at_f32(0, i), input.at_f32(1, i));
                    let phase = beat_phase(beat, beats_per_cycle).offset_by(phase_offset);
                    output.set_f32(0, i, self.evaluate(phase));
                }
            }
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::LFO_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // A modulator's output is a running signal, not a statically-known
        // constant, so every shape reports `Signal::Unknown` and fundsp's
        // PDC/const-fold pass treats it as varying. Folding a deterministic LFO
        // to a constant `Signal::Value` would need a per-modulator "is this
        // foldable?" hook, which the pure `Modulator` trait deliberately does
        // not carry — it is `phase -> value` and nothing else. `route` must
        // also not step the modulator's state.
        SignalFrame::new(1)
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
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
    use tutti_core::Signal;

    // The waveform math is `tutti-mod`'s, and so are the tests that pin it.
    // What belongs here is the adapter around it: phase generation from
    // transport, depth, and the `AudioUnit` surface.

    #[test]
    fn test_free_running_lfo() {
        let mut lfo = LfoNode::new(LfoShape::Sine);
        lfo.set_sample_rate(tutti_core::SampleRate(100.0));

        let mut output = [0.0f32];

        for _ in 0..25 {
            lfo.tick(&[], &mut output);
        }

        assert!(
            (output[0] - 1.0).abs() < 0.1,
            "Expected ~1.0, got {}",
            output[0]
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
        assert_eq!(lfo.inputs(), 0, "a free-running LFO reads no beat ports");
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

    #[test]
    fn test_beat_synced_lfo() {
        let mut lfo = LfoNode::new(LfoShape::Sine).with_beat_sync(4.0);
        assert_eq!(lfo.inputs(), BEAT_PORTS);

        let mut output = [0.0f32];

        // Beat 1.0 of a 4-beat cycle = quarter phase = sine peak.
        lfo.tick(&[1.0, 0.0], &mut output);
        assert!(
            (output[0] - 1.0).abs() < 0.01,
            "Expected 1.0, got {}",
            output[0]
        );

        lfo.tick(&[2.0, 0.0], &mut output);
        assert!(
            (output[0] - 0.0).abs() < 0.01,
            "Expected 0.0, got {}",
            output[0]
        );
    }

    #[test]
    fn beat_synced_lfo_reads_fraction_from_port_1() {
        let mut lfo = LfoNode::new(LfoShape::Sine).with_beat_sync(4.0);
        let mut split = [0.0f32];
        let mut whole = [0.0f32];

        // Beat 1.0 delivered as (0.0 whole + 1.0 frac) must equal (1.0 + 0.0):
        // the node reconstructs the beat by summing both ports.
        lfo.tick(&[0.0, 1.0], &mut split);
        lfo.tick(&[1.0, 0.0], &mut whole);
        assert!(
            (split[0] - whole[0]).abs() < 1e-6,
            "port split changed the beat: {} vs {}",
            split[0],
            whole[0]
        );
    }

    /// The split exists so precision does not decay at high beat counts: a
    /// single f32 cannot resolve sub-beat detail past ~16384 beats.
    #[test]
    fn beat_synced_lfo_keeps_sub_beat_precision_at_high_beats() {
        let mut lfo = LfoNode::new(LfoShape::Sine).with_beat_sync(4.0);
        let mut at_edge = [0.0f32];
        let mut past_edge = [0.0f32];

        // Same fractional offset (0.5 beat) at beat 0 and at beat 20000.
        lfo.tick(&[0.0, 0.5], &mut at_edge);
        lfo.tick(&[20000.0, 0.5], &mut past_edge);

        // 20000 is a multiple of the 4-beat cycle, so both are the same phase.
        assert!(
            (at_edge[0] - past_edge[0]).abs() < 0.01,
            "sub-beat precision lost at high beat count: {} vs {}",
            at_edge[0],
            past_edge[0]
        );
    }

    #[test]
    fn test_depth_control() {
        let mut lfo = LfoNode::new(LfoShape::Square);
        lfo.set_depth(0.5);

        let mut output = [0.0f32];
        lfo.tick(&[], &mut output);

        assert!((output[0] - 0.5).abs() < 0.01);
    }

    /// Negative depth inverts the modulator rather than silencing it.
    ///
    /// `Depth` is bipolar: `set_depth(-1.0)` stores `-1.0` and phase-flips the
    /// shape. Clamping to `0.0..=1.0` instead would store `0.0` and take the
    /// LFO flat, which is silent in both senses.
    #[test]
    fn negative_depth_inverts_instead_of_silencing() {
        let mut positive = LfoNode::new(LfoShape::Square);
        positive.set_depth(1.0);
        let mut a = [0.0f32];
        positive.tick(&[], &mut a);

        let mut negative = LfoNode::new(LfoShape::Square);
        negative.set_depth(-1.0);
        let mut b = [0.0f32];
        negative.tick(&[], &mut b);

        assert!(a[0].abs() > 0.01, "the reference tick must be audible");
        assert!(
            (b[0] + a[0]).abs() < 1e-6,
            "expected {} to invert to {}",
            a[0],
            -a[0]
        );

        // And the range still saturates past full scale.
        let mut clamped = LfoNode::new(LfoShape::Square);
        clamped.set_depth(-5.0);
        let mut c = [0.0f32];
        clamped.tick(&[], &mut c);
        assert!((c[0] - b[0]).abs() < 1e-6);
    }

    #[test]
    fn test_phase_offset() {
        let mut lfo = LfoNode::new(LfoShape::Sine);
        lfo.set_sample_rate(tutti_core::SampleRate(100.0));
        lfo.set_phase_offset(0.25);

        let mut output = [0.0f32];
        lfo.tick(&[], &mut output);

        assert!(
            (output[0] - 1.0).abs() < 0.1,
            "Expected ~1.0, got {}",
            output[0]
        );
    }

    #[test]
    fn test_lfo_reset() {
        let mut lfo = LfoNode::new(LfoShape::Sine);
        lfo.set_sample_rate(tutti_core::SampleRate(100.0));

        let mut output = [0.0f32];
        for _ in 0..50 {
            lfo.tick(&[], &mut output);
        }

        lfo.reset();

        let mut output_after = [0.0f32];
        lfo.tick(&[], &mut output_after);
        assert!(
            output_after[0].abs() < 0.1,
            "After reset, LFO should start near zero"
        );
    }

    #[test]
    fn test_random_produces_different_values() {
        let mut lfo = LfoNode::new(LfoShape::Random);
        lfo.set_sample_rate(tutti_core::SampleRate(100.0));

        let mut values = Vec::new();
        let mut output = [0.0f32];

        for _ in 0..500 {
            lfo.tick(&[], &mut output);
            values.push(output[0]);
        }

        let unique: std::collections::HashSet<u32> =
            values.iter().map(|v| (v * 1000.0) as u32).collect();

        assert!(
            unique.len() > 1,
            "Random LFO should produce different values, got {}",
            unique.len()
        );
    }

    #[test]
    fn test_route_reports_unknown_for_every_shape() {
        // A modulator's output is a running signal, so `route` reports
        // `Signal::Unknown` for *every* shape — never a constant. Sine, Random
        // and the rest are uniformly Unknown; nothing folds a deterministic LFO
        // to a `Signal::Value`. Covers the case where Random is misreported as
        // constant-0.
        for shape in LfoShape::all() {
            let mut lfo = LfoNode::new(*shape);
            lfo.set_sample_rate(tutti_core::SampleRate(44100.0));
            let out = lfo.route(&SignalFrame::new(1), 44100.0);
            assert!(
                matches!(out.at(0), Signal::Unknown),
                "{shape} LFO route() must be Signal::Unknown"
            );
        }
    }
}
