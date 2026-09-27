//! Stimulus nodes for this crate's integration tests.
//!
//! The engine's stimulus nodes are `tutti_nodes::testing`, and every crate
//! above `tutti-nodes` uses those. This crate cannot: `tutti-nodes` depends on
//! `tutti-core`, so naming it here — even as a dev-dependency — is a dependency
//! cycle. So the two shapes the tests need are written out here, as small as
//! they can be.
//!
//! Neither is a DSP node anyone should reach for: the tests that use them are
//! about the `Engine` and the editor (root folding, allocation budgets), and
//! the node inside is only there so the graph renders something non-zero.
//!
//! Below them, the beat model the engine tests hold the transport to.

#![allow(dead_code)]
// Each integration-test binary compiles this module separately, and no single
// binary uses every item.

use std::f64::consts::TAU;

use tutti_core::{Hz, SampleRate, Tail};
use tutti_graph::{Cx, Io, Node, Prepare, Shape, Status};
use tutti_types::ChannelLayout;

/// A mono sine source, phase 0 at the first sample.
///
/// Starts at [`SampleRate::DEFAULT`]; a graph corrects that through
/// `prepare`.
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

impl Node for Sine {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO).with_tail(Tail::Unbounded)
    }

    fn prepare(&mut self, prepare: &Prepare) {
        self.sample_rate = prepare.sample_rate();
    }

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for y in io.output(0) {
            *y = self.next();
        }
        Status::Modified
    }

    fn reset(&mut self) {
        self.phase = 0.0;
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

impl Node for Gain {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for (y, x) in outs.get(0).iter_mut().zip(ins.get(0)) {
            *y = x * self.0;
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

// ---- the beat, in closed form ---------------------------------------------

/// The beat, segment by segment, as the engine is specified to count it,
/// written here without its code: each segment is an origin
/// frame and beat and a tempo, and the beat at a frame is the origin beat
/// plus `frames × tempo / (60 × rate)` in closed form. A loop wraps on the
/// first frame whose beat reaches its end, onto
/// `start + (beat − start) mod len`, when the playhead was inside it.
///
/// The engine tests use it as the oracle for the graph's beats.
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
