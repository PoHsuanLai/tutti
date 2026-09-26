//! The dynamical `AudioUnit` abstraction and utilities.
//!
//! [`AudioUnit`] itself moved down into [`tutti_node::audiounit`] — it is the
//! node contract, and the point of that crate is that a node can be written
//! without depending on this one. It is re-exported here, so
//! `fundsp_tutti::audiounit::AudioUnit` and every prelude keep resolving.
//!
//! What stayed is the set of implementations, each of which names something the
//! contract deliberately does not:
//!
//! - [`An<X>`](crate::combinator::An) — the bridge from the static-arity
//!   [`AudioNode`](crate::audionode::AudioNode) world to the dynamic one. This
//!   is how every fundsp built-in reaches the trait.
//! - [`Unit`] — the same bridge in the other direction, wrapping a
//!   `Box<dyn AudioUnit>` back into an `AudioNode` of declared arity.
//! - [`BigBlockAdapter`] / [`BlockRateAdapter`] — block-size adapters.
//! - [`DummyUnit`] — the silent placeholder [`Net`](crate::net::Net) uses.

use super::audionode::*;
use super::buffer::*;
use super::combinator::*;
use super::math::*;
use super::setting::*;
use super::signal::*;
use super::*;
use core::marker::PhantomData;
use tutti_types::Tail;
extern crate alloc;
use alloc::boxed::Box;

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use num_complex::Complex64;
pub use tutti_node::audiounit::AudioUnit;
use tutti_node::num::{F32, Num, Sample};

/// The convenience methods that used to be defaulted on [`AudioUnit`] itself —
/// `get_mono`, `get_stereo`, `filter_mono`, `filter_stereo`, `response`,
/// `response_db`, `display` — derived from `tick`/`route`, and blanket
/// implemented for every unit (including `dyn AudioUnit` and [`Net`]).
///
/// They left the trait in tutti's design doc 013, Phase 0: no engine code
/// called them, and a defaulted method on the node contract is surface every
/// engine node carries. This fork's tests, examples and `Wave::resample_fir`
/// still use them, so they live here, in the preludes, and nowhere else.
/// `An<X>`'s inherent methods of the same names take precedence on an `An`.
pub trait AudioUnitExt<S: Sample = F32>: AudioUnit<S> {
    /// Retrieve the next mono sample from a generator.
    /// The node must have no inputs and 1 or 2 outputs.
    /// If there are two outputs, average them.
    ///
    /// ### Example
    /// ```ignore
    /// use fundsp_tutti::prelude64::*;
    /// assert_eq!(dc(2.0).get_mono(), 2.0);
    /// assert_eq!(dc((3.0, 4.0)).get_mono(), 3.5);
    /// ```
    #[inline]
    fn get_mono(&mut self) -> S::Scalar {
        debug_assert!(self.inputs() == 0);
        match self.outputs() {
            1 => {
                let mut output = [S::scalar_zero()];
                self.tick(&[], &mut output);
                output[0]
            }
            2 => {
                let mut output = [S::scalar_zero(); 2];
                self.tick(&[], &mut output);
                (output[0] + output[1]) * S::Scalar::from_f64(0.5)
            }
            _ => panic!("AudioUnit::get_mono(): Unit must have 1 or 2 outputs"),
        }
    }

    /// Retrieve the next stereo sample (left, right) from a generator.
    /// The node must have no inputs and 1 or 2 outputs.
    /// If there is just one output, duplicate it.
    ///
    /// ### Example
    /// ```ignore
    /// use fundsp_tutti::prelude64::*;
    /// assert_eq!(dc((5.0, 6.0)).get_stereo(), (5.0, 6.0));
    /// assert_eq!(dc(7.0).get_stereo(), (7.0, 7.0));
    /// ```
    #[inline]
    fn get_stereo(&mut self) -> (S::Scalar, S::Scalar) {
        debug_assert!(self.inputs() == 0);
        match self.outputs() {
            1 => {
                let mut output = [S::scalar_zero()];
                self.tick(&[], &mut output);
                (output[0], output[0])
            }
            2 => {
                let mut output = [S::scalar_zero(); 2];
                self.tick(&[], &mut output);
                (output[0], output[1])
            }
            _ => panic!("AudioUnit::get_stereo(): Unit must have 1 or 2 outputs"),
        }
    }

    /// Filter the next mono sample `x`.
    /// The node must have exactly 1 input and 1 output.
    ///
    /// ### Example
    /// ```ignore
    /// use fundsp_tutti::prelude64::*;
    /// assert_eq!(add(4.0).filter_mono(5.0), 9.0);
    /// ```
    #[inline]
    fn filter_mono(&mut self, x: S::Scalar) -> S::Scalar {
        debug_assert!(self.inputs() == 1 && self.outputs() == 1);
        let mut output = [S::scalar_zero()];
        self.tick(&[x], &mut output);
        output[0]
    }

    /// Filter the next stereo sample `(x, y)`.
    /// The node must have exactly 2 inputs and 2 outputs.
    ///
    /// ### Example
    /// ```ignore
    /// use fundsp_tutti::prelude64::*;
    /// assert_eq!(add((2.0, 3.0)).filter_stereo(4.0, 5.0), (6.0, 8.0));
    /// ```
    #[inline]
    fn filter_stereo(&mut self, x: S::Scalar, y: S::Scalar) -> (S::Scalar, S::Scalar) {
        debug_assert!(self.inputs() == 2 && self.outputs() == 2);
        let mut output = [S::scalar_zero(); 2];
        self.tick(&[x, y], &mut output);
        (output[0], output[1])
    }

    /// Evaluate frequency response of `output` at `frequency` Hz.
    /// Any linear response can be composed.
    /// Return `None` if there is no response or it could not be calculated.
    ///
    /// ### Example
    /// ```ignore
    /// use fundsp_tutti::prelude64::*;
    /// assert_eq!(pass().response(0, 440.0), Some(Complex64::new(1.0, 0.0)));
    /// ```
    fn response(&mut self, output: usize, frequency: f64) -> Option<Complex64> {
        assert!(output < self.outputs());
        let mut input = SignalFrame::new(self.inputs());
        for i in 0..self.inputs() {
            input.set(i, Signal::Response(Complex64::new(1.0, 0.0), 0.0));
        }
        let response = self.route(&input, frequency);
        match response.at(output) {
            Signal::Response(rx, _) => Some(rx),
            _ => None,
        }
    }

    /// Evaluate frequency response of `output` in dB at `frequency` Hz.
    /// Any linear response can be composed.
    /// Return `None` if there is no response or it could not be calculated.
    ///
    /// ### Example
    /// ```ignore
    /// use fundsp_tutti::prelude64::*;
    /// let db = pass().response_db(0, 440.0).unwrap();
    /// assert!(db < 1.0e-7 && db > -1.0e-7);
    /// ```
    fn response_db(&mut self, output: usize, frequency: f64) -> Option<f64> {
        assert!(output < self.outputs());
        self.response(output, frequency).map(|r| amp_db(r.norm()))
    }

    /// Print information about this unit into a string.
    fn display(&mut self) -> String {
        let mut string = String::new();

        if self.inputs() > 0 && self.outputs() > 0 && self.response(0, 440.0).is_some() {
            let scope = [
                b"------------------------------------------------",
                b"                                                ",
                b"------------------------------------------------",
                b"                                                ",
                b"------------------------------------------------",
                b"                                                ",
                b"------------------------------------------------",
                b"                                                ",
                b"------------------------------------------------",
                b"                                                ",
                b"------------------------------------------------",
                b"                                                ",
                b"------------------------------------------------",
            ];

            let mut scope: Vec<_> = scope.iter().map(|x| x.to_vec()).collect();

            let f: [f64; 48] = [
                10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 90.0, 100.0, 120.0, 140.0, 160.0,
                180.0, 200.0, 250.0, 300.0, 350.0, 400.0, 450.0, 500.0, 600.0, 700.0, 800.0, 900.0,
                1000.0, 1200.0, 1400.0, 1600.0, 1800.0, 2000.0, 2500.0, 3000.0, 3500.0, 4000.0,
                4500.0, 5000.0, 6000.0, 7000.0, 8000.0, 9000.0, 10000.0, 12000.0, 14000.0, 16000.0,
                18000.0, 20000.0, 22000.0,
            ];

            let r: Vec<_> = f
                .iter()
                .map(|&f| (self.response_db(0, f).unwrap(), f))
                .collect();

            let epsilon_db = 1.0e-2;
            let max_r = r.iter().fold((-f64::INFINITY, None), {
                |acc, &x| {
                    if abs(acc.0 - x.0) <= epsilon_db {
                        (max(acc.0, x.0), None)
                    } else if acc.0 > x.0 {
                        acc
                    } else {
                        (x.0, Some(x.1))
                    }
                }
            });
            let max_db = ceil(max_r.0 / 10.0) * 10.0;

            for i in 0..f.len() {
                let row = (max_db - r[i].0) / 5.0;
                let mut j = ceil(row) as usize;
                let mut c = if row - floor(row) <= 0.5 { b'*' } else { b'.' };
                while j < scope.len() {
                    scope[j][i] = c;
                    j += 1;
                    c = b'*';
                }
            }

            for (row, ascii_line) in scope.into_iter().enumerate() {
                let line = String::from_utf8(ascii_line).unwrap();
                if row & 1 == 0 {
                    let db = round(max_db - row as f64 * 5.0) as i64;
                    writeln!(&mut string, "{:3} dB {} {:3} dB", db, line, db).unwrap();
                } else {
                    writeln!(&mut string, "       {}", line).unwrap();
                }
            }

            writeln!(
                &mut string,
                "       |   |    |    |     |    |    |     |    |    |"
            )
            .unwrap();
            writeln!(
                &mut string,
                "       10  50   100  200   500  1k   2k    5k   10k  20k Hz\n"
            )
            .unwrap();

            write!(&mut string, "Peak Magnitude : {:.2} dB", max_r.0).unwrap();

            match max_r.1 {
                Some(frequency) => {
                    writeln!(&mut string, " ({} Hz)", frequency as i64).unwrap();
                }
                _ => {
                    string.push('\n');
                }
            }
        }

        writeln!(&mut string, "Inputs         : {}", self.inputs()).unwrap();
        writeln!(&mut string, "Outputs        : {}", self.outputs()).unwrap();
        writeln!(
            &mut string,
            "Latency        : {:.1} samples",
            self.latency().unwrap_or(0.0)
        )
        .unwrap();
        writeln!(&mut string, "Footprint      : {} bytes", self.footprint()).unwrap();

        string
    }
}

impl<S: Sample, T: AudioUnit<S> + ?Sized> AudioUnitExt<S> for T {}

impl<X: AudioNode + Sync + Send + 'static> AudioUnit for An<X>
where
    X::Inputs: Size<f32>,
    X::Outputs: Size<f32>,
{
    fn reset(&mut self) {
        self.0.reset();
    }
    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.0.set_sample_rate(crate::SampleRate(sample_rate));
    }
    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        debug_assert!(input.len() == self.inputs());
        debug_assert!(output.len() == self.outputs());
        output.copy_from_slice(self.0.tick(Frame::from_slice(input)).as_slice());
    }
    #[inline]
    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.0.process(size, input, output);
    }
    #[inline]
    fn set(&mut self, setting: Setting) {
        self.0.set(setting);
    }
    #[inline]
    fn inputs(&self) -> usize {
        self.0.inputs()
    }
    #[inline]
    fn outputs(&self) -> usize {
        self.0.outputs()
    }
    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        self.0.route(input, frequency)
    }
    #[inline]
    fn get_id(&self) -> u64 {
        X::ID
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn set_hash(&mut self, hash: u64) {
        self.0.set_hash(hash);
    }
    fn ping(&mut self, probe: bool, hash: AttoHash) -> AttoHash {
        self.0.ping(probe, hash)
    }
    fn footprint(&self) -> usize {
        core::mem::size_of::<X>()
    }
    /// Forwards the wrapped node's own answer. Without this every fundsp
    /// built-in would fall through to the `AudioUnit` default and report
    /// `Unknown`, since `An<X>` is how they all reach the dynamic interface.
    fn tail(&mut self) -> Tail {
        self.0.tail()
    }
    fn allocate(&mut self) {
        self.0.allocate();
    }
}

/// Converts an AudioUnit into an AudioNode with `I` inputs and `O` outputs.
#[derive(Clone)]
pub struct Unit<I: Size<f32>, O: Size<f32>> {
    _marker: PhantomData<(I, O)>,
    unit: Box<dyn AudioUnit>,
}

impl<I: Size<f32>, O: Size<f32>> Unit<I, O> {
    pub fn new(unit: Box<dyn AudioUnit>) -> Self {
        assert!(I::USIZE == unit.inputs());
        assert!(O::USIZE == unit.outputs());
        Self {
            _marker: PhantomData,
            unit,
        }
    }
}

impl<I: Size<f32>, O: Size<f32>> AudioNode for Unit<I, O> {
    const ID: u64 = 82;
    type Inputs = I;
    type Outputs = O;

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.unit.set_sample_rate(crate::SampleRate(sample_rate));
    }

    fn reset(&mut self) {
        self.unit.reset();
    }

    #[inline]
    fn tick(&mut self, input: &Frame<f32, Self::Inputs>) -> Frame<f32, Self::Outputs> {
        let mut output = Frame::default();
        self.unit.tick(input, &mut output);
        output
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.unit.process(size, input, output);
    }

    fn set(&mut self, setting: Setting) {
        self.unit.set(setting);
    }

    fn ping(&mut self, probe: bool, hash: AttoHash) -> AttoHash {
        self.unit.ping(probe, hash)
    }

    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        self.unit.route(input, frequency)
    }

    fn allocate(&mut self) {
        self.unit.allocate();
    }
}

/// A big block adapter.
/// The adapter enables calls to `process_big` with arbitrary buffer sizes.
#[derive(Clone)]
pub struct BigBlockAdapter {
    source: Box<dyn AudioUnit>,
    input: BufferVec,
    output: BufferVec,
}

impl BigBlockAdapter {
    pub fn new(source: Box<dyn AudioUnit>) -> Self {
        let input = BufferVec::new(source.inputs());
        let output = BufferVec::new(source.outputs());
        Self {
            source,
            input,
            output,
        }
    }

    pub fn process_big(&mut self, size: usize, input: &[&[f32]], output: &mut [&mut [f32]]) {
        let mut i = 0;
        while i < size {
            let n = min(size - i, MAX_BUFFER_SIZE);
            for input_i in 0..self.input.channels() {
                for j in 0..n {
                    self.input.set_f32(input_i, j, input[input_i][i + j]);
                }
            }
            self.source
                .process(n, &self.input.buffer_ref(), &mut self.output.buffer_mut());
            for output_i in 0..self.output.channels() {
                for j in 0..n {
                    output[output_i][i + j] = self.output.at_f32(output_i, j);
                }
            }
            i += n;
        }
    }
}

impl AudioUnit for BigBlockAdapter {
    fn reset(&mut self) {
        self.source.reset();
    }
    fn isolate(&mut self) {
        self.source.isolate();
    }
    fn rebind_offline(&mut self, ctx: &tutti_types::OfflineTransport) {
        self.source.rebind_offline(ctx);
    }
    fn forkable(&self) -> bool {
        self.source.forkable()
    }
    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.source.set_sample_rate(crate::SampleRate(sample_rate));
    }
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.source.tick(input, output);
    }
    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        self.source.process(size, input, output);
    }
    fn set(&mut self, setting: Setting) {
        self.source.set(setting);
    }
    fn inputs(&self) -> usize {
        self.source.inputs()
    }
    fn outputs(&self) -> usize {
        self.source.outputs()
    }
    fn get_id(&self) -> u64 {
        self.source.get_id()
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn ping(&mut self, probe: bool, hash: AttoHash) -> AttoHash {
        self.source.ping(probe, hash)
    }
    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        self.source.route(input, frequency)
    }
    /// A pure wrapper rings exactly as long as what it wraps.
    fn tail(&mut self) -> Tail {
        self.source.tail()
    }

    fn footprint(&self) -> usize {
        self.source.footprint()
    }
    fn allocate(&mut self) {
        self.source.allocate();
    }
}

/// Block rate adapter converts all processing calls to maximum length block processing.
/// Maximizes performance at the expense of latency.
/// The unit to be adapted must have no inputs.
#[derive(Clone)]
pub struct BlockRateAdapter {
    unit: Box<dyn AudioUnit>,
    channels: usize,
    buffer: BufferVec,
    index: usize,
}

impl BlockRateAdapter {
    pub fn new(unit: Box<dyn AudioUnit>) -> Self {
        assert_eq!(unit.inputs(), 0);
        let channels = unit.outputs();
        Self {
            unit,
            channels,
            buffer: BufferVec::new(channels),
            index: MAX_BUFFER_SIZE,
        }
    }
}

impl AudioUnit for BlockRateAdapter {
    fn reset(&mut self) {
        self.unit.reset();
        self.index = MAX_BUFFER_SIZE;
    }
    fn isolate(&mut self) {
        self.unit.isolate();
    }
    fn rebind_offline(&mut self, ctx: &tutti_types::OfflineTransport) {
        self.unit.rebind_offline(ctx);
    }
    fn forkable(&self) -> bool {
        self.unit.forkable()
    }
    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.unit.set_sample_rate(crate::SampleRate(sample_rate));
    }
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        if self.index == MAX_BUFFER_SIZE {
            self.unit.process(
                MAX_BUFFER_SIZE,
                &BufferRef::empty(),
                &mut self.buffer.buffer_mut(),
            );
            self.index = 0;
        }
        for channel in 0..self.channels {
            output[channel] = self.buffer.at_f32(channel, self.index);
        }
        self.index += 1;
    }
    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let mut i = 0;
        while i < size {
            if self.index == MAX_BUFFER_SIZE {
                self.unit
                    .process(MAX_BUFFER_SIZE, input, &mut self.buffer.buffer_mut());
                self.index = 0;
            }
            let n = min(size - i, MAX_BUFFER_SIZE - self.index);
            for channel in 0..self.channels {
                output.channel_f32_mut(channel)[i..i + n].clone_from_slice(
                    &self.buffer.channel_f32(channel)[self.index..self.index + n],
                );
            }
            i += n;
            self.index += n;
        }
    }
    fn set(&mut self, setting: Setting) {
        self.unit.set(setting);
    }
    fn inputs(&self) -> usize {
        0
    }
    fn outputs(&self) -> usize {
        self.channels
    }
    fn get_id(&self) -> u64 {
        self.unit.get_id()
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
    fn ping(&mut self, probe: bool, hash: AttoHash) -> AttoHash {
        self.unit.ping(probe, hash)
    }
    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame {
        self.unit.route(input, frequency)
    }
    /// A pure wrapper rings exactly as long as what it wraps.
    fn tail(&mut self) -> Tail {
        self.unit.tail()
    }

    fn footprint(&self) -> usize {
        self.unit.footprint()
    }
    fn allocate(&mut self) {
        self.unit.allocate();
    }
}

/// A dummy unit with zero output. It has an arbitrary number of inputs and outputs.
/// `Net` uses this unit.
#[derive(Clone)]
pub struct DummyUnit {
    inputs: usize,
    outputs: usize,
}

impl DummyUnit {
    pub fn new(inputs: usize, outputs: usize) -> Self {
        Self { inputs, outputs }
    }
}

impl AudioUnit for DummyUnit {
    #[inline]
    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        for x in output.iter_mut() {
            *x = 0.0;
        }
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        for channel in 0..self.outputs {
            output.channel_mut(channel)[0..simd_items(size)].fill(F32x::ZERO);
        }
    }

    fn inputs(&self) -> usize {
        self.inputs
    }

    fn outputs(&self) -> usize {
        self.outputs
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut signal = SignalFrame::new(self.outputs);
        signal.fill(Signal::Value(0.0));
        signal
    }

    fn get_id(&self) -> u64 {
        const ID: u64 = 93;
        ID
    }
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    /// A dummy emits silence, so there is nothing to ring.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

/// The seven derived-method examples from [`AudioUnit`]'s and
/// [`AudioUnitExt`]'s docs, executed.
///
/// Each was a doctest on the method it documents. The trait moved down into
/// `tutti-node`, and every one of these builds its subject with a
/// `prelude64` constructor — `dc`, `add`, `pass`, `tick`, `sink`, `limiter` —
/// which is this crate's. A doctest in `tutti-node` naming them would be a
/// dev-dependency cycle, so the assertions moved here, where the constructors
/// are. (All but `latency` have since moved back to this crate as
/// [`AudioUnitExt`]; their `ignore`d examples came with them.)
///
/// Mutation-tested against the trait, not the constructors: making `get_mono`
/// return `output[0]` for the 2-output case fails `get_mono_averages_a_stereo_generator`;
/// making `filter_stereo` broadcast channel 0 fails its test; dropping
/// `limiter`'s lookahead from `latency` fails `latency_is_the_minimum_over_outputs`.
#[cfg(test)]
mod trait_examples {
    use crate::prelude64::*;

    /// A 1-output generator yields its value; a 2-output generator yields the
    /// mean of the two, which is what makes `get_mono` mono rather than "left".
    #[test]
    fn get_mono_averages_a_stereo_generator() {
        assert_eq!(dc(2.0).get_mono(), 2.0);
        assert_eq!(dc((3.0, 4.0)).get_mono(), 3.5);
    }

    /// The mirror rule: a 1-output generator is duplicated rather than paired
    /// with silence.
    #[test]
    fn get_stereo_duplicates_a_mono_generator() {
        assert_eq!(dc((5.0, 6.0)).get_stereo(), (5.0, 6.0));
        assert_eq!(dc(7.0).get_stereo(), (7.0, 7.0));
    }

    #[test]
    fn filter_mono_passes_one_sample_through() {
        assert_eq!(add(4.0).filter_mono(5.0), 9.0);
    }

    /// Each channel gets its own addend — a broadcast of channel 0 would give
    /// `(6.0, 7.0)` here.
    #[test]
    fn filter_stereo_keeps_the_channels_apart() {
        assert_eq!(add((2.0, 3.0)).filter_stereo(4.0, 5.0), (6.0, 8.0));
    }

    /// A pass-through has unity response at every frequency.
    #[test]
    fn response_of_a_passthrough_is_unity() {
        assert_eq!(pass().response(0, 440.0), Some(Complex64::new(1.0, 0.0)));
    }

    /// The same fact in dB, which is 0 rather than 1.
    #[test]
    fn response_db_of_a_passthrough_is_zero() {
        let db = pass().response_db(0, 440.0).unwrap();
        assert!(db < 1.0e-7 && db > -1.0e-7);
    }

    /// `latency` is derived from `route`, so it answers for each shape the
    /// `Signal` walk can distinguish: zero for a pass-through and a unit delay
    /// (a `tick` reports its delay as latency-free by convention), `None` for a
    /// unit with no outputs at all, and a real figure for a lookahead limiter.
    #[test]
    fn latency_is_the_minimum_over_outputs() {
        assert_eq!(pass().latency(), Some(0.0));
        assert_eq!(tick().latency(), Some(0.0));
        assert_eq!(sink().latency(), None);
        assert_eq!(limiter(0.01, 0.01).latency(), Some(441.0));
    }
}
