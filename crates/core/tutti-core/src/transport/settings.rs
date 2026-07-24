//! Plain shared transport state — the values, not the decisions.
//!
//! Nothing here goes through the command queue and nothing can reject a
//! write: setting the tempo is a store, not a request. That is the whole
//! distinction from [`MotionFsm`](super::MotionFsm), where the state machine
//! decides whether a transition happens at all.
//!
//! Fields are public because there is nothing to encapsulate — each one is a
//! lock-free cell shared between the UI thread and the audio thread, and a
//! forwarding method would only rename it.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use super::state::LoopSpan;
use crate::params::Bpm;
use crate::{AtomicBool, AtomicF64};

/// Transport values shared between threads.
///
/// Clone shares every field — this is a handle, not a snapshot.
#[derive(Clone, Debug)]
pub struct TransportSettings {
    /// Beats per minute. Read by the clock every buffer.
    pub tempo: Arc<AtomicF64>,
    /// The playhead, in beats. Written by `TransportClock` via its position
    /// writeback; read by the UI and by pull-based sources.
    pub beat: Arc<AtomicF64>,
    /// Loop region and whether looping is armed.
    pub loop_span: LoopSpan,
    pub recording: Arc<AtomicBool>,
    pub in_preroll: Arc<AtomicBool>,
    /// Derived from motion by [`MotionFsm`](super::MotionFsm) — the clock
    /// reads it to decide whether to advance. Not set directly.
    pub paused: Arc<AtomicBool>,
}

impl TransportSettings {
    pub fn new() -> Self {
        Self {
            tempo: Arc::new(AtomicF64::new(120.0)),
            beat: Arc::new(AtomicF64::new(0.0)),
            loop_span: LoopSpan::default(),
            recording: Arc::new(AtomicBool::new(false)),
            in_preroll: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(true)),
        }
    }

    pub fn tempo(&self) -> Bpm {
        Bpm(self.tempo.load(Ordering::Acquire))
    }

    pub fn set_tempo(&self, bpm: impl Into<Bpm>) {
        self.tempo.store(bpm.into().get(), Ordering::Release);
    }

    pub fn beat(&self) -> f64 {
        self.beat.load(Ordering::Acquire)
    }

    pub fn set_beat(&self, beat: f64) {
        self.beat.store(beat, Ordering::Release);
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Acquire)
    }

    pub fn set_recording(&self, recording: bool) {
        self.recording.store(recording, Ordering::Release);
    }

    pub fn is_in_preroll(&self) -> bool {
        self.in_preroll.load(Ordering::Acquire)
    }

    pub fn set_in_preroll(&self, in_preroll: bool) {
        self.in_preroll.store(in_preroll, Ordering::Release);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }
}

impl Default for TransportSettings {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::super::LoopRange;
    use super::*;

    #[test]
    fn clone_shares_state() {
        let a = TransportSettings::new();
        let b = a.clone();

        a.set_tempo(140.0);
        assert_eq!(b.tempo().get(), 140.0, "clones must share, not copy");

        b.loop_span.set_range(2.0, 6.0);
        b.loop_span.set_enabled(true);
        assert_eq!(a.loop_span.range(), LoopRange::new(2.0, 6.0));
    }

    #[test]
    fn defaults_are_stopped_at_120() {
        let s = TransportSettings::new();
        assert_eq!(s.tempo().get(), 120.0);
        assert_eq!(s.beat(), 0.0);
        assert!(s.is_paused());
        assert!(!s.is_recording());
        assert_eq!(s.loop_span.range(), None, "looping starts disarmed");
    }
}
