//! The band-limited oscillator source: [`OscillatorNode`], a sine, saw, square or
//! triangle at a frequency and a level.
//!
//! The node holds the params, the sample rate and the shape; the algorithm is
//! `tutti_dsp::Oscillator`.

use crate::ramp::LastGood;
use tutti_dsp::OscillatorWave;
use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
use tutti_types::{Amplitude, ChannelLayout, Hz, Param, SampleRate, Tail, UnitParam};

/// A band-limited oscillator: a source with no inputs and one (mono) output.
///
/// A sine is exact; a saw and a square are band-limited with PolyBLEP and a
/// triangle with PolyBLAMP (see `tutti_dsp::Oscillator`), so none folds its
/// harmonics back below Nyquist. A frequency outside `0..=Nyquist` is clamped
/// for the step, and one that is not finite holds the phase.
///
/// Its params are the frequency ([`UnitParam::Frequency`], in [`Hz`]) and the level
/// ([`UnitParam::Volume`], in [`Amplitude`], a linear gain on the unit-peak
/// wave), both read per frame: a driven lane's every frame is heard, and a
/// non-finite driven value holds the last finite one. Inserted, its controls
/// are a [`ParamSet`]; a fork starts from the values last set through it, at
/// phase 0.
///
/// The node renders silence until the graph prepares it at the device rate
/// ([`Node::prepare`]). It is a source with no ring-out ([`Tail::None`]): what
/// it emits is a function of its clock, so a render's end ends it.
///
/// # Examples
///
/// ```
/// use tutti_graph::{GraphBuilder, Params, Prepare};
/// use tutti_nodes::OscillatorNode;
/// use tutti_types::{Amplitude, ChannelLayout, Hz, MaxBlock, SampleRate, Samples};
///
/// let mut g = GraphBuilder::new(ChannelLayout::MONO, ChannelLayout::MONO);
/// let tone = g.add_with_controls(Params(OscillatorNode::sine(Hz(440.0)).with_level(Amplitude(0.5))));
/// g.pipe_output(tone.0);
/// let mut r = g
///     .renderer(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(64))))
///     .expect("builds");
/// let out = r.render_input(&[&[0.0; 64]]);
/// assert_eq!(out[0][0], 0.0);
/// assert!(out[0][1] > 0.0);
/// ```
#[derive(Debug)]
pub struct OscillatorNode {
    wave: tutti_dsp::Oscillator,
    frequency: Param<Hz>,
    level: Param<Amplitude>,
    /// `None` until [`Node::prepare`]: no value is computed from a guessed rate.
    rate: Option<SampleRate>,
    /// The last finite frequency read from the cell or the port.
    good_frequency: LastGood,
    /// The last finite level read from the cell or the port.
    good_level: LastGood,
}

// The params an [`OscillatorNode`] lets the graph modulate, in port order.
tutti_graph::declare_params!([] OscillatorNode, n {
    Frequency => Hz = &n.frequency,
    Volume => Amplitude = &n.level,
});

impl OscillatorNode {
    fn new(wave: OscillatorWave, frequency: Hz) -> Self {
        Self {
            wave: tutti_dsp::Oscillator::new(wave),
            frequency: Param::new(frequency),
            level: Param::new(Amplitude::UNITY),
            rate: None,
            good_frequency: LastGood::new(frequency.get(), 0.0),
            good_level: LastGood::new(Amplitude::UNITY.get(), 1.0),
        }
    }

    /// A sine at `frequency`, unity level.
    #[must_use]
    pub fn sine(frequency: impl Into<Hz>) -> Self {
        Self::new(OscillatorWave::Sine, frequency.into())
    }

    /// A band-limited rising saw at `frequency`, unity level.
    #[must_use]
    pub fn saw(frequency: impl Into<Hz>) -> Self {
        Self::new(OscillatorWave::Saw, frequency.into())
    }

    /// A band-limited square at `frequency`, unity level.
    #[must_use]
    pub fn square(frequency: impl Into<Hz>) -> Self {
        Self::new(OscillatorWave::Square, frequency.into())
    }

    /// A band-limited triangle at `frequency`, unity level.
    #[must_use]
    pub fn triangle(frequency: impl Into<Hz>) -> Self {
        Self::new(OscillatorWave::Triangle, frequency.into())
    }

    /// Sets the level the output is scaled by.
    #[must_use]
    pub fn with_level(self, level: impl Into<Amplitude>) -> Self {
        self.level.store(level.into());
        self
    }

    /// Sets the frequency; heard from the next block. Lock-free.
    pub fn set_frequency(&self, frequency: impl Into<Hz>) {
        self.frequency.store(frequency.into());
    }

    /// Sets the level; heard from the next block. Lock-free.
    pub fn set_level(&self, level: impl Into<Amplitude>) {
        self.level.store(level.into());
    }

    /// A handle on the frequency cell, for a UI or automation to share.
    #[must_use]
    pub fn frequency(&self) -> Param<Hz> {
        self.frequency.handle()
    }

    /// A handle on the level cell, for a UI or automation to share.
    #[must_use]
    pub fn level(&self) -> Param<Amplitude> {
        self.level.handle()
    }
}

/// Shares the cells (the template [`tutti_graph::param_parts`] forks from).
impl Clone for OscillatorNode {
    fn clone(&self) -> Self {
        Self {
            wave: self.wave,
            frequency: self.frequency.handle(),
            level: self.level.handle(),
            rate: self.rate,
            good_frequency: self.good_frequency,
            good_level: self.good_level,
        }
    }
}

impl Node for OscillatorNode {
    /// No inputs, one output, no latency, no ring-out; arithmetic and one
    /// `sin` a frame, `Light`.
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
            .with_cost(tutti_graph::Cost::Light)
            .with_tail(Tail::None)
            .with_param_addrs(self.declared_ports().as_slice())
    }

    fn prepare(&mut self, prepare: &Prepare) {
        self.rate = Some(prepare.sample_rate());
    }

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let Some(rate) = self.rate else {
            return Status::Silent;
        };
        let size = io.frames();
        let frequency = self
            .declared_param::<{ UnitParam::Frequency as u16 }>(&io)
            .frames();
        let level = self
            .declared_param::<{ UnitParam::Volume as u16 }>(&io)
            .frames();
        let frequency_cell = self.frequency.load().get();
        let level_cell = self.level.load().get();
        let out = io.output(0);
        for (i, o) in out[..size].iter_mut().enumerate() {
            let f = self
                .good_frequency
                .read(frequency.map_or(frequency_cell, |l| l[i]));
            let g = self.good_level.read(level.map_or(level_cell, |l| l[i]));
            *o = self.wave.step(Hz(f), rate) * g;
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.wave.reset();
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        self.declared_base(k)
    }
}

impl ParamNode for OscillatorNode {
    /// The frequency and the level.
    fn param_set(&self) -> ParamSet {
        self.declared_set()
    }

    /// A clone with its cells detached (at their values now), at phase 0.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.frequency.detach();
        fork.level.detach();
        Node::reset(&mut fork);
        fork
    }
}

/// Inserted with its [`ParamSet`] as its controls and a fork from the values
/// last set through it ([`tutti_graph::param_parts`]).
impl IntoNode for OscillatorNode {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        tutti_graph::param_parts(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{RATE, amplitude_at, mean};
    use tutti_graph::contract::{Direct, assert_param_fork, drive};

    fn render(node: OscillatorNode, frames: usize) -> Vec<f32> {
        let mut d = Direct::new(node, RATE, frames);
        d.block();
        d.output(0).to_vec()
    }

    #[test]
    fn a_fork_shares_no_cell() {
        assert_param_fork(OscillatorNode::saw(Hz(220.0)));
    }

    #[test]
    fn the_shape_is_a_mono_source_declaring_frequency_then_level() {
        let node = OscillatorNode::sine(Hz(440.0)).with_level(Amplitude(0.25));
        let shape = node.shape();
        assert_eq!(shape.audio_in().count(), 0);
        assert_eq!(shape.audio_out().count(), 1);
        assert_eq!(
            shape.params.as_slice(),
            &[UnitParam::Frequency, UnitParam::Volume][..]
        );
        assert_eq!(node.param_base(0), Some(440.0));
        assert_eq!(node.param_base(1), Some(0.25));
    }

    #[test]
    fn the_sine_is_sin_two_pi_f_t_at_the_graph_rate() {
        let out = render(OscillatorNode::sine(Hz(440.0)), 1_000);
        for (n, &y) in out.iter().enumerate() {
            let exact = libm::sin(2.0 * core::f64::consts::PI * 440.0 * n as f64 / 48_000.0);
            assert!((f64::from(y) - exact).abs() < 1e-6, "sample {n}");
        }
    }

    #[test]
    fn level_scales_the_output_exactly() {
        let full = render(OscillatorNode::saw(Hz(997.0)), 512);
        let half = render(
            OscillatorNode::saw(Hz(997.0)).with_level(Amplitude(0.5)),
            512,
        );
        for (a, b) in full.iter().zip(&half) {
            assert_eq!(*b, a * 0.5);
        }
    }

    #[test]
    fn a_square_and_a_saw_have_no_dc_over_whole_periods() {
        for node in [
            OscillatorNode::square(Hz(1_000.0)),
            OscillatorNode::saw(Hz(1_000.0)),
        ] {
            let out = render(node, 48 * 20);
            assert!(mean(&out).abs() < 1e-6);
        }
    }

    #[test]
    fn a_square_peaks_at_plus_and_minus_one() {
        let out = render(OscillatorNode::square(Hz(1_000.0)), 48);
        let (hi, lo) = out
            .iter()
            .fold((f32::MIN, f32::MAX), |(h, l), &v| (h.max(v), l.min(v)));
        assert!((hi - 1.0).abs() < 1e-6 && (lo + 1.0).abs() < 1e-6);
        let high = out.iter().filter(|&&v| v > 0.5).count();
        assert_eq!(high, 23);
    }

    /// A 5 kHz saw at 48 kHz lies on 1 kHz lines, and 1 kHz is no harmonic of
    /// it: the naive saw's harmonics 19 and 29 fold onto it at `2/(19 pi)` and
    /// `2/(29 pi)`, each over 0.02; the band-limited one holds under 0.003.
    #[test]
    fn the_saw_does_not_alias_onto_a_non_harmonic_line() {
        let out = render(OscillatorNode::saw(Hz(5_000.0)), 48 * 100);
        assert!(amplitude_at(&out, 1_000.0, 48_000.0) < 0.003);
    }

    #[test]
    fn a_driven_frequency_changes_the_pitch_per_frame() {
        let mut d = Direct::new(OscillatorNode::sine(Hz(1_000.0)), RATE, 96);
        let lane: Vec<f32> = (0..96)
            .map(|i| if i < 48 { 1_000.0 } else { 2_000.0 })
            .collect();
        d.feed(0, Some(&lane));
        d.block();
        let out = d.output(0);
        // 1 kHz for 48 samples is one cycle, so the 2 kHz half starts at phase 0
        // and runs two cycles in the next 48.
        for (n, &y) in out.iter().enumerate().skip(48) {
            let exact =
                libm::sin(2.0 * core::f64::consts::PI * 2_000.0 * (n - 48) as f64 / 48_000.0);
            assert!((f64::from(y) - exact).abs() < 1e-6, "sample {n}");
        }
    }

    #[test]
    fn a_non_finite_driven_value_holds_the_last_finite_one() {
        let mut lane = vec![1_000.0f32; 16];
        lane[8] = f32::NAN;
        lane[9] = f32::INFINITY;
        let mut d = Direct::new(OscillatorNode::sine(Hz(500.0)), RATE, 16);
        d.feed(0, Some(&lane));
        d.block();
        for (n, &y) in d.output(0).iter().enumerate() {
            let exact = libm::sin(2.0 * core::f64::consts::PI * 1_000.0 * n as f64 / 48_000.0);
            assert!((f64::from(y) - exact).abs() < 1e-6, "sample {n}");
        }
        let mut level = Direct::new(OscillatorNode::sine(Hz(1_000.0)), RATE, 4);
        level.feed(1, Some(&[0.5, f32::NAN, f32::NEG_INFINITY, 0.5]));
        level.block();
        let held = level.output(0)[1];
        assert!(held.is_finite());
        assert!(level.output(0).iter().all(|v| v.is_finite()));
    }

    #[test]
    fn an_unprepared_node_renders_silence() {
        let mut node = OscillatorNode::sine(Hz(440.0));
        let ones = [1.0f32; 32];
        let out = drive(&mut node, RATE, &[], &[None, Some(&ones)]);
        assert!(out[0].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn reset_restarts_the_cycle() {
        let mut d = Direct::new(OscillatorNode::triangle(Hz(300.0)), RATE, 64);
        d.block();
        let first = d.output(0).to_vec();
        d.node.reset();
        d.block();
        assert_eq!(d.output(0), &first[..]);
    }

    #[test]
    fn a_set_frequency_is_heard_on_the_next_block() {
        let node = OscillatorNode::sine(Hz(1_000.0));
        let cell = node.frequency();
        let mut d = Direct::new(node, RATE, 48);
        d.block();
        cell.store(Hz(2_000.0));
        d.block();
        let out = d.output(0);
        // 1 kHz filled one cycle, so the block starts at phase 0 and holds two
        // cycles of 2 kHz.
        let exact = libm::sin(2.0 * core::f64::consts::PI * 2_000.0 * 12.0 / 48_000.0);
        assert!((f64::from(out[12]) - exact).abs() < 1e-6);
    }
}
