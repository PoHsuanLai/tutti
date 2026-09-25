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
use super::timed::{Schedule, ScheduleFull, TransportCommand};
use crate::Beat;
use crate::{AtomicU8, AudioThreadCell};
use tutti_types::At;

pub use super::fsm::MotionState;

/// How a motion change reaches the output.
///
/// The transport's only real fade decision: ramp the gain to zero first, or
/// switch on the next buffer. A parameter rather than a variant per verb, so
/// adding a new motion verb does not double the event list.
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
    /// Roll from the current position. A no-op while already rolling.
    Play,
    /// Stop at the current position.
    Stop {
        /// Whether to ramp the output down first.
        fade: FadeOut,
    },
    /// Jump to `beat`.
    Locate {
        /// The absolute target position.
        beat: Beat,
        /// Whether to ramp the output down before jumping.
        fade: FadeOut,
        /// What the transport should be doing once the jump lands.
        then: Then,
    },
    /// Begin scrubbing forward.
    FastForward,
    /// Begin scrubbing backward.
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
    pub const fn locate(beat: Beat) -> Self {
        Self::Locate {
            beat,
            fade: FadeOut::Declick,
            then: Then::Keep,
        }
    }

    /// Jump to `beat` and roll from there.
    pub const fn locate_and_play(beat: Beat) -> Self {
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
    pub const fn stop_and_return(beat: Beat) -> Self {
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
    /// The event that was dropped, handed back so a caller can retry it.
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
    /// Timestamped commands, applied by the engine on their frame. See
    /// [`schedule`](Self::schedule).
    timed: Schedule,
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
    /// A stopped machine publishing into `settings`, with an empty queue.
    pub fn new(settings: TransportSettings) -> Self {
        Self {
            queue: Arc::new(ArrayQueue::new(COMMAND_QUEUE_CAPACITY)),
            fsm: Arc::new(AudioThreadCell::new(TransportFsm::new())),
            motion: Arc::new(AtomicU8::new(MotionState::Stopped.into())),
            seek: SeekSlot::new(),
            declick: Declick::new(),
            settings,
            timed: Schedule::new(),
        }
    }

    /// Request a transport change **at a time**: a play, stop or seek
    /// ([`MotionEvent`]), a tempo or a loop edit ([`TransportCommand`]).
    /// Lock-free, callable from any thread, never allocates.
    ///
    /// The engine applies it on its frame, sample-accurately
    /// ([`Engine::process`](crate::Engine::process)):
    ///
    /// - an [`At::Frame`] on the engine's frame clock. For a graph engine
    ///   that is the executor's (`tutti_graph::Executor::frame`): it tracks
    ///   device time, advances while a re-prepare has the graph suspended,
    ///   and is rescaled to the same wall-clock time on a rate change. For a
    ///   `Net` engine it is the frames rendered since the engine was built;
    /// - an [`At::Beat`] on the first frame at or after that beat once
    ///   playback reaches it;
    /// - an [`At::NextBlock`] at the next block's first frame.
    ///
    /// Commands due on one frame apply in the order they were sent. A frame
    /// already past, a beat continuous playback already crossed, or a
    /// command past a block's cut bound
    /// ([`MAX_TRANSPORT_CHANGES`](tutti_graph::MAX_TRANSPORT_CHANGES)) lands
    /// at the start of the next block and is counted
    /// ([`late_commands`](Self::late_commands)); an `At::NextBlock` needs no
    /// cut and is never late. A beat a seek or loop jumped over waits until
    /// playback reaches it, holding its credit;
    /// [`cancel_scheduled`](Self::cancel_scheduled) takes it back.
    ///
    /// `Err` when [`SCHEDULE_CAPACITY`](super::SCHEDULE_CAPACITY) commands
    /// are in flight: nothing was sent, and the command is handed back.
    ///
    /// The untimed [`try_send`](Self::try_send) and settings stores still
    /// work and mean `At::NextBlock`; this method has no untimed form of its
    /// own, so "whenever" is spelled out as `At::NextBlock`.
    pub fn schedule(
        &self,
        at: At,
        command: impl Into<TransportCommand>,
    ) -> Result<(), ScheduleFull> {
        self.timed.send(at, command.into())
    }

    /// Take back every scheduled command not yet applied, and free their
    /// credit. Takes effect at the engine's next block.
    pub fn cancel_scheduled(&self) {
        self.timed.cancel_all();
    }

    /// Scheduled commands in flight: sent, and not yet applied or cancelled.
    pub fn scheduled_outstanding(&self) -> usize {
        self.timed.outstanding()
    }

    /// Scheduled commands that were already past due when the engine first
    /// saw them, and so landed at the start of that block instead of on
    /// their frame. Never dropped.
    pub fn late_commands(&self) -> u64 {
        self.timed.late()
    }

    /// The timestamped queue, for the engine.
    pub(crate) fn timed(&self) -> &Schedule {
        &self.timed
    }

    /// The settings this machine publishes into, for the engine.
    pub(crate) fn settings(&self) -> &TransportSettings {
        &self.settings
    }

    /// Apply one scheduled command now. **Audio thread only**, as
    /// [`drain`](Self::drain): a motion change goes through the state
    /// machine exactly as a drained event does.
    pub(crate) fn apply(&self, command: TransportCommand) {
        match command {
            TransportCommand::Motion(event) => {
                let result = { self.fsm.borrow_mut().transition(event) };
                if let Some(result) = result {
                    self.publish(result);
                }
            }
            TransportCommand::Tempo(bpm) => self.settings.set_tempo(bpm),
            TransportCommand::Loop(Some(range)) => {
                self.settings
                    .loop_span
                    .set_range(range.start(), range.end());
                self.settings.loop_span.set_enabled(true);
            }
            TransportCommand::Loop(None) => self.settings.loop_span.set_enabled(false),
        }
    }

    /// Request a transition. Lock-free, callable from any thread.
    ///
    /// `Ok` means only that the event was *queued* — the FSM decides on the
    /// audio thread whether it actually applies, and that decision is not
    /// available synchronously. Observe the outcome by reading
    /// [`MotionFsm::motion`] on a later frame.
    ///
    /// `Err` means the queue was full and the event is gone — the one failure
    /// a caller can act on, and the event is handed back inside [`QueueFull`]
    /// so it can be retried.
    ///
    /// RT-safe: lock-free, no allocation.
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

    /// Whether the published motion is exactly [`MotionState::Rolling`] —
    /// `false` while scrubbing or mid-fade.
    pub fn is_playing(&self) -> bool {
        self.motion() == MotionState::Rolling
    }

    /// Whether the published motion is exactly [`MotionState::Stopped`] —
    /// `false` during a fade that has not yet completed.
    pub fn is_stopped(&self) -> bool {
        self.motion() == MotionState::Stopped
    }

    /// Drain the queue into the FSM and publish the results.
    ///
    /// **Audio thread only.** RT-safe: no allocation, no locks, bounded by the
    /// queue's 64-command capacity.
    ///
    /// # Panics
    ///
    /// In debug builds, if called from a thread other than the one that first
    /// borrowed the FSM's `AudioThreadCell`. Announce a new callback thread
    /// with [`reset_owner`](Self::reset_owner) rather than letting it be
    /// discovered here.
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

    /// Reset the audio-thread ownership assertion, ahead of a device switch.
    ///
    /// Currently a no-op: it delegates to `AudioThreadCell::reset_owner`,
    /// which pins no owner thread (the cell's debug check detects a
    /// *concurrent borrow*, not a foreign thread). Kept for source
    /// compatibility — see `tutti_cpal::AudioCallbackState::reset_owners`.
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
            TransitionResult::MotionChanged(motion) if is_declicking(motion) => {
                // A fade retargeted in flight. Its ramp is mid-count and must
                // NOT restart (the gain would step back to full and click),
                // but its new outcome takes effect on the transport now, as a
                // fresh fade's does.
                self.set_mirror(motion);
                let outcome = { self.fsm.borrow().declick_outcome() };
                if let Some(outcome) = outcome {
                    self.apply_outcome(outcome);
                }
            }
            TransitionResult::MotionChanged(motion) => {
                // A settled state change cancels any fade.
                self.set_motion(motion);
                self.declick.clear();
            }
            TransitionResult::DeclickStarted {
                motion,
                frames,
                on_complete,
            } => {
                // The transport stops or jumps **now**, on the command's
                // frame; the fade is an audio-only concern (doc 013 §6). The
                // output ramps to zero over `frames` from here, and the motion
                // mirror reads the declick state until the fade completes.
                self.set_mirror(motion);
                self.declick.start(frames);
                self.apply_outcome(on_complete);
            }
            TransitionResult::Located { pos, motion } => {
                self.locate_to(pos);
                self.set_motion(motion);
                self.declick.clear();
            }
        }
    }

    /// Put a fade's outcome into effect on the transport: stop the playhead,
    /// or jump it and leave it rolling or stopped as the outcome says.
    fn apply_outcome(&self, outcome: DeclickOutcome) {
        match outcome {
            DeclickOutcome::Stop => self.settings.paused.store(true, Ordering::Release),
            DeclickOutcome::Locate { pos, motion } => {
                self.locate_to(pos);
                self.settings
                    .paused
                    .store(motion == MotionState::Stopped, Ordering::Release);
            }
        }
    }

    /// Publish `motion` to the UI mirror only, leaving the playhead's
    /// pausedness alone (a declick state is not a playhead state).
    fn set_mirror(&self, motion: MotionState) {
        self.motion.store(motion.into(), Ordering::Release);
    }

    /// Called by the processor when a declick fade reaches zero: settle the
    /// motion the fade was heading for.
    ///
    /// The transport itself already stopped or jumped when the fade began
    /// (see `publish`); what is left is the mirror and the parked outcome.
    /// Driven by the outcome the FSM parked, not by reading the published
    /// mirror back: the mirror is a projection, so dispatching on it lets an
    /// FSM/mirror disagreement pass unnoticed.
    ///
    /// **Audio thread only**, for the same reason as
    /// [`drain`](Self::drain) — it borrows the FSM's cell.
    pub fn complete_declick(&self) {
        self.declick.clear();

        let outcome = { self.fsm.borrow_mut().take_declick_outcome() };
        let Some(outcome) = outcome else {
            return;
        };

        match outcome {
            DeclickOutcome::Stop => self.set_motion(MotionState::Stopped),
            DeclickOutcome::Locate { motion, .. } => self.set_motion(motion),
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
    use crate::Beat;

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
        let _ = m.try_send(MotionEvent::locate(Beat(8.0)));
        m.drain();

        assert_eq!(m.seek.take(), Some(Beat(8.0)), "the clock must see a seek");
        assert_eq!(m.settings.beat.load(Ordering::Acquire), 8.0);
    }

    #[test]
    fn locate_and_play_rolls_after_the_jump() {
        let m = fsm();
        let _ = m.try_send(MotionEvent::locate_and_play(Beat(4.0)));
        m.drain();

        assert_eq!(m.seek.take(), Some(Beat(4.0)));
        assert!(m.is_playing());

        // The assertion above passes even when the FSM is desynced from its
        // published mirror — a mirror reading `Rolling` over an FSM holding
        // `Stopped` sends both stop arms into their `None` case, and the
        // transport cannot be stopped at all. Proving it *stops* is what pins
        // the real property.
        let _ = m.try_send(MotionEvent::stop_now());
        m.drain();
        assert!(
            m.is_stopped(),
            "a transport that started rolling must be stoppable"
        );
    }

    /// The Stop button's fade and its return-to-zero are ONE event, and the
    /// transport acts on it at once: the playhead jumps (and stops) on the
    /// command's frame, and the fade only shapes the audio from there (doc
    /// 013 §6, the declick decision: a fade is an audio-only concern and
    /// never delays the transport state a graph sees). Completing the fade
    /// settles the motion mirror and does **not** jump again.
    ///
    /// This replaced a test pinning the opposite rule (the seek waited for
    /// the fade), which the doc 013 decision reversed.
    ///
    /// Mutation: leave `apply_outcome` out of the `DeclickStarted` arm → no
    /// seek, playhead still 12 → fails. Call `locate_to` again in
    /// `complete_declick` → a second seek is requested → fails.
    #[test]
    fn stop_and_return_moves_the_playhead_at_once_and_fades_the_audio() {
        let m = fsm();
        let _ = m.try_send(MotionEvent::Play);
        m.drain();
        m.settings.set_beat(12.0);

        let _ = m.try_send(MotionEvent::stop_and_return(Beat(0.0)));
        m.drain();

        assert_eq!(m.motion(), MotionState::DeclickToLocate);
        assert!(m.declick.is_active(), "the fade must be armed");
        assert_eq!(m.seek.take(), Some(Beat(0.0)), "the jump lands now");
        assert_eq!(m.settings.beat.load(Ordering::Acquire), 0.0);
        assert!(m.settings.is_paused(), "and the playhead holds there");

        m.complete_declick();
        assert_eq!(m.seek.take(), None, "no second jump");
        assert!(m.is_stopped());
    }

    /// A declick stop pauses the playhead on its command; the motion mirror
    /// reads the fade until it completes.
    ///
    /// Mutation: store `paused` from the motion (`set_motion`) in the
    /// `DeclickStarted` arm → `DeclickToStop` is not `Stopped`, so the
    /// playhead keeps rolling → fails.
    #[test]
    fn a_declick_stop_pauses_the_playhead_at_once() {
        let m = fsm();
        let _ = m.try_send(MotionEvent::Play);
        m.drain();
        let _ = m.try_send(MotionEvent::stop());
        m.drain();
        assert_eq!(m.motion(), MotionState::DeclickToStop);
        assert!(m.settings.is_paused());
        m.complete_declick();
        assert!(m.is_stopped() && m.settings.is_paused());
    }

    /// Pressing Stop while already stopped has nothing to fade, so it returns
    /// to zero immediately — the standard DAW second-press-rewinds idiom.
    #[test]
    fn stop_and_return_from_a_stop_jumps_immediately() {
        let m = fsm();
        m.settings.set_beat(9.0);

        let _ = m.try_send(MotionEvent::stop_and_return(Beat(0.0)));
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
            .try_send_all([MotionEvent::locate(Beat(16.0)), MotionEvent::Play])
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
