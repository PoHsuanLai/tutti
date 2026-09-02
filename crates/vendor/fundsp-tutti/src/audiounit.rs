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

pub use tutti_node::audiounit::AudioUnit;

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
    fn rebind_offline(&mut self, ctx: &dyn core::any::Any) {
        self.source.rebind_offline(ctx);
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
    fn rebind_offline(&mut self, ctx: &dyn core::any::Any) {
        self.unit.rebind_offline(ctx);
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
