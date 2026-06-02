//! Low Frequency Oscillator (LFO) node.

use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::{
    dsp::{Signal, DEFAULT_SR},
    AudioUnit, BufferMut, BufferRef, SignalFrame, TransportHandle, TransportReader,
};

use tutti_core::{Hz, Linear, Param};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LfoShape {
    #[default]
    Sine,
    Triangle,
    Square,
    Sawtooth,
    SawtoothDown,
    Random,
    RandomSmooth,
}

impl LfoShape {
    /// True for shapes whose output depends on per-instance state (not just phase).
    #[inline]
    pub fn is_random(&self) -> bool {
        matches!(self, Self::Random | Self::RandomSmooth)
    }

    /// Evaluate a purely phase-deterministic shape. Callers must first check
    /// [`LfoShape::is_random`]; random shapes require per-instance state and
    /// are not handled here.
    #[inline]
    pub fn evaluate_periodic(&self, phase: f32) -> f32 {
        match self {
            Self::Sine => (phase * core::f32::consts::TAU).sin(),
            Self::Triangle => {
                let p = phase * 4.0;
                if p < 1.0 {
                    p
                } else if p < 3.0 {
                    2.0 - p
                } else {
                    p - 4.0
                }
            }
            Self::Square => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Self::Sawtooth => phase * 2.0 - 1.0,
            Self::SawtoothDown => 1.0 - phase * 2.0,
            Self::Random | Self::RandomSmooth => {
                debug_assert!(false, "evaluate_periodic called on random shape");
                0.0
            }
        }
    }

    pub fn all() -> &'static [Self] {
        &[
            Self::Sine,
            Self::Triangle,
            Self::Square,
            Self::Sawtooth,
            Self::SawtoothDown,
            Self::Random,
            Self::RandomSmooth,
        ]
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Sine => "Sine",
            Self::Triangle => "Triangle",
            Self::Square => "Square",
            Self::Sawtooth => "Sawtooth",
            Self::SawtoothDown => "Saw Down",
            Self::Random => "Random",
            Self::RandomSmooth => "Random (Smooth)",
        }
    }
}

impl core::fmt::Display for LfoShape {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

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

pub struct LfoNode<R: TransportReader = TransportHandle> {
    shape: LfoShape,
    mode: LfoMode,
    /// In `FreeRunning` mode: oscillator frequency in Hz.
    /// In `BeatSynced` mode: beats per cycle (stored in the same atomic; the
    /// unit is context-dependent on `mode`).
    frequency: Param<Hz>,
    depth: Param<Linear>,
    phase_offset: Param<Linear>,
    phase: f32,
    sample_rate: f64,
    random_state: RandomState,
    transport: Option<R>,
}

#[derive(Debug, Clone)]
struct RandomState {
    current: f32,
    previous: f32,
    last_phase: f32,
    seed: u32,
}

impl Default for RandomState {
    fn default() -> Self {
        Self {
            current: 0.0,
            previous: 0.0,
            last_phase: 0.0,
            seed: 12345,
        }
    }
}

impl RandomState {
    fn next(&mut self) -> f32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        (self.seed as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    fn update_for_phase(&mut self, phase: f32) {
        if phase < self.last_phase - 0.5 {
            self.previous = self.current;
            self.current = self.next();
        }
        self.last_phase = phase;
    }

    fn get_random(&self) -> f32 {
        self.current
    }

    fn get_random_smooth(&self, phase: f32) -> f32 {
        self.previous + (self.current - self.previous) * phase
    }
}

impl LfoNode {
    /// Create a free-running LFO with default frequency 1.0 Hz.
    ///
    /// Chain `.with_frequency(hz)`, `.with_beat_sync(transport, beats)`, or
    /// `.with_beat_sync_input(beats)` to configure further.
    pub fn new(shape: LfoShape) -> Self {
        Self::build(shape, LfoMode::FreeRunning, 1.0, None)
    }
}

impl<R: TransportReader> LfoNode<R> {
    fn build(shape: LfoShape, mode: LfoMode, freq_or_beats: f32, transport: Option<R>) -> Self {
        Self {
            shape,
            mode,
            frequency: Param::new(Hz(freq_or_beats)),
            depth: Param::new(Linear(1.0)),
            phase_offset: Param::new(Linear(0.0)),
            phase: 0.0,
            sample_rate: DEFAULT_SR,
            random_state: RandomState::default(),
            transport,
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

    /// Switch to beat-synced mode reading the beat position from `transport`.
    ///
    /// In this mode the LFO has 0 inputs — it reads the current beat directly
    /// from the transport reader.
    pub fn with_beat_sync(mut self, transport: R, beats_per_cycle: impl Into<Hz>) -> Self {
        self.mode = LfoMode::BeatSynced;
        self.transport = Some(transport);
        self.frequency.store(beats_per_cycle.into());
        self
    }

    /// Switch to beat-synced mode reading the beat position from input 0.
    ///
    /// In this mode the LFO has 1 input (the current beat).
    pub fn with_beat_sync_input(mut self, beats_per_cycle: impl Into<Hz>) -> Self {
        self.mode = LfoMode::BeatSynced;
        self.transport = None;
        self.frequency.store(beats_per_cycle.into());
        self
    }

    /// Set the LFO depth (0.0 - 1.0).
    pub fn with_depth(self, depth: impl Into<Linear>) -> Self {
        self.depth.store(Linear(depth.into().get().clamp(0.0, 1.0)));
        self
    }

    /// Set the phase offset (0.0 - 1.0).
    pub fn with_phase_offset(self, offset: impl Into<Linear>) -> Self {
        self.phase_offset.store(Linear(offset.into().get() % 1.0));
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

    pub fn set_depth(&self, depth: impl Into<Linear>) {
        self.depth.store(Linear(depth.into().get().clamp(0.0, 1.0)));
    }

    pub fn set_phase_offset(&self, offset: impl Into<Linear>) {
        self.phase_offset.store(Linear(offset.into().get() % 1.0));
    }

    #[inline]
    fn evaluate(&mut self, phase: f32) -> f32 {
        let depth = self.depth.load().get();

        match self.shape {
            LfoShape::Random => {
                self.random_state.update_for_phase(phase);
                self.random_state.get_random() * depth
            }
            LfoShape::RandomSmooth => {
                self.random_state.update_for_phase(phase);
                self.random_state.get_random_smooth(phase) * depth
            }

            _ => self.shape.evaluate_periodic(phase) * depth,
        }
    }
}

impl<R: TransportReader + Clone + 'static> AudioUnit for LfoNode<R> {
    fn inputs(&self) -> usize {
        match self.mode {
            LfoMode::FreeRunning => 0,
            LfoMode::BeatSynced => usize::from(self.transport.is_none()),
        }
    }

    fn outputs(&self) -> usize {
        1
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        self.random_state = RandomState::default();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let phase_offset = self.phase_offset.load().get();

        let phase = match self.mode {
            LfoMode::FreeRunning => {
                let freq = self.frequency.load().get();
                self.phase += freq / self.sample_rate as f32;
                if self.phase >= 1.0 {
                    self.phase -= 1.0;
                }
                (self.phase + phase_offset) % 1.0
            }
            LfoMode::BeatSynced => {
                let beat = if let Some(ref transport) = self.transport {
                    transport.current_beat() as f32
                } else {
                    input[0]
                };
                let beats_per_cycle = self.frequency.load().get();
                if beats_per_cycle > 0.0 {
                    ((beat / beats_per_cycle) + phase_offset) % 1.0
                } else {
                    phase_offset
                }
            }
        };

        output[0] = self.evaluate(phase);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let phase_offset = self.phase_offset.load().get();

        match self.mode {
            LfoMode::FreeRunning => {
                let freq = self.frequency.load().get();
                let phase_increment = freq / self.sample_rate as f32;

                for i in 0..size {
                    let phase = (self.phase + phase_offset) % 1.0;
                    output.set_f32(0, i, self.evaluate(phase));

                    self.phase += phase_increment;
                    if self.phase >= 1.0 {
                        self.phase -= 1.0;
                    }
                }
            }
            LfoMode::BeatSynced => {
                let beats_per_cycle = self.frequency.load().get();

                if let Some(ref transport) = self.transport {
                    let beat = transport.current_beat() as f32;
                    let phase = if beats_per_cycle > 0.0 {
                        ((beat / beats_per_cycle) + phase_offset) % 1.0
                    } else {
                        phase_offset
                    };
                    let value = self.evaluate(phase);
                    for i in 0..size {
                        output.set_f32(0, i, value);
                    }
                } else {
                    for i in 0..size {
                        let beat = input.at_f32(0, i);
                        let phase = if beats_per_cycle > 0.0 {
                            ((beat / beats_per_cycle) + phase_offset) % 1.0
                        } else {
                            phase_offset
                        };
                        output.set_f32(0, i, self.evaluate(phase));
                    }
                }
            }
        }
    }

    fn get_id(&self) -> u64 {
        tutti_core::node_id::LFO_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(1);

        // Random shapes are stateful; their output cannot be expressed as a
        // constant Signal::Value, so we leave the default Signal::Unknown.
        if self.shape.is_random() {
            return output;
        }

        if self.mode == LfoMode::BeatSynced {
            if let Signal::Value(beat) = input.at(0) {
                let beats_per_cycle = self.frequency.load().get() as f64;
                let phase_offset = self.phase_offset.load().get() as f64;
                let phase = if beats_per_cycle > 0.0 {
                    ((beat / beats_per_cycle) + phase_offset) % 1.0
                } else {
                    phase_offset
                };
                let value = self.shape.evaluate_periodic(phase as f32) * self.depth.load().get();
                output.set(0, Signal::Value(value as f64));
            }
        } else {
            let phase_offset = self.phase_offset.load().get();
            let value =
                self.shape.evaluate_periodic(self.phase + phase_offset) * self.depth.load().get();
            output.set(0, Signal::Value(value as f64));
        }

        output
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl<R: TransportReader + Clone> Clone for LfoNode<R> {
    fn clone(&self) -> Self {
        Self {
            shape: self.shape,
            mode: self.mode,
            frequency: self.frequency.handle(),
            depth: self.depth.handle(),
            phase_offset: self.phase_offset.handle(),
            phase: self.phase,
            sample_rate: self.sample_rate,
            random_state: self.random_state.clone(),
            transport: self.transport.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lfo_shapes() {
        let sine_val = LfoShape::Sine.evaluate_periodic(0.25);
        assert!((sine_val - 1.0).abs() < 0.01);

        let square_val = LfoShape::Square.evaluate_periodic(0.25);
        assert_eq!(square_val, 1.0);

        let square_val2 = LfoShape::Square.evaluate_periodic(0.75);
        assert_eq!(square_val2, -1.0);

        let tri_val = LfoShape::Triangle.evaluate_periodic(0.25);
        assert!((tri_val - 1.0).abs() < 0.01);

        let saw_val = LfoShape::Sawtooth.evaluate_periodic(0.5);
        assert!((saw_val - 0.0).abs() < 0.01);
    }

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
        let mut lfo = LfoNode::new(LfoShape::Sine).with_beat_sync_input(4.0);

        let mut output = [0.0f32];

        lfo.tick(&[1.0], &mut output);
        assert!(
            (output[0] - 1.0).abs() < 0.01,
            "Expected 1.0, got {}",
            output[0]
        );

        lfo.tick(&[2.0], &mut output);
        assert!(
            (output[0] - 0.0).abs() < 0.01,
            "Expected 0.0, got {}",
            output[0]
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
    fn test_sawtooth_down_shape() {
        let val_start = LfoShape::SawtoothDown.evaluate_periodic(0.0);
        assert!((val_start - 1.0).abs() < 0.01);

        let val_mid = LfoShape::SawtoothDown.evaluate_periodic(0.5);
        assert!((val_mid - 0.0).abs() < 0.01);

        let val_end = LfoShape::SawtoothDown.evaluate_periodic(1.0);
        assert!((val_end - (-1.0)).abs() < 0.01);
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
    fn test_route_random_reports_unknown_not_zero() {
        // Regression: LfoShape::Random used to evaluate as 0.0 in route(),
        // silently misreporting random-mode LFOs as constant-0 in PDC analysis.
        let mut lfo = LfoNode::new(LfoShape::Random);
        lfo.set_sample_rate(tutti_core::SampleRate(44100.0));
        let out = lfo.route(&SignalFrame::new(1), 44100.0);
        assert!(
            matches!(out.at(0), Signal::Unknown),
            "Random LFO route() must be Signal::Unknown"
        );

        let mut sine = LfoNode::new(LfoShape::Sine);
        sine.set_sample_rate(tutti_core::SampleRate(44100.0));
        let out = sine.route(&SignalFrame::new(1), 44100.0);
        assert!(
            matches!(out.at(0), Signal::Value(_)),
            "Sine LFO route() must be Signal::Value"
        );
    }
}
