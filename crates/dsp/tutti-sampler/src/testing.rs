//! The one transport the crate's tests share, and the block driver they
//! drive a node under it with.
//!
//! A node reads the transport from its block's `Env` (doc 013), so a test
//! holds a [`MockTransport`] — a playhead it moves between blocks, as a host
//! moves one — and hands each block the `Env` it describes
//! ([`MockTransport::env`], or [`block`] / [`play`], which drive a node
//! through `tutti_graph::contract::drive_in`).
//!
//! Constructed from [`Beat`] and [`Bpm`] rather than two bare `f64`s. Position
//! and tempo have the same primitive representation, so an argument-order slip
//! compiles clean and yields a transport at the wrong tempo *and* the wrong
//! position — a test that then fails for a reason nowhere near the code under
//! test. Unit types make the swap a compile error.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::{Beat, Bpm, Frame, SampleRate, Samples};
use tutti_graph::{contract, Env, Node, Transport, TransportChanges};

/// A transport a test can move between blocks, as a real one moves.
///
/// Interior-mutable and shared (`Arc`), so a test can move it from inside a
/// closure that renders. A mock that never moved would have every frame
/// derive the same position, and an equivalence test over it would pass no
/// matter what the code under test does.
pub struct MockTransport {
    playing: AtomicBool,
    /// `f64` bits — `AtomicF64` is not in std.
    beat: AtomicU64,
    tempo: AtomicU64,
}

impl MockTransport {
    /// Rolling, at `beat` and `tempo`.
    pub fn rolling(beat: Beat, tempo: Bpm) -> Arc<Self> {
        Arc::new(Self {
            playing: AtomicBool::new(true),
            beat: AtomicU64::new(beat.get().to_bits()),
            tempo: AtomicU64::new(tempo.get().to_bits()),
        })
    }

    /// Stopped, at `beat` and `tempo`. The placement gate reads whether the
    /// transport plays first, so this is how a test asserts
    /// silence-while-stopped.
    pub fn stopped(beat: Beat, tempo: Bpm) -> Arc<Self> {
        let t = Self::rolling(beat, tempo);
        t.playing.store(false, Ordering::Relaxed);
        t
    }

    /// Start or stop the transport where it stands — the beat does not move.
    pub fn set_rolling(&self, rolling: bool) {
        self.playing.store(rolling, Ordering::Relaxed);
    }

    /// Jump the playhead — a seek or a scrub, seen by the next block.
    pub fn set_beat(&self, beat: Beat) {
        self.beat.store(beat.get().to_bits(), Ordering::Relaxed);
    }

    /// A seek: [`set_beat`](Self::set_beat), by the name a host uses.
    pub fn seek(&self, beat: Beat) {
        self.set_beat(beat);
    }

    /// The playhead's beat.
    pub fn beat(&self) -> Beat {
        Beat::new(f64::from_bits(self.beat.load(Ordering::Relaxed)))
    }

    /// Whether it plays.
    pub fn is_rolling(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    /// Its tempo.
    pub fn tempo(&self) -> Bpm {
        Bpm::new(f64::from_bits(self.tempo.load(Ordering::Relaxed)))
    }

    /// Move by `samples` at `sample_rate`, the way a block-driven transport does
    /// after a block is rendered. Negative `samples` rewinds, so a test can
    /// replay a span twice — which is why it is a signed `i64` and not
    /// [`Samples`] (an unsigned count that cannot carry the rewind).
    pub fn advance(&self, samples: i64, sample_rate: impl Into<SampleRate>) {
        let tempo = self.tempo().get();
        let beats = samples as f64 * tempo / 60.0 / sample_rate.into().get();
        let now = self.beat().get();
        self.beat.store((now + beats).to_bits(), Ordering::Relaxed);
    }

    /// The transport as a block's `Env` carries it: here, now.
    pub fn transport(&self) -> Transport {
        Transport::new(self.is_rolling(), self.tempo(), self.beat(), None)
    }

    /// A block of `frames` at `rate` starting where the playhead stands.
    pub fn env(&self, frames: usize, rate: impl Into<SampleRate>) -> Env {
        Env {
            frame: Frame(0),
            sample_rate: rate.into(),
            block_len: Samples(frames),
            transport: self.transport(),
            changes: TransportChanges::NONE,
        }
    }
}

/// `node`, prepared at `rate` for blocks of up to `max_block` frames.
pub fn prepared<N: Node>(node: N, rate: impl Into<SampleRate>, max_block: usize) -> N {
    contract::prepared(node, rate.into(), max_block)
}

/// One block of `frames` of `node` (no inputs) under `t` at `rate`; the
/// transport is **not** moved. One `Vec` per output channel.
pub fn block(
    node: &mut dyn Node,
    t: &MockTransport,
    rate: impl Into<SampleRate>,
    frames: usize,
) -> Vec<Vec<f32>> {
    let env = t.env(frames, rate);
    contract::drive_in(node, &env, &[], &[])
}

/// `frames` of `node` in blocks of `block_len`, the transport moved after
/// each, as a host renders; planar.
pub fn play(
    node: &mut dyn Node,
    t: &MockTransport,
    rate: impl Into<SampleRate> + Copy,
    frames: usize,
    block_len: usize,
) -> Vec<Vec<f32>> {
    let rate: SampleRate = rate.into();
    let mut out: Vec<Vec<f32>> = Vec::new();
    let mut done = 0;
    while done < frames {
        let n = block_len.min(frames - done);
        let b = block(node, t, rate, n);
        if out.is_empty() {
            out = vec![Vec::with_capacity(frames); b.len()];
        }
        for (o, c) in out.iter_mut().zip(b) {
            o.extend(c);
        }
        t.advance(n as i64, rate);
        done += n;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_moves_forward_and_rewinds() {
        let t = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        // 120 BPM = 2 beats/s, so one second of samples is two beats.
        t.advance(44_100, 44_100.0);
        assert!((t.beat().get() - 2.0).abs() < 1e-9);
        t.advance(-44_100, 44_100.0);
        assert!(t.beat().get().abs() < 1e-9);
    }

    #[test]
    fn set_beat_jumps_without_regard_to_tempo() {
        let t = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        t.set_beat(Beat::new(64.0));
        assert_eq!(t.beat(), Beat::new(64.0));
    }

    #[test]
    fn stopped_is_not_rolling_but_still_reports_its_position() {
        let t = MockTransport::stopped(Beat::new(8.0), Bpm::new(90.0));
        assert!(!t.is_rolling());
        assert_eq!(t.beat(), Beat::new(8.0));
        assert_eq!(t.tempo(), Bpm::new(90.0));
        let env = t.env(4, 48_000.0);
        assert!(!env.transport.playing);
        assert_eq!(env.transport.beat(), Beat::new(8.0));
    }
}
