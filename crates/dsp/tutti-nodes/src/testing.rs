//! Test and stimulus nodes: sources that feed a graph a known signal, and the
//! plumbing (pass-through, fan-out, sink) a test graph is wired out of.
//!
//! Available with the `testing` feature; enable it from `[dev-dependencies]`.
//!
//! - [`Const`] and [`Osc`] are sources: a constant, or a naive sine, saw,
//!   square or triangle ([`Waveform`]).
//! - [`Through`], [`Split`] and [`Sink`] are plumbing: pass-through, fan-out
//!   from one input, and a sink with no outputs.
//!
//! Widths are runtime: every node takes a [`ChannelLayout`], so a stereo tone
//! is one node, `Osc::sine(Hz(440.0)).with_layout(ChannelLayout::STEREO)`.
//! Each is a plain graph [`Node`] and its own [`IntoNode`], forkable by clone
//! (none shares state with its clones), so a test graph is built the same way
//! a production one is: `GraphBuilder::add(Osc::sine(..))`.
//!
//! # Examples
//!
//! ```
//! use tutti_core::{ChannelLayout, Hz, SampleRate, Samples};
//! use tutti_graph::{GraphBuilder, Prepare};
//! use tutti_nodes::testing::Osc;
//!
//! let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
//! let tone = g.add(Osc::sine(Hz(440.0)).with_layout(ChannelLayout::STEREO));
//! g.pipe_output(tone);
//! let mut r = g
//!     .renderer(Prepare::new(SampleRate(48_000.0), Samples(64)))
//!     .expect("builds");
//! let out = r.render(64);
//! assert_eq!(out.len(), 2);
//! ```
//!
//! # Not for production audio
//!
//! The oscillators are **naive** — not band-limited. A saw or square here
//! aliases above a few kHz, which a test wants (the samples are a closed-form
//! function of the phase, so an expected value can be written down) and a
//! synth does not. `tutti-polysynth` owns the audio-rate oscillators an
//! instrument plays.

use std::f64::consts::TAU;

use tutti_core::{Amplitude, ChannelLayout, Hz, Phase, SampleRate};
use tutti_graph::{Cx, ForkByClone, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};

/// Each stimulus node is its own [`IntoNode`]: forkable by clone, as none
/// shares state with its clones.
macro_rules! forks_by_clone {
    ($($t:ty),*) => {$(
        impl IntoNode for $t {
            type Controls = ();
            fn into_parts(self) -> NodeParts<()> {
                ForkByClone(self).into_parts()
            }
        }
    )*};
}

forks_by_clone!(Const, Osc, Through, Split, Sink);

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
    /// Creates a constant `value` on every channel of `layout`.
    pub fn new(value: f32, layout: impl Into<ChannelLayout>) -> Self {
        let channels = layout.into().count() as usize;
        Self {
            values: vec![value; channels],
        }
    }

    /// Creates a one-channel constant.
    pub fn mono(value: f32) -> Self {
        Self::new(value, ChannelLayout::MONO)
    }

    /// Creates a constant with one value per channel, so channels can be told
    /// apart. The width is `values.len()`.
    pub fn frame(values: &[f32]) -> Self {
        Self {
            values: values.to_vec(),
        }
    }
}

impl Node for Const {
    /// Nothing drives a source, so there is nothing to drain: what it emits
    /// *is* the signal, and it stops when the render does — `Tail::None`, the
    /// convention a graph's tail walk relies on (`Unbounded` here would make
    /// every graph with a stimulus in it unspendable).
    fn shape(&self) -> Shape {
        Shape::audio(
            ChannelLayout::EMPTY,
            ChannelLayout::from_count(self.values.len() as u16),
        )
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for (c, &v) in self.values.iter().enumerate() {
            io.output(c).fill(v);
        }
        Status::Modified
    }

    fn reset(&mut self) {}
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
/// drift. The start phase is deterministic: a stimulus a test measures should
/// start where the test says.
///
/// **Starts at the placeholder [`SampleRate::DEFAULT`]** until it is
/// prepared ([`Node::prepare`], which a graph does at insert).
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
    /// Creates a mono, unity-amplitude oscillator of `waveform` at `frequency`.
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

    /// Creates a sine oscillator.
    pub fn sine(frequency: Hz) -> Self {
        Self::new(Waveform::Sine, frequency)
    }

    /// Creates a naive (not band-limited) saw oscillator.
    pub fn saw(frequency: Hz) -> Self {
        Self::new(Waveform::Saw, frequency)
    }

    /// Creates a naive (not band-limited) square oscillator.
    pub fn square(frequency: Hz) -> Self {
        Self::new(Waveform::Square, frequency)
    }

    /// Creates a naive (not band-limited) triangle oscillator.
    pub fn triangle(frequency: Hz) -> Self {
        Self::new(Waveform::Triangle, frequency)
    }

    /// Scales the output by `amplitude`.
    pub fn with_amplitude(mut self, amplitude: Amplitude) -> Self {
        self.amplitude = amplitude;
        self
    }

    /// Starts the cycle at `phase` rather than 0. A sine sampled off its peaks is how
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

    /// Puts the signal on every channel of `layout`.
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

impl Node for Osc {
    /// A generator at zero latency; `Tail::None`, as [`Const`]'s.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, self.layout)
    }

    fn prepare(&mut self, prepare: &Prepare) {
        self.sample_rate = prepare.sample_rate();
    }

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let channels = usize::from(self.layout.count());
        for i in 0..io.frames() {
            let y = self.next();
            for c in 0..channels {
                io.output(c)[i] = y;
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.phase = f64::from(self.start.get());
    }
}

/// A pass-through node: `N` inputs copied unchanged to `N` outputs.
///
/// The node a test uses where it needs *a* node of a given width and no
/// processing: a sink port to declare wiring into, a stand-in for a track.
#[derive(Clone, Debug)]
pub struct Through {
    layout: ChannelLayout,
}

impl Through {
    /// Creates a pass-through `layout` wide.
    pub fn new(layout: impl Into<ChannelLayout>) -> Self {
        Self {
            layout: layout.into(),
        }
    }

    /// Creates a one-channel pass-through.
    pub fn mono() -> Self {
        Self::new(ChannelLayout::MONO)
    }
}

impl Node for Through {
    /// A copy stops with its input: `Tail::None`.
    fn shape(&self) -> Shape {
        Shape::audio(self.layout, self.layout)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for c in 0..usize::from(self.layout.count()) {
            outs.get(c).copy_from_slice(ins.get(c));
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

/// A fan-out node: one input copied to every channel of `layout`.
#[derive(Clone, Debug)]
pub struct Split {
    layout: ChannelLayout,
}

impl Split {
    /// Creates a fan-out from one input to `layout`'s channels.
    pub fn new(layout: impl Into<ChannelLayout>) -> Self {
        Self {
            layout: layout.into(),
        }
    }
}

impl Node for Split {
    /// A copy stops with its input: `Tail::None`.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, self.layout)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for c in 0..usize::from(self.layout.count()) {
            outs.get(c).copy_from_slice(ins.get(0));
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

/// A sink node: `N` inputs, no outputs; consumes whatever is wired into it.
#[derive(Clone, Debug)]
pub struct Sink {
    layout: ChannelLayout,
}

impl Sink {
    /// Creates a sink `layout` wide.
    pub fn new(layout: impl Into<ChannelLayout>) -> Self {
        Self {
            layout: layout.into(),
        }
    }

    /// Creates a one-channel sink.
    pub fn mono() -> Self {
        Self::new(ChannelLayout::MONO)
    }
}

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::audio(self.layout, ChannelLayout::EMPTY)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status {
        Status::Modified
    }

    fn reset(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::Samples;
    use tutti_graph::Solo;

    /// `node` alone in a graph at `rate`, in blocks of `block` frames.
    fn solo<N: IntoNode<Controls = ()>>(node: N, rate: SampleRate, block: usize) -> Solo<()> {
        let mut solo = Solo::new(node, Prepare::new(rate, Samples(64)));
        solo.renderer_mut().set_block(Samples(block));
        solo
    }

    /// The oscillator's samples are the closed form of its phase, on every
    /// channel, and a render in blocks of one frame agrees with a render in
    /// one block, sample for sample.
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
            let stereo = osc.clone().with_layout(ChannelLayout::STEREO);
            let ticked = solo(stereo.clone(), sr, 1).render(16);
            let blocked = solo(stereo, sr, 16).render(16);
            for (k, &e) in expect.iter().enumerate() {
                let frame = [ticked[0][k], ticked[1][k]];
                assert!(
                    (frame[0] - e).abs() < 1e-6 && frame[1] == frame[0],
                    "{:?} block of one, frame {k}: {frame:?}, expected {e}",
                    osc.waveform
                );
                for (c, ch) in blocked.iter().enumerate() {
                    assert!(
                        (ch[k] - e).abs() < 1e-6,
                        "{:?} process ch{c} frame {k}: {}, expected {e}",
                        osc.waveform,
                        ch[k]
                    );
                }
            }
        }
    }

    /// Amplitude scales, the sample rate sets the pitch, and `reset` returns
    /// the phase to its start.
    ///
    /// Mutation: ignoring the rate in `prepare` leaves it at the 44.1 kHz
    /// placeholder, so sample 2 is `sin(2π·2000/44100)` rather than `sin(π/2)`;
    /// making `reset` a no-op fails the replay.
    #[test]
    fn amplitude_rate_and_reset_are_honoured() {
        let mut osc = Osc::sine(Hz(1_000.0)).with_amplitude(Amplitude(0.25));
        osc.prepare(&Prepare::new(SampleRate(4_000.0), Samples(64)));
        let first: Vec<f32> = (0..4).map(|_| osc.next()).collect();
        // Quarter-cycle steps: 0, +peak, 0, −peak.
        let expect = [0.0, 0.25, 0.0, -0.25];
        for (k, &e) in expect.iter().enumerate() {
            assert!((first[k] - e).abs() < 1e-6, "frame {k}");
        }
        let _ = osc.next(); // off the cycle's start
        Node::reset(&mut osc);
        let replay: Vec<f32> = (0..4).map(|_| osc.next()).collect();
        assert_eq!(replay, first);
    }

    /// Plumbing widths come from the layout, and each node moves the samples
    /// it claims to.
    ///
    /// Mutation: `Split` copying input channel `c` instead of 0 reads past its
    /// single input (panics); `Through` skipping channel 1 leaves it zero;
    /// `Const::frame` broadcasting `values[0]` fails the channel-1 value.
    #[test]
    fn plumbing_moves_the_samples_it_claims() {
        let sr = SampleRate(48_000.0);
        let width = |s: Shape| (s.audio_in.count(), s.audio_out.count());
        let dc = Const::frame(&[0.5, -0.25]);
        assert_eq!(width(dc.shape()), (0, 2));
        let out = solo(dc, sr, 8).render(8);
        assert_eq!((out[0][7], out[1][7]), (0.5, -0.25));

        let ramp: Vec<f32> = (0..8).map(|i| i as f32).collect();
        let neg: Vec<f32> = ramp.iter().map(|x| -x).collect();

        let through = Through::new(ChannelLayout::STEREO);
        assert_eq!(width(through.shape()), (2, 2));
        let out = solo(through, sr, 8).render_input(&[&ramp, &neg]);
        assert_eq!(out, vec![ramp.clone(), neg]);

        let split = Split::new(ChannelLayout::QUAD);
        assert_eq!(width(split.shape()), (1, 4));
        let out = solo(split, sr, 8).render_input(&[&ramp]);
        for (c, ch) in out.iter().enumerate() {
            assert_eq!(ch[5], 5.0, "split channel {c}");
        }

        assert_eq!(width(Sink::new(ChannelLayout::from(6u16)).shape()), (6, 0));
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
            osc.prepare(&Prepare::new(SampleRate(8_000.0), Samples(64)));
            let first = osc.next();
            assert!((first - quarter).abs() < 1e-6, "start {start}: {first}");
            let _ = osc.next();
            Node::reset(&mut osc);
            let again = osc.next();
            assert!((again - quarter).abs() < 1e-6, "reset {start}: {again}");
        }
        // The f32 edge: a tiny negative rounds to 1.0 in `rem_euclid`.
        let mut edge = Osc::saw(Hz(1_000.0)).with_phase(Phase(-1e-9));
        assert_eq!(edge.next(), -1.0);
    }
}
