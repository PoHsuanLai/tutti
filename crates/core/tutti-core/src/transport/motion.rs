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
//! | `declick` ([`Declick`]) | `Engine` | read every buffer to shape gain |
//!
//! The settings half — tempo, loop region, recording — lives in
//! [`TransportSettings`](super::TransportSettings) and is not routed through
//! this queue: nothing decides those, they are just values.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use crossbeam_queue::ArrayQueue;

use super::fsm::{DeclickOutcome, TransitionResult, TransportFsm};
use super::settings::TransportSettings;
use super::state::{Declick, SeekSlot};
use crate::params::Beat;
use crate::{AtomicU8, AudioThreadCell};

pub use super::fsm::MotionState;

/// How a motion change reaches the output.
///
/// The transport's only real fade decision: ramp the gain to zero first, or
/// switch on the next buffer. This was previously encoded by having two
/// variants per verb (`Stop`/`StopNow`, `Locate`/`LocateWithDeclick`) — a
/// parameter promoted to a type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FadeOut {
    /// Ramp out over the declick window, then complete the action. What a
    /// user-facing button should almost always be.
    #[default]
    Declick,
    /// Take effect on the next buffer. Clicks unless output is already silent.
    Immediate,
}

/// What the transport should be doing once a locate lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Then {
    /// Stay (or become) stopped at the target.
    #[default]
    Stop,
    /// Roll from the target.
    Roll,
    /// Keep whatever the transport was doing — a locate while rolling keeps
    /// rolling, a locate while stopped stays stopped.
    Keep,
}

/// A requested transport transition.
///
/// Every variant is a genuine state-machine input — the FSM may reject it
/// (`Play` while already rolling does nothing) or defer it (a `Declick` stop
/// while rolling fades out first). Settings changes are deliberately *not*
/// here; see [`TransportSettings`](super::TransportSettings).
///
/// `fade` and `then` are orthogonal: `fade` is about the output gain, `then`
/// about the motion after landing. Neither constrains the other, which is what
/// makes them parameters rather than more variants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MotionEvent {
    Play,
    /// Stop where we are.
    Stop {
        fade: FadeOut,
    },
    /// Jump to `beat`.
    Locate {
        beat: f64,
        fade: FadeOut,
        then: Then,
    },
    FastForward,
    Rewind,
    /// Leave fast-forward/rewind, returning to the previous motion.
    EndScrub,
}

impl MotionEvent {
    /// Stop with a fade-out. The default stop.
    pub const fn stop() -> Self {
        Self::Stop {
            fade: FadeOut::Declick,
        }
    }

    /// Stop on the next buffer. For teardown, and for tests that assert the
    /// unfaded path.
    pub const fn stop_now() -> Self {
        Self::Stop {
            fade: FadeOut::Immediate,
        }
    }

    /// Jump to `beat` without disturbing the current motion — the scrub-bar
    /// seek.
    pub const fn locate(beat: f64) -> Self {
        Self::Locate {
            beat,
            fade: FadeOut::Declick,
            then: Then::Keep,
        }
    }

    /// Jump to `beat` and roll from there.
    pub const fn locate_and_play(beat: f64) -> Self {
        Self::Locate {
            beat,
            fade: FadeOut::Declick,
            then: Then::Roll,
        }
    }

    /// Fade out, then return to `beat` when the fade completes.
    ///
    /// The Stop button. Sending a stop and a locate as two events drains both
    /// in one callback, so the seek lands immediately and the fade ramps down
    /// audio rendered from the *new* position — protecting nothing.
    pub const fn stop_and_return(beat: f64) -> Self {
        Self::Locate {
            beat,
            fade: FadeOut::Declick,
            then: Then::Stop,
        }
    }
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
    /// Fade contract with `Engine`.
    pub declick: Declick,
    /// The FSM writes the playhead on a locate, and pausedness tracks motion,
    /// so it needs the settings it publishes into.
    settings: TransportSettings,
}

/// Reports the *published* state only. The FSM behind `AudioThreadCell` is
/// audio-thread-only, so formatting must not reach into it.
impl core::fmt::Debug for MotionFsm {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MotionFsm")
            .field("motion", &self.motion())
            .field("queued", &self.queue.len())
            .field("seek_pending", &self.seek.is_pending())
            .field("declick_active", &self.declick.is_active())
            .finish_non_exhaustive()
    }
}

impl MotionFsm {
    pub fn new(settings: TransportSettings) -> Self {
        Self {
            queue: Arc::new(ArrayQueue::new(COMMAND_QUEUE_CAPACITY)),
            fsm: Arc::new(AudioThreadCell::new(TransportFsm::new())),
            motion: Arc::new(AtomicU8::new(MotionState::Stopped.into())),
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
        MotionState::from(self.motion.load(Ordering::Acquire))
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
        self.motion.store(motion.into(), Ordering::Release);
        self.settings
            .paused
            .store(motion == MotionState::Stopped, Ordering::Release);
    }

    fn locate_to(&self, pos: Beat) {
        self.settings.beat.store(pos.get(), Ordering::Release);
        self.seek.request(pos);
    }

    fn publish(&self, result: TransitionResult) {
        match result {
            TransitionResult::MotionChanged(motion) => {
                self.set_motion(motion);
                // A settled state change cancels any fade. Retargeting one
                // declick state to another must NOT clear it — that ramp is
                // mid-count, and restarting it would step the gain back to
                // full and click.
                if !is_declicking(motion) {
                    self.declick.clear();
                }
            }
            TransitionResult::DeclickStarted { motion, frames, .. } => {
                self.set_motion(motion);
                // Audio keeps playing while the gain ramps to zero.
                self.declick.start(frames);
            }
            TransitionResult::Located { pos, motion } => {
                self.locate_to(pos);
                self.set_motion(motion);
                self.declick.clear();
            }
        }
    }

    /// Called by the processor when a declick fade reaches zero: finish the
    /// action the fade was covering for.
    ///
    /// Driven by the outcome the FSM parked when the fade started, not by
    /// reading the published mirror back — the mirror is a projection, and
    /// dispatching on it is how a desynced FSM used to go unnoticed.
    pub fn complete_declick(&self) {
        self.declick.clear();

        let outcome = { self.fsm.borrow_mut().take_declick_outcome() };
        let Some(outcome) = outcome else {
            return;
        };

        match outcome {
            DeclickOutcome::Stop => self.set_motion(MotionState::Stopped),
            DeclickOutcome::Locate { pos, motion } => {
                self.locate_to(pos);
                self.set_motion(motion);
            }
        }
    }
}

/// Whether `motion` is a fade in progress. A retarget between two declick
/// states must leave the ramp counting; only a settled state clears it.
#[inline]
fn is_declicking(motion: MotionState) -> bool {
    matches!(
        motion,
        MotionState::DeclickToStop | MotionState::DeclickToLocate
    )
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

        let _ = m.try_send(MotionEvent::stop());
        m.drain();
        assert_eq!(m.motion(), MotionState::DeclickToStop);
        assert!(m.declick.is_active(), "Stop must fade out");

        // The fade completing is what actually stops it.
        m.complete_declick();
        assert!(m.is_stopped());

        let _ = m.try_send(MotionEvent::Play);
        m.drain();
        let _ = m.try_send(MotionEvent::stop_now());
        m.drain();
        assert!(m.is_stopped(), "StopNow stops immediately");
        assert!(!m.declick.is_active(), "StopNow must not fade");
    }

    #[test]
    fn locate_requests_a_seek_and_moves_the_beat() {
        let m = fsm();
        let _ = m.try_send(MotionEvent::locate(8.0));
        m.drain();

        assert_eq!(m.seek.take(), Some(Beat(8.0)), "the clock must see a seek");
        assert_eq!(m.settings.beat.load(Ordering::Acquire), 8.0);
    }

    #[test]
    fn locate_and_play_rolls_after_the_jump() {
        let m = fsm();
        let _ = m.try_send(MotionEvent::locate_and_play(4.0));
        m.drain();

        assert_eq!(m.seek.take(), Some(Beat(4.0)));
        assert!(m.is_playing());

        // D3 regression. The assertion above passed even while the FSM was
        // desynced from its published mirror — the mirror said `Rolling` while
        // the FSM still held `Stopped`, so both stop arms hit their `None` case
        // and the transport could not be stopped at all. Proving it *stops* is
        // what actually pins the fix.
        let _ = m.try_send(MotionEvent::stop_now());
        m.drain();
        assert!(
            m.is_stopped(),
            "a transport that started rolling must be stoppable"
        );
    }

    /// D1: the Stop button used to send `Stop` + `Locate(0.0)`. `drain` pops
    /// both in one callback, so the seek landed immediately while 480 samples of
    /// fade remained — the declick then ramped down audio rendered from the new
    /// position, protecting nothing, and the click it exists to suppress
    /// happened unmasked at the seek instant.
    ///
    /// One event now carries both halves, and the jump waits for silence.
    #[test]
    fn stop_and_return_holds_the_playhead_until_the_fade_ends() {
        let m = fsm();
        let _ = m.try_send(MotionEvent::Play);
        m.drain();
        m.settings.set_beat(12.0);

        let _ = m.try_send(MotionEvent::stop_and_return(0.0));
        m.drain();

        assert_eq!(m.motion(), MotionState::DeclickToLocate);
        assert!(m.declick.is_active(), "the fade must be armed");
        assert_eq!(m.seek.take(), None, "the seek must wait for the fade");
        assert_eq!(
            m.settings.beat.load(Ordering::Acquire),
            12.0,
            "the playhead must not move while audio is still fading"
        );

        m.complete_declick();
        assert_eq!(m.seek.take(), Some(Beat(0.0)), "the jump lands on silence");
        assert_eq!(m.settings.beat.load(Ordering::Acquire), 0.0);
        assert!(m.is_stopped());
    }

    /// Pressing Stop while already stopped has nothing to fade, so it returns
    /// to zero immediately — matching the old two-event behaviour and the
    /// standard DAW second-press-rewinds idiom.
    #[test]
    fn stop_and_return_from_a_stop_jumps_immediately() {
        let m = fsm();
        m.settings.set_beat(9.0);

        let _ = m.try_send(MotionEvent::stop_and_return(0.0));
        m.drain();

        assert!(m.is_stopped());
        assert_eq!(m.seek.take(), Some(Beat(0.0)));
        assert!(!m.declick.is_active(), "nothing audible to fade");
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
            .try_send_all([MotionEvent::locate(16.0), MotionEvent::Play])
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
