//! Test and stimulus nodes: sources that feed a graph a known signal, and the
//! plumbing (pass-through, fan-out, sink) a test graph is wired out of.
//!
//! These replace the fundsp one-liners — `dc`, `sine_hz`, `saw_hz`,
//! `square_hz`, `triangle`, `pass`, `multipass`, `split`, `sink` — that tests,
//! examples and benches used to build stimulus graphs with. Two things are
//! different, and both are the point:
//!
//! - **Widths are runtime.** Every node here takes a [`ChannelLayout`], where
//!   fundsp spelled a width as a `typenum` (`split::<U2>()`) or as an operator
//!   expression (`pass() | pass()`). A stereo tone is one node,
//!   `Osc::sine(Hz(440.0)).with_layout(ChannelLayout::STEREO)`, not
//!   `sine_hz(440.0) >> split::<U2>()`.
//! - **They are plain [`AudioUnit`]s.** There is no operator DSL here and no
//!   `An<X>` wrapper, so a test graph is built the same way a production one
//!   is: push nodes into a `Net` and wire them.
//!
//! # Not for production audio
//!
//! The oscillators are **naive** — not band-limited. A saw or square here
//! aliases above a few kHz, which a test wants (the samples are a closed-form
//! function of the phase, so an expected value can be written down) and a
//! synth does not. `tutti-polysynth` owns the audio-rate oscillators an
//! instrument plays.
//!
//! [`AudioUnit`]: tutti_core::AudioUnit

use std::f64::consts::TAU;

use tutti_core::{Amplitude, ChannelLayout, Hz, Phase, SampleRate, Tail};
use tutti_core::{AudioUnit, BufferMut, BufferRef, Signal, SignalFrame};

use crate::node_id::{TEST_CONST_ID, TEST_OSC_ID, TEST_SINK_ID, TEST_SPLIT_ID, TEST_THROUGH_ID};

/// A constant source: no inputs, one output per channel, each holding its
/// value forever.
///
/// The value is a raw **sample**, not a unit: a DC level can be negative, so it
/// is not an [`Amplitude`] (a gain, floored at zero), and it is multiplied onto
/// nothing, so it is not any other control either. It is the signal itself —
/// the thing the unit newtypes describe properties *of*.
#[derive(Clone, Debug)]
pub struct Const {
    values: Vec<f32>,
}

impl Const {
    /// `value` on every channel of `layout`.
    pub fn new(value: f32, layout: impl Into<ChannelLayout>) -> Self {
        let channels = layout.into().count() as usize;
        Self {
            values: vec![value; channels],
        }
    }

    /// A one-channel constant — fundsp's `dc(value)`.
    pub fn mono(value: f32) -> Self {
        Self::new(value, ChannelLayout::MONO)
    }

    /// One value per channel, so channels can be told apart — fundsp's
    /// `dc((a, b))`. The width is `values.len()`.
    pub fn frame(values: &[f32]) -> Self {
        Self {
            values: values.to_vec(),
        }
    }
}

impl AudioUnit for Const {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        self.values.len()
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[..self.values.len()].copy_from_slice(&self.values);
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        for (c, &v) in self.values.iter().enumerate() {
            output.channel_f32_mut(c)[..size].fill(v);
        }
    }

    /// A constant is *known*, which is more than a latency: [`Signal::Value`]
    /// lets a response probe fold it the way it folds fundsp's `dc`.
    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(self.values.len());
        for (c, &v) in self.values.iter().enumerate() {
            output.set(c, Signal::Value(f64::from(v)));
        }
        output
    }

    fn get_id(&self) -> u64 {
        TEST_CONST_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    /// Nothing drives a source, so there is nothing to drain: what it emits
    /// *is* the signal, and it stops when the render does. The convention
    /// fundsp's `dc` follows, and the one a graph's tail walk
    /// relies on — `Unbounded` here would make every graph with a stimulus in
    /// it unspendable.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// The closed-form shape an [`Osc`] evaluates at each phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waveform {
    /// `sin(2π·phase)` — starts at 0, rising.
    Sine,
    /// `2·phase − 1` — a rising ramp from −1, wrapping to −1 at each cycle.
    Saw,
    /// `+1` for the first half-cycle, `−1` for the second.
    Square,
    /// `1 − 4·|phase − ½|` — −1 at the cycle start, +1 at the half.
    Triangle,
}

impl Waveform {
    /// The waveform at `phase` in cycles, `0..1`.
    #[inline]
    fn at(self, phase: f64) -> f64 {
        match self {
            Self::Sine => (phase * TAU).sin(),
            Self::Saw => 2.0 * phase - 1.0,
            Self::Square => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Self::Triangle => 1.0 - 4.0 * (phase - 0.5).abs(),
        }
    }
}

/// A fixed-frequency oscillator: no inputs, the same signal on every channel
/// of its layout.
///
/// The phase starts at [`Phase::START`] unless [`with_phase`](Self::with_phase)
/// says otherwise, and is accumulated in `f64`, so a long render does not
/// drift. Unlike fundsp's `sine_hz`, the start phase is **not** seeded from the
/// graph's hash — a stimulus a test measures should start where the test says.
///
/// **Starts at the placeholder [`SampleRate::DEFAULT`]**; call
/// [`AudioUnit::set_sample_rate`] (a `Net` does, for every node it holds) or
/// the pitch is off by the rate ratio.
#[derive(Clone, Debug)]
pub struct Osc {
    waveform: Waveform,
    frequency: Hz,
    amplitude: Amplitude,
    layout: ChannelLayout,
    sample_rate: SampleRate,
    /// Where the cycle starts, and where `reset` returns it.
    start: Phase,
    /// Position in the cycle, `0..1`. `f64` rather than [`Phase`] because it
    /// integrates an increment every sample, and `Phase` is `f32`: over a
    /// minute at 48 kHz the `f32` sum drifts audibly.
    phase: f64,
}

impl Osc {
    /// A mono, unity-amplitude oscillator of `waveform` at `frequency`.
    pub fn new(waveform: Waveform, frequency: Hz) -> Self {
        Self {
            waveform,
            frequency,
            amplitude: Amplitude::UNITY,
            layout: ChannelLayout::MONO,
            sample_rate: SampleRate::DEFAULT,
            start: Phase::START,
            phase: 0.0,
        }
    }

    /// A sine — fundsp's `sine_hz`.
    pub fn sine(frequency: Hz) -> Self {
        Self::new(Waveform::Sine, frequency)
    }

    /// A naive saw — fundsp's `saw_hz`, without the band-limiting.
    pub fn saw(frequency: Hz) -> Self {
        Self::new(Waveform::Saw, frequency)
    }

    /// A naive square — fundsp's `square_hz`, without the band-limiting.
    pub fn square(frequency: Hz) -> Self {
        Self::new(Waveform::Square, frequency)
    }

    /// A naive triangle — fundsp's `triangle_hz`, without the band-limiting.
    pub fn triangle(frequency: Hz) -> Self {
        Self::new(Waveform::Triangle, frequency)
    }

    /// Scale the output by `amplitude` — fundsp's `sine_hz(f) * a`.
    pub fn with_amplitude(mut self, amplitude: Amplitude) -> Self {
        self.amplitude = amplitude;
        self
    }

    /// Start the cycle at `phase` rather than 0 — fundsp's
    /// `sine_hz(f)` with `Setting::phase`. A sine sampled off its peaks is how
    /// a test gets a true peak that falls *between* samples.
    ///
    /// `phase` is in turns and is wrapped into `[0, 1)` here, so `1.25` and
    /// `-0.75` both start a quarter-cycle in. `Phase` is a bare `pub` newtype,
    /// so an unwrapped value is constructible, and the closed forms in
    /// [`Waveform`] assume `0..1` — a saw started at `1.25` would emit `1.5`,
    /// off the end of its range, until the first wrap.
    pub fn with_phase(mut self, phase: Phase) -> Self {
        let wrapped = Phase::wrapped(phase.get());
        // `rem_euclid` in f32 can round a tiny negative up to exactly 1.0,
        // which is outside the half-open range; that is the start of a cycle.
        self.start = if wrapped.get() >= 1.0 {
            Phase::START
        } else {
            wrapped
        };
        self.phase = f64::from(self.start.get());
        self
    }

    /// Put the signal on every channel of `layout` — fundsp's
    /// `sine_hz(f) >> split::<N>()`, or `sine_hz(f) | sine_hz(f)`.
    pub fn with_layout(mut self, layout: impl Into<ChannelLayout>) -> Self {
        self.layout = layout.into();
        self
    }

    /// The next sample, advancing the phase.
    #[inline]
    fn next(&mut self) -> f32 {
        let y = self.waveform.at(self.phase) * f64::from(self.amplitude.get());
        self.phase += f64::from(self.frequency.get()) / self.sample_rate.get();
        self.phase -= self.phase.floor();
        y as f32
    }
}

impl AudioUnit for Osc {
    fn reset(&mut self) {
        self.phase = f64::from(self.start.get());
    }

    fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        self.sample_rate = sample_rate;
    }

    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        self.layout.count() as usize
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        let y = self.next();
        output[..self.outputs()].fill(y);
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        let channels = self.outputs();
        for i in 0..size {
            let y = self.next();
            for c in 0..channels {
                output.set_f32(c, i, y);
            }
        }
    }

    /// A generator: every output is available at zero latency.
    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(self.outputs());
        output.fill(Signal::Latency(0.0));
        output
    }

    fn get_id(&self) -> u64 {
        TEST_OSC_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    /// Nothing drives a source, so there is nothing to drain: what it emits
    /// *is* the signal, and it stops when the render does. The convention
    /// fundsp's `dc` follows, and the one a graph's tail walk
    /// relies on — `Unbounded` here would make every graph with a stimulus in
    /// it unspendable.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// `N` inputs copied unchanged to `N` outputs — fundsp's `pass()` (mono) and
/// `multipass::<N>()` / `pass() | pass()` (wider).
///
/// The node a test uses where it needs *a* node of a given width and no
/// processing: a sink port to declare wiring into, a stand-in for a track.
#[derive(Clone, Debug)]
pub struct Through {
    layout: ChannelLayout,
}

impl Through {
    /// A pass-through `layout` wide.
    pub fn new(layout: impl Into<ChannelLayout>) -> Self {
        Self {
            layout: layout.into(),
        }
    }

    /// A one-channel pass-through — fundsp's `pass()`.
    pub fn mono() -> Self {
        Self::new(ChannelLayout::MONO)
    }
}

impl AudioUnit for Through {
    fn inputs(&self) -> usize {
        self.layout.count() as usize
    }

    fn outputs(&self) -> usize {
        self.layout.count() as usize
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let n = self.outputs();
        output[..n].copy_from_slice(&input[..n]);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        for c in 0..self.outputs() {
            output.channel_f32_mut(c)[..size].copy_from_slice(&input.channel_f32(c)[..size]);
        }
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        input.clone()
    }

    fn get_id(&self) -> u64 {
        TEST_THROUGH_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    /// A copy stops with its input.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// One input copied to every channel of `layout` — fundsp's `split::<N>()`.
#[derive(Clone, Debug)]
pub struct Split {
    layout: ChannelLayout,
}

impl Split {
    /// A fan-out from one input to `layout`'s channels.
    pub fn new(layout: impl Into<ChannelLayout>) -> Self {
        Self {
            layout: layout.into(),
        }
    }
}

impl AudioUnit for Split {
    fn inputs(&self) -> usize {
        1
    }

    fn outputs(&self) -> usize {
        self.layout.count() as usize
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let n = self.outputs();
        output[..n].fill(input[0]);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let source = &input.channel_f32(0)[..size];
        for c in 0..self.outputs() {
            output.channel_f32_mut(c)[..size].copy_from_slice(source);
        }
    }

    /// Every output carries exactly what the one input does.
    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(self.outputs());
        output.fill(input.at(0));
        output
    }

    fn get_id(&self) -> u64 {
        TEST_SPLIT_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    /// A copy stops with its input.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// `N` inputs, no outputs: consumes whatever is wired into it — fundsp's
/// `sink()`, at a runtime width.
#[derive(Clone, Debug)]
pub struct Sink {
    layout: ChannelLayout,
}

impl Sink {
    /// A sink `layout` wide.
    pub fn new(layout: impl Into<ChannelLayout>) -> Self {
        Self {
            layout: layout.into(),
        }
    }

    /// A one-channel sink — fundsp's `sink()`.
    pub fn mono() -> Self {
        Self::new(ChannelLayout::MONO)
    }
}

impl AudioUnit for Sink {
    fn inputs(&self) -> usize {
        self.layout.count() as usize
    }

    fn outputs(&self) -> usize {
        0
    }

    fn tick(&mut self, _input: &[f32], _output: &mut [f32]) {}

    fn process(&mut self, _size: usize, _input: &BufferRef, _output: &mut BufferMut) {}

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(0)
    }

    fn get_id(&self) -> u64 {
        TEST_SINK_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// Render one `process` call of `node` over `inputs` (one slice per input,
/// at most `MAX_BUFFER_SIZE` frames), with each of its
/// [`ParamFeed`](tutti_core::ParamFeed)'s params fed `params[k]` when `Some`
/// and cleared when `None` — what the graph's `Legacy` adapter hands a unit
/// whose params it modulates (design doc 013 item 6). Returns one `Vec` per
/// output.
///
/// # Panics
///
/// If `node` has no feed, `params` is not one entry per feed param, or a
/// slice is longer than the block.
pub fn process_fed(
    node: &mut dyn AudioUnit,
    inputs: &[&[f32]],
    params: &[Option<&[f32]>],
) -> Vec<Vec<f32>> {
    let n = inputs.first().map_or_else(
        || params.iter().flatten().next().map_or(0, |p| p.len()),
        |i| i.len(),
    );
    feed(node, params, 0, n);
    let mut ib = tutti_core::BufferVec::new(node.inputs());
    let mut ob = tutti_core::BufferVec::new(node.outputs());
    for (c, sig) in inputs.iter().enumerate() {
        for (i, &x) in sig.iter().enumerate() {
            ib.set_f32(c, i, x);
        }
    }
    node.process(n, &ib.buffer_ref(), &mut ob.buffer_mut());
    (0..node.outputs())
        .map(|c| (0..n).map(|i| ob.at_f32(c, i)).collect())
        .collect()
}

/// [`process_fed`] one frame at a time through `tick`: before each frame the
/// feed holds frame `i` of every `Some` param. A unit reads a live feed's
/// first value in `tick`.
pub fn tick_fed(
    node: &mut dyn AudioUnit,
    inputs: &[&[f32]],
    params: &[Option<&[f32]>],
) -> Vec<Vec<f32>> {
    let n = inputs.first().map_or_else(
        || params.iter().flatten().next().map_or(0, |p| p.len()),
        |i| i.len(),
    );
    let mut out = vec![vec![0.0f32; n]; node.outputs()];
    let mut fi = vec![0.0f32; node.inputs()];
    let mut fo = vec![0.0f32; node.outputs()];
    for i in 0..n {
        feed(node, params, i, 1);
        for (c, sig) in inputs.iter().enumerate() {
            fi[c] = sig[i];
        }
        node.tick(&fi, &mut fo);
        for (c, o) in out.iter_mut().enumerate() {
            o[i] = fo[c];
        }
    }
    out
}

/// Feed `params[k][start..start + len]` (or clear it) into `node`'s feed.
fn feed(node: &mut dyn AudioUnit, params: &[Option<&[f32]>], start: usize, len: usize) {
    let f = node.param_feed().expect("the unit has a param feed");
    assert_eq!(params.len(), f.params().len(), "one entry per feed param");
    for (k, p) in params.iter().enumerate() {
        match p {
            Some(v) => f.feed(k, &v[start..start + len]),
            None => f.clear(k),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::BufferVec;

    /// Render `size` frames of a source through `process`.
    fn render(unit: &mut dyn AudioUnit, size: usize) -> BufferVec {
        let input = BufferVec::new(unit.inputs());
        let mut output = BufferVec::new(unit.outputs());
        unit.process(size, &input.buffer_ref(), &mut output.buffer_mut());
        output
    }

    /// The oscillator's samples are the closed form of its phase, on every
    /// channel, and `tick` and `process` agree sample for sample.
    ///
    /// Mutation: dropping the `phase -= floor` wrap leaves the sine intact (it
    /// is periodic) but walks the saw off past +1 — the saw row fails. Filling
    /// only channel 0 in `process` fails the per-channel assertion.
    #[test]
    fn oscillator_samples_are_the_closed_form_of_the_phase() {
        let sr = SampleRate(8_000.0);
        // 1 kHz at 8 kHz: exactly 8 samples per cycle, so the phases are
        // k/8 and every expected value is exact.
        for (osc, expect) in [
            (
                Osc::sine(Hz(1_000.0)),
                (0..16)
                    .map(|k| ((k % 8) as f64 / 8.0 * TAU).sin() as f32)
                    .collect::<Vec<_>>(),
            ),
            (
                Osc::saw(Hz(1_000.0)),
                (0..16).map(|k| 2.0 * (k % 8) as f32 / 8.0 - 1.0).collect(),
            ),
            (
                Osc::square(Hz(1_000.0)),
                (0..16)
                    .map(|k| if k % 8 < 4 { 1.0 } else { -1.0 })
                    .collect(),
            ),
            (
                Osc::triangle(Hz(1_000.0)),
                [-1.0, -0.5, 0.0, 0.5, 1.0, 0.5, 0.0, -0.5].repeat(2),
            ),
        ] {
            let mut ticked = osc.clone().with_layout(ChannelLayout::STEREO);
            ticked.set_sample_rate(sr);
            let mut blocked = ticked.clone();

            let out = render(&mut blocked, 16);
            for (k, &e) in expect.iter().enumerate() {
                let mut frame = [0.0f32; 2];
                ticked.tick(&[], &mut frame);
                assert!(
                    (frame[0] - e).abs() < 1e-6 && frame[1] == frame[0],
                    "{:?} tick {k}: {frame:?}, expected {e}",
                    osc.waveform
                );
                for c in 0..2 {
                    assert!(
                        (out.at_f32(c, k) - e).abs() < 1e-6,
                        "{:?} process ch{c} frame {k}: {}, expected {e}",
                        osc.waveform,
                        out.at_f32(c, k)
                    );
                }
            }
        }
    }

    /// Amplitude scales, the sample rate sets the pitch, and `reset` returns
    /// the phase to its start.
    ///
    /// Mutation: ignoring `set_sample_rate` leaves the rate at the 44.1 kHz
    /// placeholder, so sample 2 is `sin(2π·2000/44100)` rather than `sin(π/2)`;
    /// making `reset` a no-op fails the replay.
    #[test]
    fn amplitude_rate_and_reset_are_honoured() {
        let mut osc = Osc::sine(Hz(1_000.0)).with_amplitude(Amplitude(0.25));
        osc.set_sample_rate(SampleRate(4_000.0));
        let first = render(&mut osc, 4);
        // Quarter-cycle steps: 0, +peak, 0, −peak.
        let expect = [0.0, 0.25, 0.0, -0.25];
        for (k, &e) in expect.iter().enumerate() {
            assert!((first.at_f32(0, k) - e).abs() < 1e-6, "frame {k}");
        }
        osc.reset();
        let replay = render(&mut osc, 4);
        for k in 0..4 {
            assert_eq!(replay.at_f32(0, k), first.at_f32(0, k), "frame {k}");
        }
    }

    /// Plumbing widths come from the layout, and each node moves the samples
    /// it claims to.
    ///
    /// Mutation: `Split` copying input channel `c` instead of 0 reads past its
    /// single input (panics); `Through` skipping channel 1 leaves it zero;
    /// `Const::frame` broadcasting `values[0]` fails the channel-1 value.
    #[test]
    fn plumbing_moves_the_samples_it_claims() {
        let mut dc = Const::frame(&[0.5, -0.25]);
        assert_eq!((dc.inputs(), dc.outputs()), (0, 2));
        let out = render(&mut dc, 8);
        assert_eq!((out.at_f32(0, 7), out.at_f32(1, 7)), (0.5, -0.25));

        let mut input = BufferVec::new(2);
        for i in 0..8 {
            input.set_f32(0, i, i as f32);
            input.set_f32(1, i, -(i as f32));
        }

        let mut through = Through::new(ChannelLayout::STEREO);
        assert_eq!((through.inputs(), through.outputs()), (2, 2));
        let mut out = BufferVec::new(2);
        through.process(8, &input.buffer_ref(), &mut out.buffer_mut());
        for i in 0..8 {
            assert_eq!(
                (out.at_f32(0, i), out.at_f32(1, i)),
                (i as f32, -(i as f32))
            );
        }

        let mut split = Split::new(ChannelLayout::QUAD);
        assert_eq!((split.inputs(), split.outputs()), (1, 4));
        let mut out = BufferVec::new(4);
        split.process(8, &input.buffer_ref(), &mut out.buffer_mut());
        for c in 0..4 {
            assert_eq!(out.at_f32(c, 5), 5.0, "split channel {c}");
        }

        let sink = Sink::new(ChannelLayout::from(6u16));
        assert_eq!((sink.inputs(), sink.outputs()), (6, 0));
    }

    /// The start phase is wrapped into `[0, 1)`, both when set and after
    /// `reset`: `1.25` and `-0.75` start where `0.25` does.
    ///
    /// A saw shows it, because its closed form is only a ramp on `0..1`: from
    /// an unwrapped `1.25` it would emit `2·1.25 − 1 = 1.5`.
    ///
    /// Mutation: skipping `Phase::wrapped` fails the `1.25` row (the guard then
    /// sends it to 0, so the saw emits −1.0; with no guard either it emits 1.5);
    /// relaxing the guard to `> 1.0` makes the `-1e-9` start emit +1.0 rather
    /// than −1.0.
    #[test]
    fn with_phase_wraps_into_one_cycle_and_reset_restores_it() {
        let quarter = 2.0 * 0.25 - 1.0; // the saw's value at 0.25 turns
        for start in [0.25f32, 1.25, -0.75, 3.25] {
            let mut osc = Osc::saw(Hz(1_000.0)).with_phase(Phase(start));
            osc.set_sample_rate(SampleRate(8_000.0));
            let first = render(&mut osc, 3).at_f32(0, 0);
            assert!((first - quarter).abs() < 1e-6, "start {start}: {first}");
            osc.reset();
            let again = render(&mut osc, 1).at_f32(0, 0);
            assert!((again - quarter).abs() < 1e-6, "reset {start}: {again}");
        }
        // The f32 edge: a tiny negative rounds to 1.0 in `rem_euclid`.
        let mut edge = Osc::saw(Hz(1_000.0)).with_phase(Phase(-1e-9));
        assert_eq!(render(&mut edge, 1).at_f32(0, 0), -1.0);
    }
}
