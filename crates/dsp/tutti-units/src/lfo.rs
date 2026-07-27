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
//! `value()` inlines: codegen is identical to the old hand-inlined `LfoNode`,
//! so the extraction is RT-cost-free.
//!
//! The waveform math (`LfoShape::evaluate_periodic`), the random stepper
//! (`RandomState`), and the `Lfo` modulator now live in `tutti-mod`; this file
//! is purely the adapter. `LfoShape` is re-exported so downstream `use
//! tutti_units::LfoShape` keeps working.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{
    beat_from_ports, dsp::DEFAULT_SR, AudioUnit, BufferMut, BufferRef, SignalFrame, BEAT_PORTS,
};

use tutti_core::{Depth, Hz, Param, Phase, PhaseIncrement};

// The waveform vocabulary + the pure LFO modulator live in tutti-mod now. Re-
// exported so existing `use tutti_units::LfoShape` / `Lfo` sites are untouched.
pub use tutti_mod::{Lfo, LfoShape, Modulator};

/// Whether the adapter derives phase from a free-running oscillator or from the
/// transport beat. This is an *adapter* concern (transport wiring), not
/// modulation math — hence it lives here, not in `tutti-mod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LfoMode {
    FreeRunning,
    BeatSynced,
}

impl LfoMode {
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
    /// In `FreeRunning` mode: oscillator frequency in Hz.
    /// In `BeatSynced` mode: beats per cycle (stored in the same atomic; the
    /// unit is context-dependent on `mode`).
    frequency: Param<Hz>,
    depth: Param<Depth>,
    phase_offset: Param<PhaseIncrement>,
    phase: Phase,
    sample_rate: f64,
}

/// The concrete LFO node the graph builds — a [`ModulatorNode`] driving a pure
/// [`Lfo`]. Monomorphized, so `value()` inlines to the old codegen.
pub type LfoNode = ModulatorNode<Lfo>;

impl ModulatorNode<Lfo> {
    /// Create a free-running LFO with default frequency 1.0 Hz.
    ///
    /// Chain `.with_frequency(hz)` or `.with_beat_sync(beats)` to configure
    /// further.
    pub fn new(shape: LfoShape) -> Self {
        Self::with_modulator(Lfo::new(shape), LfoMode::FreeRunning, 1.0)
    }
}

impl<M: Modulator> ModulatorNode<M> {
    /// Build a modulation node over an arbitrary pure modulator. The generic
    /// entry point behind [`LfoNode::new`]; also the seam any future modulator
    /// (envelope, sample & hold, …) wires through.
    pub fn with_modulator(modulator: M, mode: LfoMode, freq_or_beats: f32) -> Self {
        Self {
            modulator,
            mod_state: M::State::default(),
            mode,
            frequency: Param::new(Hz(freq_or_beats)),
            depth: Param::new(Depth::FULL),
            phase_offset: Param::new(PhaseIncrement(0.0)),
            phase: Phase::START,
            sample_rate: DEFAULT_SR,
        }
    }

    /// Set the frequency in Hz (free-running) or beats-per-cycle (beat-synced).
    ///
    /// In free-running mode this is the oscillator frequency in Hz. In
    /// beat-synced mode this is beats-per-cycle (the same atomic is reused;
    /// the unit is context-dependent on `mode`).
    pub fn with_frequency(self, hz_or_beats: impl Into<Hz>) -> Self {
        self.frequency.store(hz_or_beats.into());
        self
    }

    /// Switch to beat-synced mode, taking the beat on the input ports.
    ///
    /// The node gains [`BEAT_PORTS`] inputs, wired from `TransportClock`:
    /// port 0 whole beats, port 1 the fraction. This is per-sample accurate.
    pub fn with_beat_sync(mut self, beats_per_cycle: impl Into<Hz>) -> Self {
        self.mode = LfoMode::BeatSynced;
        self.frequency.store(beats_per_cycle.into());
        self
    }

    /// Set the modulation depth, `-1.0` to `1.0`.
    ///
    /// Bipolar since the `Depth` split: a negative depth inverts the
    /// modulator, so `-1.0` is the same shape phase-flipped. This setter
    /// previously clamped to `0.0..=1.0`, so a negative argument silenced
    /// modulation instead of inverting it.
    pub fn with_depth(self, depth: impl Into<Depth>) -> Self {
        self.depth.store(Depth::new_clamped(depth.into().get()));
        self
    }

    /// Set the phase offset (0.0 - 1.0).
    ///
    /// Stored as given; the wrap happens where the offset is *applied*, via
    /// `Phase::advance`. Wrapping here as well would be redundant, and the old
    /// `% 1.0` was actively wrong for a negative offset — it left the value
    /// negative, which then read off the front of the shape table.
    pub fn with_phase_offset(self, offset: impl Into<PhaseIncrement>) -> Self {
        self.phase_offset.store(offset.into());
        self
    }

    pub fn frequency(&self) -> Arc<AtomicF32> {
        self.frequency.as_atomic()
    }

    pub fn depth(&self) -> Arc<AtomicF32> {
        self.depth.as_atomic()
    }

    pub fn phase_offset(&self) -> Arc<AtomicF32> {
        self.phase_offset.as_atomic()
    }

    pub fn set_frequency(&self, freq: impl Into<Hz>) {
        self.frequency.store(freq.into());
    }

    pub fn set_depth(&self, depth: impl Into<Depth>) {
        self.depth.store(Depth::new_clamped(depth.into().get()));
    }

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

    fn reset(&mut self) {
        self.phase = Phase::START;
        // The node owns the modulator's threaded state, so it resets it here to
        // the seed — cleaner than the old node, which could not reach the
        // modulator's internal RNG. Stateless modulators reset a `()`.
        self.mod_state = M::State::default();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
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
                let beat = beat_from_ports(input[0], input[1]) as f32;
                let beats_per_cycle = self.frequency.load().get();
                if beats_per_cycle > 0.0 {
                    Phase::wrapped(beat / beats_per_cycle).offset_by(phase_offset)
                } else {
                    Phase::START.offset_by(phase_offset)
                }
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
                let beats_per_cycle = self.frequency.load().get();

                for i in 0..size {
                    let beat = beat_from_ports(input.at_f32(0, i), input.at_f32(1, i)) as f32;
                    let phase = if beats_per_cycle > 0.0 {
                        Phase::wrapped(beat / beats_per_cycle).offset_by(phase_offset)
                    } else {
                        Phase::START.offset_by(phase_offset)
                    };
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
        // constant, so report `Signal::Unknown` — fundsp's PDC/const-fold pass
        // treats it as varying. (We deliberately don't try to fold a stopped
        // deterministic LFO to a constant `Signal::Value`; that was a minor
        // optimization whose only enabler — a per-modulator "is this foldable?"
        // hook — has been dropped to keep the pure `Modulator` trait to
        // `phase -> value`. `route` also must not step the modulator's state.)
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
    use tutti_core::dsp::Signal;

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
    /// This is a deliberate behaviour change from the `Depth` split: the old
    /// setter clamped to `0.0..=1.0`, so `set_depth(-1.0)` stored `0.0` and
    /// the LFO went flat. It now stores `-1.0` and phase-flips the shape.
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
        // `Signal::Unknown` for *every* shape — never a constant. This subsumes
        // the old regression (Random must not be misreported as constant-0);
        // now Sine, Random, and every other shape are uniformly Unknown, since
        // we no longer try to fold a deterministic LFO to a `Signal::Value`.
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
