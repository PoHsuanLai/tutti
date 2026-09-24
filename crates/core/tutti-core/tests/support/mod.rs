//! Stimulus nodes for this crate's integration tests.
//!
//! The engine's stimulus nodes are `tutti_nodes::testing`, and every crate
//! above `tutti-nodes` uses those. This crate cannot: `tutti-nodes` depends on
//! `tutti-core`, so naming it here — even as a dev-dependency — is a dependency
//! cycle. The fundsp one-liners these tests used before (`sine_hz`,
//! `lowpass_hz`) are no longer forwarded by `tutti_core::dsp`, so the two
//! shapes the tests need are written out here, as small as they can be.
//!
//! Neither is a DSP node anyone should reach for: the tests that use them are
//! about the `Net` and the `Engine` (root folding, allocation budgets), and the
//! node inside is only there so the graph renders something non-zero.

#![allow(dead_code)]
// Each integration-test binary compiles this module separately, and no single
// binary uses every item.

use std::f64::consts::TAU;

use tutti_core::{AudioUnit, BufferMut, BufferRef, Hz, SampleRate, Signal, SignalFrame, Tail};

/// A mono sine source, phase 0 at the first sample.
///
/// Starts at [`SampleRate::DEFAULT`]; a `Net` corrects that through
/// `set_sample_rate`.
#[derive(Clone)]
pub struct Sine {
    frequency: Hz,
    sample_rate: SampleRate,
    phase: f64,
}

impl Sine {
    pub fn new(frequency: Hz) -> Self {
        Self {
            frequency,
            sample_rate: SampleRate::DEFAULT,
            phase: 0.0,
        }
    }

    fn next(&mut self) -> f32 {
        let y = (self.phase * TAU).sin() as f32;
        self.phase += f64::from(self.frequency.get()) / self.sample_rate.get();
        self.phase -= self.phase.floor();
        y
    }
}

impl AudioUnit for Sine {
    fn reset(&mut self) {
        self.phase = 0.0;
    }

    fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        self.sample_rate = sample_rate;
    }

    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        1
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        output[0] = self.next();
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            let y = self.next();
            output.set_f32(0, i, y);
        }
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        out.set(0, Signal::Latency(0.0));
        out
    }

    fn get_id(&self) -> u64 {
        tutti_core::mnemonic(b"CORETSIN")
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

/// One input scaled by a fixed factor into one output: the smallest node that
/// does per-sample work on an input, for building a chain of `n` nodes.
///
/// The factor is a raw `f32` rather than an `Amplitude` only because the
/// allocation-budget chain varies it per node so no two nodes do identical
/// work — it is a test knob, not a control a host sets.
#[derive(Clone)]
pub struct Gain(pub f32);

impl AudioUnit for Gain {
    fn inputs(&self) -> usize {
        1
    }

    fn outputs(&self) -> usize {
        1
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        output[0] = input[0] * self.0;
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            output.set_f32(0, i, input.at_f32(0, i) * self.0);
        }
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        out.set(0, input.at(0).scale(f64::from(self.0)));
        out
    }

    fn get_id(&self) -> u64 {
        tutti_core::mnemonic(b"CORETGAN")
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
