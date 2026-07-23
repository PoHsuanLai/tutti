//! The motion state machine and its published outputs.
//!
//! This is the *decision* half of the transport. The UI enqueues a
//! [`MotionEvent`]; the audio thread drains the queue into the FSM, which
//! decides what actually happens (a `Play` while already rolling is a no-op,
//! a `Stop` while rolling becomes a declick fade) and publishes the result to
//! whoever needs it.
//!
//! Three published outputs, one per consumer:
//!
//! | output | consumer | why it can't be read from the FSM directly |
//! |---|---|---|
//! | [`MotionFsm::motion`] | the UI | the FSM is audio-thread-only |
//! | `seek` ([`SeekSlot`]) | `TransportClock` | consumed once per buffer |
//! | `declick` ([`Declick`]) | `GraphProcessor` | read every buffer to shape gain |
//!
//! The settings half — tempo, loop region, recording — lives in
//! [`TransportSettings`](super::TransportSettings) and is not routed through
//! this queue: nothing decides those, they are just values.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crossbeam_queue::ArrayQueue;

use super::fsm::{LocateState, TransitionResult, TransportFsm};
use super::position::MusicalPosition;
use super::settings::TransportSettings;
use super::state::{Declick, SeekSlot};
use crate::{AtomicU8, AudioThreadCell};

pub use super::fsm::MotionState;

/// A requested transport transition.
///
/// Every variant is a genuine state-machine input — the FSM may reject it
/// (`Play` while already rolling does nothing) or defer it (`Stop` while
/// rolling starts a declick fade first). Settings changes are deliberately
/// *not* here; see [`TransportSettings`](super::TransportSettings).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MotionEvent {
    Play,
    /// Stop after a declick fade-out. This is what a user-facing "stop"
    /// should almost always be — see [`MotionEvent::StopNow`].
    Stop,
    /// Stop on the next buffer with no fade. Clicks unless the output is
    /// already silent.
    StopNow,
    /// Jump to a beat, staying stopped.
    Locate(f64),
    /// Jump to a beat, fading out first if currently rolling.
    LocateWithDeclick(f64),
    /// Jump to a beat and start rolling.
    LocateAndPlay(f64),
    FastForward,
    Rewind,
    /// Leave fast-forward/rewind, returning to the previous motion.
    EndScrub,
    /// Flip playback direction.
    Reverse,
}

/// Capacity of the UI → audio-thread command queue. Transport commands are
/// user-driven and rare; overflowing means something is spamming events.
const COMMAND_QUEUE_CAPACITY: usize = 64;

/// The command queue was full, so `event` never reached the state machine.
///
/// Distinct from the FSM *rejecting* a transition (`Play` while already
/// rolling is a legitimate no-op): this means the event was lost, which is a
/// bug — the queue holds 64 user-driven commands.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueueFull {
    pub event: MotionEvent,
}

impl core::fmt::Display for QueueFull {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "transport command queue full; dropped {:?}", self.event)
    }
}

impl std::error::Error for QueueFull {}

/// The motion state machine plus the outputs it publishes.
///
/// Clone shares every field — this is a handle, not a value.
#[derive(Clone)]
pub struct MotionFsm {
    /// MPMC lock-free bounded queue. A full queue drops the command;
    /// [`MotionFsm::try_send`] reports that rather than hiding it.
    queue: Arc<ArrayQueue<MotionEvent>>,
    /// Mutated only from the audio thread, via [`MotionFsm::drain`].
    /// `AudioThreadCell` enforces this in debug builds.
    fsm: Arc<AudioThreadCell<TransportFsm>>,
    /// Published mirror of the FSM's motion, so the UI can read it without
    /// touching the audio thread's cell.
    motion: Arc<AtomicU8>,
    /// Pending absolute jump. Consumed by `TransportClock` once per buffer.
    pub seek: SeekSlot,
    /// Fade contract with `GraphProcessor`.
    pub declick: Declick,
    /// The FSM writes the playhead on a locate, and pausedness tracks motion,
    /// so it needs the settings it publishes into.
    settings: TransportSettings,
}

impl MotionFsm {
    pub fn new(settings: TransportSettings) -> Self {
        Self {
            queue: Arc::new(ArrayQueue::new(COMMAND_QUEUE_CAPACITY)),
            fsm: Arc::new(AudioThreadCell::new(TransportFsm::new())),
            motion: Arc::new(AtomicU8::new(MotionState::Stopped.to_u8())),
            seek: SeekSlot::new(),
            declick: Declick::new(),
            settings,
        }
    }

    /// Request a transition. Lock-free, callable from any thread.
    ///
    /// `Ok` means only that the event was *queued* — the FSM decides on the
    /// audio thread whether it actually applies, and that decision is not
    /// available synchronously. Observe the outcome by reading
    /// [`MotionFsm::motion`] on a later frame.
    ///
    /// `Err` means the queue was full and the event is gone. That is the one
    /// failure a caller can act on, so it is `#[must_use]`.
    pub fn try_send(&self, event: MotionEvent) -> Result<(), QueueFull> {
        self.queue.push(event).map_err(|event| QueueFull { event })
    }

    /// Request several transitions, in order.
    ///
    /// Stops at the first drop and reports it; events queued before that point
    /// still stand, so the transport may be left partway through the batch.
    pub fn try_send_all(
        &self,
        events: impl IntoIterator<Item = MotionEvent>,
    ) -> Result<(), QueueFull> {
        events.into_iter().try_for_each(|e| self.try_send(e))
    }

    /// The current motion, as published by the last drain.
    pub fn motion(&self) -> MotionState {
        MotionState::from_u8(self.motion.load(Ordering::Acquire))
    }

    pub fn is_playing(&self) -> bool {
        self.motion() == MotionState::Rolling
    }

    pub fn is_stopped(&self) -> bool {
        self.motion() == MotionState::Stopped
    }

    /// Drain the queue into the FSM and publish the results.
    ///
    /// **Audio thread only** — `AudioThreadCell` panics in debug builds
    /// otherwise.
    pub fn drain(&self) {
        while let Some(event) = self.queue.pop() {
            let result = {
                let mut fsm = self.fsm.borrow_mut();
                fsm.transition(event)
            };
            if let Some(result) = result {
                self.publish(result);
            }
        }
    }

    /// Reset the audio-thread ownership assertion. Needed when the device
    /// switches and a different thread takes over the callback.
    pub fn reset_owner(&self) {
        self.fsm.reset_owner();
    }

    fn set_motion(&self, motion: MotionState) {
        self.motion.store(motion.to_u8(), Ordering::Release);
        self.settings
            .paused
            .store(motion == MotionState::Stopped, Ordering::Release);
    }

    fn locate_to(&self, pos: MusicalPosition) {
        self.settings.beat.store(pos.beats, Ordering::Release);
        self.seek.request(pos.beats);
    }

    fn publish(&self, result: TransitionResult) {
        match result {
            TransitionResult::MotionChanged(motion) => {
                self.set_motion(motion);
                // A direct state change cancels any fade in progress.
                self.declick.clear();
            }
            TransitionResult::DeclickStarted(motion) => {
                self.set_motion(motion);
                // Audio keeps playing while the gain ramps to zero.
                let total = self.fsm.borrow().declick_samples() as u32;
                self.declick.start(total);
            }
            TransitionResult::Locating(pos) => {
                self.locate_to(pos);
                if self.fsm.borrow().locate_state() == LocateState::LocateAndRoll {
                    self.set_motion(MotionState::Rolling);
                }
            }
            TransitionResult::DirectionChanged(direction) => {
                self.settings.reverse.store(
                    direction == super::fsm::Direction::Backwards,
                    Ordering::Release,
                );
            }
        }
    }

    /// Called by the processor when a declick fade reaches zero: finish the
    /// action the fade was covering for.
    pub fn complete_declick(&self) {
        self.declick.clear();

        match self.motion() {
            MotionState::DeclickToStop => self.set_motion(MotionState::Stopped),
            MotionState::DeclickToLocate => {
                let (pending, roll) = {
                    let fsm = self.fsm.borrow();
                    (
                        fsm.pending_locate(),
                        fsm.locate_state() == LocateState::LocateAndRoll,
                    )
                };
                if let Some(pos) = pending {
                    self.locate_to(pos);
                }
                self.set_motion(if roll {
                    MotionState::Rolling
                } else {
                    MotionState::Stopped
                });
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::Beat;

    fn fsm() -> MotionFsm {
        MotionFsm::new(TransportSettings::new())
    }

    #[test]
    fn send_then_drain_applies_the_transition() {
        let m = fsm();
        assert!(m.is_stopped());

        assert!(m.try_send(MotionEvent::Play).is_ok());
        // Nothing happens until the audio thread drains.
        assert!(m.is_stopped(), "send must not apply the transition itself");

        m.drain();
        assert!(m.is_playing());
    }

    #[test]
    fn send_reports_a_full_queue() {
        let m = fsm();
        for _ in 0..COMMAND_QUEUE_CAPACITY {
            assert!(m.try_send(MotionEvent::Play).is_ok());
        }
        assert_eq!(
            m.try_send(MotionEvent::Play),
            Err(QueueFull {
                event: MotionEvent::Play
            }),
            "a full queue must report the drop, and hand the event back"
        );
    }

    #[test]
    fn stop_fades_and_stop_now_does_not() {
        let m = fsm();
        let _ = m.try_send(MotionEvent::Play);
        m.drain();

        let _ = m.try_send(MotionEvent::Stop);
        m.drain();
        assert_eq!(m.motion(), MotionState::DeclickToStop);
        assert!(m.declick.is_active(), "Stop must fade out");

        // The fade completing is what actually stops it.
        m.complete_declick();
        assert!(m.is_stopped());

        let _ = m.try_send(MotionEvent::Play);
        m.drain();
        let _ = m.try_send(MotionEvent::StopNow);
        m.drain();
        assert!(m.is_stopped(), "StopNow stops immediately");
        assert!(!m.declick.is_active(), "StopNow must not fade");
    }

    #[test]
    fn locate_requests_a_seek_and_moves_the_beat() {
        let m = fsm();
        let _ = m.try_send(MotionEvent::Locate(8.0));
        m.drain();

        assert_eq!(m.seek.take(), Some(Beat(8.0)), "the clock must see a seek");
        assert_eq!(m.settings.beat.load(Ordering::Acquire), 8.0);
    }

    #[test]
    fn locate_and_play_rolls_after_the_jump() {
        let m = fsm();
        let _ = m.try_send(MotionEvent::LocateAndPlay(4.0));
        m.drain();

        assert_eq!(m.seek.take(), Some(Beat(4.0)));
        assert!(m.is_playing());
    }

    #[test]
    fn paused_tracks_motion() {
        let m = fsm();
        assert!(m.settings.paused.load(Ordering::Acquire));

        let _ = m.try_send(MotionEvent::Play);
        m.drain();
        assert!(
            !m.settings.paused.load(Ordering::Acquire),
            "paused is derived from motion, not set separately"
        );
    }

    #[test]
    fn send_all_preserves_order() {
        let m = fsm();
        assert!(m
            .try_send_all([MotionEvent::Locate(16.0), MotionEvent::Play])
            .is_ok());
        m.drain();

        assert_eq!(m.seek.take(), Some(Beat(16.0)));
        assert!(m.is_playing());
    }

    #[test]
    fn scrub_returns_to_the_previous_motion() {
        let m = fsm();
        let _ = m.try_send(MotionEvent::Play);
        m.drain();

        let _ = m.try_send(MotionEvent::FastForward);
        m.drain();
        assert_eq!(m.motion(), MotionState::FastForward);

        let _ = m.try_send(MotionEvent::EndScrub);
        m.drain();
        assert!(m.is_playing(), "EndScrub restores what was playing before");
    }
}
