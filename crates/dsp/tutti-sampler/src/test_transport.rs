//! The single mock [`Timeline`] the crate's tests share.
//!
//! One mock, not one per module: a per-module copy exposes only the setters its
//! own tests happened to need, which is how a seek-while-stretched test becomes
//! unwritable in the module that needs it most.
//!
//! Constructed from [`Beat`] and [`Bpm`] rather than two bare `f64`s. Position
//! and tempo have the same primitive representation, so an argument-order slip
//! compiles clean and yields a transport at the wrong tempo *and* the wrong
//! position — a test that then fails for a reason nowhere near the code under
//! test. Unit types make the swap a compile error.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use tutti_core::{Beat, Bpm, SampleRate, Timeline};

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

    /// Start or stop the transport where it stands — the beat does not move.
    pub fn set_rolling(&self, rolling: bool) {
        self.playing.store(rolling, Ordering::Relaxed);
    }

    /// Jump the playhead — a seek or a scrub. The discontinuity this creates is
    /// the thing under test in the stretch-flush tests.
    pub fn set_beat(&self, beat: Beat) {
        self.beat.store(beat.get().to_bits(), Ordering::Relaxed);
    }

    /// Move by `samples` at `sample_rate`, the way a block-driven transport does
    /// after `process` returns. Negative `samples` rewinds, so a test can replay
    /// a span twice — which is why it is a signed `i64` and not [`Samples`]
    /// (an unsigned count that cannot carry the rewind).
    ///
    /// [`Samples`]: tutti_core::Samples
    pub fn advance(&self, samples: i64, sample_rate: impl Into<SampleRate>) {
        let tempo = f64::from_bits(self.tempo.load(Ordering::Relaxed));
        let beats = samples as f64 * tempo / 60.0 / sample_rate.into().get();
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
