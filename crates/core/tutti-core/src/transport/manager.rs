//! Transport manager with FSM-based state management.

use crate::AudioThreadCell;
use crossbeam_queue::ArrayQueue;
use std::sync::Arc;

use super::fsm::{LocateState, TransportEvent, TransportFSM};
use super::position::{LoopRange, MusicalPosition};
use super::state::{ClockInputs, Declick, LoopSpan, SeekSlot, TransportState};
use crate::params::{Bpm, SampleRate};
use crate::{AtomicBool, AtomicF64, AtomicU8};
use std::sync::atomic::Ordering;

pub use super::fsm::{Direction, MotionState};

impl MotionState {
    fn to_u8(self) -> u8 {
        match self {
            Self::Stopped => 0,
            Self::Rolling => 1,
            Self::FastForward => 2,
            Self::Rewind => 3,
            Self::DeclickToStop => 4,
            Self::DeclickToLocate => 5,
        }
    }

    fn from_u8(val: u8) -> Self {
        match val {
            1 => Self::Rolling,
            2 => Self::FastForward,
            3 => Self::Rewind,
            4 => Self::DeclickToStop,
            5 => Self::DeclickToLocate,
            _ => Self::Stopped,
        }
    }
}

/// Transport manager - lock-free command queue for FSM updates.
pub struct TransportManager {
    /// MPMC lock-free bounded queue. Full queue drops the command —
    /// acceptable for rare user-driven transport events.
    command_queue: Arc<ArrayQueue<TransportEvent>>,
    /// FSM is only mutated from the audio thread via `process_commands()`.
    /// `AudioThreadCell` enforces this in debug builds.
    fsm: AudioThreadCell<TransportFSM>,
    tempo: Arc<AtomicF64>,
    paused: Arc<AtomicBool>,
    reverse: Arc<AtomicBool>,
    recording: Arc<AtomicBool>,
    in_preroll: Arc<AtomicBool>,
    current_beat: Arc<AtomicF64>,
    /// Loop region + arming, as one value.
    loop_span: LoopSpan,
    motion_state: Arc<AtomicU8>,
    /// Pending absolute jump, as one value.
    seek: SeekSlot,
    /// Fade contract with `GraphProcessor`.
    declick: Declick,

    sample_rate: f64,
}

// TransportManager is Send + Sync because all fields are Send + Sync:
// - command_queue: Arc<ArrayQueue<TransportEvent>> (Send + Sync)
// - fsm: AudioThreadCell<TransportFSM> (Send + Sync via AudioThreadCell's impl)
// - All other fields: Arc, atomics (Send + Sync)
// No manual unsafe impl needed.

impl TransportManager {
    pub fn new(sample_rate: impl Into<SampleRate>) -> Self {
        let sample_rate = sample_rate.into().get();
        let command_queue = Arc::new(ArrayQueue::new(64));
        let fsm = AudioThreadCell::new(TransportFSM::new());

        Self {
            command_queue,
            fsm,
            tempo: Arc::new(AtomicF64::new(120.0)),
            paused: Arc::new(AtomicBool::new(true)),
            reverse: Arc::new(AtomicBool::new(false)),
            recording: Arc::new(AtomicBool::new(false)),
            in_preroll: Arc::new(AtomicBool::new(false)),
            current_beat: Arc::new(AtomicF64::new(0.0)),
            loop_span: LoopSpan::new(0.0, 16.0),
            motion_state: Arc::new(AtomicU8::new(MotionState::Stopped.to_u8())),
            seek: SeekSlot::new(),
            declick: Declick::new(),
            sample_rate,
        }
    }

    /// Reset the FSM's AudioThreadCell owner for device switching.
    pub fn reset_fsm_owner(&self) {
        self.fsm.reset_owner();
    }

    /// Process pending transport commands.
    ///
    /// Must only be called from the audio thread. `AudioThreadCell` enforces
    /// this invariant in debug builds.
    pub fn process_commands(&self) {
        while let Some(event) = self.command_queue.pop() {
            let result = {
                let mut fsm = self.fsm.borrow_mut();
                fsm.transition(event)
            };
            if let Some(result) = result {
                self.apply_fsm_result(result);
            }
        }
    }

    /// Send transport command (lock-free, safe from any thread).
    /// Drops the event if the queue is full (bounded at 64).
    fn send_command(&self, event: TransportEvent) {
        let _ = self.command_queue.push(event);
    }

    pub fn tempo(&self) -> &Arc<AtomicF64> {
        &self.tempo
    }

    pub fn current_beat(&self) -> &Arc<AtomicF64> {
        &self.current_beat
    }

    pub fn paused(&self) -> &Arc<AtomicBool> {
        &self.paused
    }

    pub fn recording(&self) -> &Arc<AtomicBool> {
        &self.recording
    }

    pub fn in_preroll(&self) -> &Arc<AtomicBool> {
        &self.in_preroll
    }

    /// The loop region as one value.
    pub fn loop_span(&self) -> &LoopSpan {
        &self.loop_span
    }

    /// The pending-seek slot as one value.
    pub fn seek_slot(&self) -> &SeekSlot {
        &self.seek
    }

    /// Everything `TransportClock` reads to advance time.
    pub fn clock_inputs(&self) -> ClockInputs {
        ClockInputs {
            tempo: Arc::clone(&self.tempo),
            paused: Arc::clone(&self.paused),
            seek: self.seek.clone(),
            loop_span: self.loop_span.clone(),
        }
    }

    /// The read-only "what time is it" half that the clock publishes.
    pub fn state(&self) -> TransportState {
        TransportState {
            beat: Arc::clone(&self.current_beat),
            recording: Arc::clone(&self.recording),
            in_preroll: Arc::clone(&self.in_preroll),
        }
    }

    pub fn get_tempo(&self) -> Bpm {
        Bpm(self.tempo.load(Ordering::Acquire))
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Acquire)
    }

    pub fn is_recording(&self) -> bool {
        self.recording.load(Ordering::Acquire)
    }

    pub fn is_in_preroll(&self) -> bool {
        self.in_preroll.load(Ordering::Acquire)
    }

    pub fn is_reverse(&self) -> bool {
        self.reverse.load(Ordering::Acquire)
    }

    pub fn direction(&self) -> Direction {
        if self.reverse.load(Ordering::Acquire) {
            Direction::Backwards
        } else {
            Direction::Forwards
        }
    }

    pub fn get_current_beat(&self) -> f64 {
        self.current_beat.load(Ordering::Acquire)
    }

    pub fn is_loop_enabled(&self) -> bool {
        self.loop_span.is_enabled()
    }

    pub fn get_loop_range(&self) -> Option<(f64, f64)> {
        self.loop_span.is_enabled().then(|| {
            (
                self.loop_span.start.load(Ordering::Acquire),
                self.loop_span.end.load(Ordering::Acquire),
            )
        })
    }

    pub fn set_tempo(&self, bpm: impl Into<Bpm>) {
        self.tempo.store(bpm.into().get(), Ordering::Release);
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }

    pub fn set_current_beat(&self, beat: f64) {
        self.current_beat.store(beat, Ordering::Release);
    }

    pub fn set_recording(&self, recording: bool) {
        self.recording.store(recording, Ordering::Release);
    }

    pub fn set_in_preroll(&self, in_preroll: bool) {
        self.in_preroll.store(in_preroll, Ordering::Release);
    }

    pub fn play(&self) {
        self.send_command(TransportEvent::Play);
    }

    pub fn stop(&self) {
        self.send_command(TransportEvent::StopWithDeclick);
    }

    pub fn stop_immediate(&self) {
        self.send_command(TransportEvent::Stop);
    }

    fn locate_to(&self, beats: f64, event: fn(MusicalPosition) -> TransportEvent) {
        self.send_command(event(MusicalPosition::from_beats(beats)));
    }

    pub fn locate(&self, beats: f64) {
        self.locate_to(beats, TransportEvent::Locate)
    }
    pub fn locate_and_play(&self, beats: f64) {
        self.locate_to(beats, TransportEvent::LocateAndPlay)
    }
    pub fn locate_with_declick(&self, beats: f64) {
        self.locate_to(beats, TransportEvent::LocateWithDeclick)
    }

    pub fn toggle_loop(&self) {
        let current = self.loop_span.is_enabled();
        self.loop_span.set_enabled(!current);
        self.send_command(TransportEvent::SetLoopEnabled(!current));
    }

    pub fn set_loop_range_fsm(&self, start: f64, end: f64) {
        self.loop_span.set_range(start, end);
        self.loop_span.set_enabled(true);
        let range = LoopRange::new(start, end);
        self.send_command(TransportEvent::SetLoopRange(range));
    }

    pub fn clear_loop(&self) {
        self.loop_span.set_enabled(false);
        self.send_command(TransportEvent::ClearLoop);
    }

    pub fn fast_forward(&self) {
        self.send_command(TransportEvent::FastForward);
    }

    pub fn rewind(&self) {
        self.send_command(TransportEvent::Rewind);
    }

    pub fn end_scrub(&self) {
        self.send_command(TransportEvent::EndScrub);
    }

    pub fn reverse(&self) {
        self.send_command(TransportEvent::Reverse);
    }

    pub fn motion_state(&self) -> MotionState {
        MotionState::from_u8(self.motion_state.load(Ordering::Acquire))
    }

    /// The declick fade contract shared with `GraphProcessor`.
    pub fn declick(&self) -> &Declick {
        &self.declick
    }

    /// Called by the processor when the declick fade reaches zero.
    /// Completes the pending action (stop or locate).
    pub fn complete_declick(&self) {
        self.declick.clear();

        let motion = self.motion_state();
        match motion {
            MotionState::DeclickToStop => {
                self.motion_state
                    .store(MotionState::Stopped.to_u8(), Ordering::Release);
                self.paused.store(true, Ordering::Release);
            }
            MotionState::DeclickToLocate => {
                // Apply the pending locate
                let fsm = self.fsm.borrow();
                if let Some(pos) = fsm.pending_locate() {
                    self.current_beat.store(pos.beats, Ordering::Release);
                    self.seek.request(pos.beats);
                }
                // Resume rolling or stop based on locate state
                if fsm.locate_state() == LocateState::LocateAndRoll {
                    self.motion_state
                        .store(MotionState::Rolling.to_u8(), Ordering::Release);
                    self.paused.store(false, Ordering::Release);
                } else {
                    self.motion_state
                        .store(MotionState::Stopped.to_u8(), Ordering::Release);
                    self.paused.store(true, Ordering::Release);
                }
            }
            _ => {}
        }
    }

    fn apply_fsm_result(&self, result: super::fsm::TransitionResult) {
        use super::fsm::TransitionResult;

        match result {
            TransitionResult::MotionChanged(motion) => {
                self.motion_state.store(motion.to_u8(), Ordering::Release);
                let paused = matches!(motion, MotionState::Stopped);
                self.paused.store(paused, Ordering::Release);
                // Cancel any active declick on direct state change
                self.declick.clear();
            }
            TransitionResult::DeclickStarted(motion) => {
                self.motion_state.store(motion.to_u8(), Ordering::Release);
                // Start the fade-out. Audio keeps playing while gain ramps to zero.
                let fsm = self.fsm.borrow();
                let total = fsm.declick_samples() as u32;
                self.declick.start(total);
            }
            TransitionResult::Locating(pos) => {
                self.current_beat.store(pos.beats, Ordering::Release);
                self.seek.request(pos.beats);
                // If LocateAndRoll, resume playback after seek
                let fsm = self.fsm.borrow();
                if fsm.locate_state() == LocateState::LocateAndRoll {
                    self.motion_state
                        .store(MotionState::Rolling.to_u8(), Ordering::Release);
                    self.paused.store(false, Ordering::Release);
                }
            }
            TransitionResult::LoopModeChanged(enabled) => {
                self.loop_span.set_enabled(enabled);
            }
            TransitionResult::DirectionChanged(direction) => {
                self.reverse
                    .store(matches!(direction, Direction::Backwards), Ordering::Release);
            }
        }
    }

    pub fn set_loop_enabled(&self, enabled: bool) {
        self.loop_span.set_enabled(enabled);
        self.send_command(TransportEvent::SetLoopEnabled(enabled));
    }

    pub fn set_loop_range(&self, start: f64, end: f64) {
        self.loop_span.set_range(start, end);
    }

    pub fn sample_rate(&self) -> SampleRate {
        SampleRate(self.sample_rate)
    }

    pub fn beats_per_second(&self) -> f64 {
        self.tempo.load(Ordering::Acquire) / 60.0
    }

    pub fn samples_per_beat(&self) -> f64 {
        self.sample_rate / self.beats_per_second()
    }
}

impl Default for TransportManager {
    fn default() -> Self {
        Self::new(44100.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_default_state() {
        let manager = TransportManager::new(48000.0);

        assert_eq!(manager.get_tempo().get(), 120.0);
        assert!(manager.is_paused());
        assert_eq!(manager.get_current_beat(), 0.0);
        assert_eq!(manager.sample_rate().get(), 48000.0);
        assert!(!manager.is_loop_enabled());
    }

    #[test]
    fn test_atomic_accessors() {
        let manager = TransportManager::new(48000.0);

        assert_eq!(
            manager.tempo().load(Ordering::Acquire),
            manager.get_tempo().get()
        );
        assert_eq!(
            manager.current_beat().load(Ordering::Acquire),
            manager.get_current_beat()
        );
    }

    #[test]
    fn test_tempo_changes() {
        let manager = TransportManager::new(48000.0);

        manager.set_tempo(140.0);
        assert_eq!(manager.get_tempo().get(), 140.0);
        assert_eq!(manager.tempo().load(Ordering::Acquire), 140.0);

        manager.set_tempo(80.0);
        assert_eq!(manager.get_tempo().get(), 80.0);
    }

    #[test]
    fn test_playback_state() {
        let manager = TransportManager::new(48000.0);

        assert!(manager.is_paused());

        manager.set_paused(false);
        assert!(!manager.is_paused());

        manager.set_paused(true);
        assert!(manager.is_paused());
    }

    #[test]
    fn test_position_changes() {
        let manager = TransportManager::new(48000.0);

        manager.set_current_beat(4.5);
        assert_eq!(manager.get_current_beat(), 4.5);
        assert_eq!(manager.current_beat().load(Ordering::Acquire), 4.5);

        manager.set_current_beat(0.0);
        assert_eq!(manager.get_current_beat(), 0.0);
    }

    #[test]
    fn test_loop_range() {
        let manager = TransportManager::new(48000.0);

        // Initially disabled
        assert!(!manager.is_loop_enabled());
        assert_eq!(manager.get_loop_range(), None);

        manager.set_loop_range(2.0, 8.0);
        manager.set_loop_enabled(true);
        manager.process_commands(); // flush FSM queue

        assert!(manager.is_loop_enabled());
        assert_eq!(manager.get_loop_range(), Some((2.0, 8.0)));

        // Disable loop
        manager.set_loop_enabled(false);
        manager.process_commands();
        assert_eq!(manager.get_loop_range(), None);
    }

    #[cfg(feature = "std")]
    #[test]
    fn test_concurrent_access() {
        use std::sync::Arc;
        use std::thread;

        let manager = Arc::new(TransportManager::new(48000.0));

        // Spawn threads that read atomics
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let m = manager.clone();
                thread::spawn(move || {
                    for _ in 0..100 {
                        let _ = m.get_tempo();
                        let _ = m.is_paused();
                        let _ = m.get_current_beat();
                        let _ = m.is_loop_enabled();
                    }
                })
            })
            .collect();

        // Main thread writes
        for i in 0..100 {
            manager.set_tempo(100.0 + i as f64);
            manager.set_current_beat(i as f64);
        }

        for handle in handles {
            handle.join().expect("Thread panicked");
        }
    }
}
