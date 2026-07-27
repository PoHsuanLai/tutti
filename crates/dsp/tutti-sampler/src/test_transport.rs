//! One mock [`Timeline`] for the crate's tests.
//!
//! There were three, in `memory_source`, `streaming_sampler`, and
//! `track_clip_reader` — identical state (playing / beat / tempo, all
//! interior-mutable) with the methods split arbitrarily between them, so a test
//! could only move the playhead the way its own module's copy happened to allow.
//! `track_clip_reader`'s had no setter at all, which is why a
//! seek-while-stretched test could not be written there.
//!
//! Worse than the duplication: the two constructors disagreed on argument order
//! — `new(beat, tempo)` in `memory_source` against `new(tempo, beat, playing)` in
//! the other two. Both take `f64`, so transposing them compiles and yields a
//! transport at the wrong tempo *and* the wrong position. This version takes
//! [`Bpm`] and [`Beat`], so the compiler refuses the swap.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::{Beat, Bpm, Timeline};

/// A transport a test can move between blocks, as a real one moves.
///
/// Interior-mutable on purpose. A mock holding a plain `f64` cannot advance: the
/// beat never changes, every frame derives the same position, and an equivalence
/// test over it passes no matter what the code under test does.
pub struct MockTransport {
    playing: AtomicBool,
    /// `f64` bits — `AtomicF64` is not in std.
    beat: AtomicU64,
    tempo: AtomicU64,
}

impl MockTransport {
    /// Rolling, at `beat` and `tempo`.
    ///
    /// Unit-typed rather than two bare `f64`s precisely because the old
    /// signatures disagreed on their order.
    pub fn rolling(beat: Beat, tempo: Bpm) -> Arc<Self> {
        Arc::new(Self {
            playing: AtomicBool::new(true),
            beat: AtomicU64::new(beat.get().to_bits()),
            tempo: AtomicU64::new(tempo.get().to_bits()),
        })
    }

    /// Stopped, at `beat` and `tempo`. The placement gate reads `is_rolling`
    /// first, so this is how a test asserts silence-while-stopped.
    pub fn stopped(beat: Beat, tempo: Bpm) -> Arc<Self> {
        let t = Self::rolling(beat, tempo);
        t.playing.store(false, Ordering::Relaxed);
        t
    }

    /// Jump the playhead — a seek or a scrub. The discontinuity this creates is
    /// the thing under test in the stretch-flush tests.
    pub fn set_beat(&self, beat: Beat) {
        self.beat.store(beat.get().to_bits(), Ordering::Relaxed);
    }

    /// Move by `samples` at `sample_rate`, the way a block-driven transport does
    /// after `process` returns. Negative `samples` rewinds, so a test can replay
    /// a span twice.
    pub fn advance(&self, samples: i64, sample_rate: f64) {
        let tempo = f64::from_bits(self.tempo.load(Ordering::Relaxed));
        let beats = samples as f64 * tempo / 60.0 / sample_rate;
        let now = f64::from_bits(self.beat.load(Ordering::Relaxed));
        self.beat.store((now + beats).to_bits(), Ordering::Relaxed);
    }
}

impl Timeline for MockTransport {
    fn beat(&self) -> Beat {
        Beat::new(f64::from_bits(self.beat.load(Ordering::Relaxed)))
    }

    fn is_rolling(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    fn tempo(&self) -> Bpm {
        Bpm::new(f64::from_bits(self.tempo.load(Ordering::Relaxed)))
    }
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
    }
}
