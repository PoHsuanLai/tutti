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
use crate::{AtomicBool, AtomicF64, AtomicI64};
use crate::{Beat, Bpm};

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
    /// Whether the transport is armed and capturing. Read by the metronome's
    /// `RecordingOnly` mode and by hosted plugins' transport snapshot.
    pub recording: Arc<AtomicBool>,
    /// Whether a count-in is running. Read independently of `recording` — the
    /// metronome's `RecordingOnly` mode plays only when recording *and not* in
    /// preroll, while `PrerollOnly` plays on exactly the opposite condition.
    ///
    /// No engine-side writer sets it; a host drives it around its own
    /// count-in.
    pub in_preroll: Arc<AtomicBool>,
    /// Derived from motion by [`MotionFsm`](super::MotionFsm) — the clock
    /// reads it to decide whether to advance. Not set directly.
    pub paused: Arc<AtomicBool>,
    /// Free-running sample count since the stream started, written by
    /// `TransportClock` every buffer.
    ///
    /// Unlike [`beat`](Self::beat) this never jumps: it ignores loops, seeks and
    /// stops. Hosted plugins receive it as VST3's `continousTimeSamples` /
    /// CLAP's `steady_time`, which free-running effects key off precisely
    /// because the playhead is discontinuous.
    pub steady_time: Arc<AtomicI64>,
}

impl TransportSettings {
    /// Fresh settings: 120 BPM, beat 0, paused, not recording, loop disarmed.
    pub fn new() -> Self {
        Self {
            tempo: Arc::new(AtomicF64::new(120.0)),
            beat: Arc::new(AtomicF64::new(0.0)),
            loop_span: LoopSpan::default(),
            recording: Arc::new(AtomicBool::new(false)),
            in_preroll: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(true)),
            steady_time: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Samples elapsed since the stream started. Free-running: never reset by a
    /// loop, seek, or stop.
    pub fn steady_time(&self) -> i64 {
        self.steady_time.load(Ordering::Relaxed)
    }

    /// The tempo in force.
    pub fn tempo(&self) -> Bpm {
        Bpm(self.tempo.load(Ordering::Acquire))
    }

    /// Store a new tempo. **Unclamped** — zero and negative values reach the
    /// clock, which is why the derived conversions guard against them.
    pub fn set_tempo(&self, bpm: impl Into<Bpm>) {
        self.tempo.store(bpm.into().get(), Ordering::Release);
    }

    /// The playhead as last published by the clock.
    pub fn beat(&self) -> Beat {
        Beat(self.beat.load(Ordering::Acquire))
    }

    /// Overwrite the published playhead.
    ///
    /// This moves the *readout*, not the clock — a control-thread caller that
    /// wants the graph to jump sends [`MotionEvent::locate`](super::MotionEvent::locate)
    /// instead, which requests a seek the clock consumes.
    pub fn set_beat(&self, beat: impl Into<Beat>) {
        self.beat.store(beat.into().get(), Ordering::Release);
    }

    /// Whether the transport is armed and capturing.
    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Acquire)
    }

    /// Arm or disarm recording.
    pub fn set_recording(&self, recording: bool) {
        self.recording.store(recording, Ordering::Release);
    }

    /// Whether a count-in is running.
    pub fn is_in_preroll(&self) -> bool {
        self.in_preroll.load(Ordering::Acquire)
    }

    /// Mark the count-in as running or finished.
    pub fn set_in_preroll(&self, in_preroll: bool) {
        self.in_preroll.store(in_preroll, Ordering::Release);
    }

    /// Whether the clock is holding position.
    ///
    /// Read-only here: the flag is derived from motion by
    /// [`MotionFsm`](super::MotionFsm), so there is no setter to race it with.
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
}
