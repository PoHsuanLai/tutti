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
//!
//! Below them, the beat model the engine tests hold the transport to.

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

// ---- the beat, in closed form ---------------------------------------------

/// The beat, segment by segment, as the engine is specified to count it
/// (doc 013 §6), written here without its code: each segment is an origin
/// frame and beat and a tempo, and the beat at a frame is the origin beat
/// plus `frames × tempo / (60 × rate)` in closed form. A loop wraps on the
/// first frame whose beat reaches its end, onto
/// `start + (beat − start) mod len`, when the playhead was inside it.
///
/// Until doc 013 PR 15 the engine tests compared the graph's beats against
/// a `Net`'s `TransportClock` rendered by the same engine; this is what that
/// clock was pinned to, and the oracle now.
#[derive(Clone, Copy, Debug)]
pub struct Segment {
    pub frame: u64,
    pub beat: f64,
    pub bpm: f64,
    pub rate: f64,
}

impl Segment {
    /// The beat on frame `f`: `TimelineSegment::beat_at`'s arithmetic,
    /// IEEE-exact on every target (no libm).
    pub fn at(&self, f: u64) -> f64 {
        self.beat + ((f - self.frame) as f64 * self.bpm) / (60.0 * self.rate)
    }
}

/// One change to the model's transport, applied on its frame (after the
/// wrap check for that frame, as the engine advances to a frame before it
/// applies the commands due on it).
#[derive(Clone, Copy, Debug)]
pub enum Change {
    Tempo(f64),
    Loop(Option<(f64, f64)>),
    Seek(f64),
}

/// The model's beat on every frame `0..=frames` at `rate`, rolling from
/// beat 0 at `bpm`, under `changes` (frame, change), which must be in frame
/// order.
pub fn model_beats(rate: f64, bpm: f64, changes: &[(u64, Change)], frames: u64) -> Vec<f64> {
    let mut seg = Segment {
        frame: 0,
        beat: 0.0,
        bpm,
        rate,
    };
    let mut looping: Option<(f64, f64)> = None;
    let mut next = 0;
    let mut out = Vec::with_capacity(frames as usize + 1);
    for f in 0..=frames {
        if f > 0 {
            if let Some((start, end)) = looping {
                let (was, now) = (seg.at(f - 1), seg.at(f));
                if was < end && now >= end {
                    seg = Segment {
                        frame: f,
                        beat: start + (now - start).rem_euclid(end - start),
                        ..seg
                    };
                }
            }
        }
        while next < changes.len() && changes[next].0 == f {
            match changes[next].1 {
                Change::Tempo(bpm) => {
                    seg = Segment {
                        frame: f,
                        beat: seg.at(f),
                        bpm,
                        rate,
                    }
                }
                Change::Loop(l) => looping = l,
                Change::Seek(beat) => {
                    seg = Segment {
                        frame: f,
                        beat,
                        ..seg
                    }
                }
            }
            next += 1;
        }
        out.push(seg.at(f));
    }
    out
}
