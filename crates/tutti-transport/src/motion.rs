//! The motion state machine and its published outputs.
//!
//! This is the *decision* half of the transport. The UI enqueues a
//! [`MotionEvent`] through the control handle, [`MotionFsm`]; the engine
//! owns the audio half, `MotionAudio`, and applies each command to its FSM,
//! which decides what actually happens (a `Play` while already rolling is a no-op,
//! a `Stop` while rolling becomes a declick fade) and publishes the result to
//! whoever needs it.
//!
//! Two published outputs, one per consumer:
//!
//! | output | consumer | why it can't be read from the FSM directly |
//! |---|---|---|
//! | [`MotionFsm::motion`] | the UI | the FSM is the engine's |
//! | `seek` ([`SeekSlot`]) | `TransportClock` | consumed once per buffer |
//!
//! The settings half — tempo, recording — lives in
//! [`TransportSettings`] and is not routed through
//! this queue: nothing decides those, they are just values. The loop region
//! is the engine's: an edit is a scheduled
//! [`TransportCommand::Loop`], and
//! [`MotionFsm::loop_region`] reads what the engine published.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crossbeam_queue::ArrayQueue;

use super::fsm::{DeclickOutcome, TransitionResult, TransportFsm};
use super::loop_region::{LoopOwner, LoopReader, LoopRegion};
use super::settings::TransportSettings;
use super::state::SeekSlot;
use super::timed::{Schedule, ScheduleFull, TransportCommand};
use std::sync::atomic::AtomicU8;
use tutti_types::At;
use tutti_types::Beat;

pub use super::fsm::MotionState;

/// How a motion change reaches the output.
///
/// The transport's only real fade decision: ramp the gain to zero first, or
/// switch on the next buffer. A parameter rather than a variant per verb, so
/// adding a new motion verb does not double the event list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FadeOut {
    /// Fade the audio out over the declick window. What a user-facing
    /// button should almost always be.
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
/// here; see [`TransportSettings`].
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
    /// Returns a stop with a fade-out, the default stop.
    pub const fn stop() -> Self {
        Self::Stop {
            fade: FadeOut::Declick,
        }
    }

    /// Returns a stop that takes effect on the next buffer, unfaded. For
    /// teardown, and for tests that assert the unfaded path.
    pub const fn stop_unfaded() -> Self {
        Self::Stop {
            fade: FadeOut::Immediate,
        }
    }

    /// Returns a jump to `beat` that keeps the current motion — the
    /// scrub-bar seek.
    pub const fn locate(beat: Beat) -> Self {
        Self::Locate {
            beat,
            fade: FadeOut::Declick,
            then: Then::Keep,
        }
    }

    /// Returns a jump to `beat` that then rolls from there.
    pub const fn play_from(beat: Beat) -> Self {
        Self::Locate {
            beat,
            fade: FadeOut::Declick,
            then: Then::Roll,
        }
    }

    /// Returns a stop that fades out, then returns to `beat` when the fade
    /// completes.
    ///
    /// The Stop button. Sending a stop and a locate as two events applies both
    /// in one callback, so the seek lands immediately and the fade ramps down
    /// audio rendered from the *new* position — protecting nothing.
    pub const fn stop_then_locate(beat: Beat) -> Self {
        Self::Locate {
            beat,
            fade: FadeOut::Declick,
            then: Then::Stop,
        }
    }
}

/// The command queue was full, so `event` never reached the state machine.
///
/// Distinct from the FSM *rejecting* a transition (`Play` while already
/// rolling is a legitimate no-op): this means the event was not sent, which
/// is a bug — the queue holds [`SCHEDULE_CAPACITY`](super::SCHEDULE_CAPACITY)
/// user-driven commands in flight.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QueueFull {
    /// The event that was refused, handed back so a caller can retry it.
    pub event: MotionEvent,
}

impl core::fmt::Display for QueueFull {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "transport command queue full; {:?} not sent", self.event)
    }
}

impl std::error::Error for QueueFull {}

/// The control handle of the transport's motion machine: send and schedule
/// commands, read what the machine published.
///
/// Clone shares every field — this is a handle, not a value. The state
/// machine itself is the `Engine`'s while one runs over
/// this transport, and comes back here when that engine is dropped.
#[derive(Clone)]
pub struct MotionFsm {
    /// Published mirror of the FSM's motion, so the UI can read it without
    /// reaching the engine.
    motion: Arc<AtomicU8>,
    /// Pending absolute jump. Consumed by `TransportClock` once per buffer.
    pub(crate) seek: SeekSlot,
    /// The FSM writes the playhead on a locate, and pausedness tracks motion,
    /// so it needs the settings it publishes into.
    settings: TransportSettings,
    /// Every command, applied by the engine on its frame. See
    /// [`schedule`](Self::schedule).
    timed: Schedule,
    /// The one FSM and loop region, while no [`MotionAudio`] holds them. A
    /// capacity-one queue: [`audio`](Self::audio) pops it, the audio half's
    /// drop pushes it back, so their state outlives the engine that ran
    /// them.
    home: Arc<ArrayQueue<Home>>,
    /// The read side of the loop region the engine publishes.
    loop_region: LoopReader,
}

/// What outlives an engine: the FSM and the loop region.
struct Home {
    fsm: TransportFsm,
    owner: LoopOwner,
}

/// Reports the *published* state only. The FSM is the engine's.
impl core::fmt::Debug for MotionFsm {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MotionFsm")
            .field("motion", &self.motion())
            .field("scheduled", &self.timed.outstanding())
            .field("seek_pending", &self.seek.is_pending())
            .finish_non_exhaustive()
    }
}

impl MotionFsm {
    /// Creates a stopped machine publishing into `settings`, with an empty queue.
    #[must_use]
    pub fn new(settings: TransportSettings) -> Self {
        let home = Arc::new(ArrayQueue::new(1));
        let (owner, loop_region) = LoopOwner::new();
        // A fresh capacity-one queue has room.
        let _ = home.push(Home {
            fsm: TransportFsm::new(),
            owner,
        });
        Self {
            motion: Arc::new(AtomicU8::new(MotionState::Stopped.into())),
            seek: SeekSlot::new(),
            settings,
            timed: Schedule::new(),
            home,
            loop_region,
        }
    }

    /// The loop region the engine holds, as of its last edit: the bounds
    /// (kept while disarmed) and whether looping is armed. A copy of one
    /// write, never the start of one edit and the end of another. Any
    /// thread; wait-free but for a retry while a write is in flight.
    ///
    /// An edit ([`schedule`](Self::schedule) a
    /// [`LoopEdit`](super::LoopEdit)) shows here once the engine has applied
    /// it, so a read right after the call can still see the old region.
    #[must_use]
    pub fn loop_region(&self) -> LoopRegion {
        self.loop_region.read()
    }

    /// The audio half: the FSM, taken out of this handle until the
    /// returned value is dropped. `None` while another holds it (on this
    /// handle or a clone), so there is one FSM per transport.
    pub(crate) fn audio(&self) -> Option<MotionAudio> {
        let Home { fsm, owner } = self.home.pop()?;
        Some(MotionAudio {
            fsm,
            owner: Some(owner),
            motion: Arc::clone(&self.motion),
            seek: self.seek.clone(),
            settings: self.settings.clone(),
            home: Some(Arc::clone(&self.home)),
        })
    }

    /// Requests a transport change **at a time**: a play, stop or seek
    /// ([`MotionEvent`]), a tempo or a loop edit ([`TransportCommand`]).
    /// Lock-free, callable from any thread, never allocates.
    ///
    /// The engine applies it on its frame, sample-accurately
    /// (`Engine::process`):
    ///
    /// - an [`At::Frame`] on the engine's frame clock, the executor's
    ///   (`tutti_graph::Executor::frame`): it tracks device time, advances
    ///   while a re-prepare has the graph suspended, and is rescaled to the
    ///   same wall-clock time on a rate change. A command still in flight
    ///   when the engine adopts a new rate moves to the same wall-clock time
    ///   (nearest frame); one sent after that is read at the new rate;
    /// - an [`At::Beat`] on the first frame at or after that beat once
    ///   playback reaches it;
    /// - an [`At::NextBlock`] at the next block's first frame.
    ///
    /// Commands due on one frame apply in the order they were sent.
    ///
    /// A **declicked** stop or seek (`FadeOut::Declick`) is where a time
    /// pays off: the engine sees it coming and fades the old audio out so it
    /// reaches zero exactly on the command's frame, then fades back in from
    /// there, with no gain step anywhere. The same command sent as `At::NextBlock` (or
    /// [`try_send`](Self::try_send), or late) has no lead: the old audio ends on the
    /// block's first frame and the new one fades in. See the engine's module
    /// docs. A frame
    /// already past, a beat continuous playback already crossed, or a
    /// command past a block's cut bound
    /// ([`MAX_TRANSPORT_CHANGES`](tutti_graph::MAX_TRANSPORT_CHANGES)) lands
    /// at the start of the next block and is counted
    /// ([`late_command_count`](Self::late_command_count)); an `At::NextBlock` needs no
    /// cut and is never late. A beat a seek or loop jumped over waits until
    /// playback reaches it, holding its credit;
    /// [`cancel_scheduled`](Self::cancel_scheduled) takes it back.
    ///
    /// [`try_send`](Self::try_send) is this method at `At::NextBlock`, and the
    /// two share one queue: commands apply in the order they were sent when
    /// they are due on the same frame. This method has no untimed form of its
    /// own, so "whenever" is spelled out as `At::NextBlock`.
    ///
    /// With no engine running over the transport, commands stay in flight,
    /// up to [`SCHEDULE_CAPACITY`](super::SCHEDULE_CAPACITY) (64), and past
    /// that this returns [`ScheduleFull`].
    /// Loop edits share those credits and are not coalesced: send a dragged
    /// brace's edit on release ([`LoopEdit`](super::LoopEdit)).
    ///
    /// # Errors
    ///
    /// Returns [`ScheduleFull`] when
    /// [`SCHEDULE_CAPACITY`](super::SCHEDULE_CAPACITY) commands are in flight:
    /// nothing was sent, and the command is handed back inside it.
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_types::{At, Beat, Frame};
    /// use tutti_transport::{MotionEvent, Transport, TransportCommand};
    ///
    /// let transport = Transport::new(tutti_types::SampleRate(48_000.0));
    /// // Start rolling one second into the stream, stop at beat 16.
    /// transport.motion.schedule(At::Frame(Frame(48_000)), MotionEvent::Play)?;
    /// transport.motion.schedule(At::Beat(Beat(16.0)), MotionEvent::stop())?;
    /// // A tempo edit on the next block.
    /// transport
    ///     .motion
    ///     .schedule(At::NextBlock, TransportCommand::Tempo(tutti_types::Bpm::of(90.0)))?;
    /// assert_eq!(transport.motion.scheduled_outstanding(), 3);
    /// # Ok::<_, tutti_transport::ScheduleFull>(())
    /// ```
    pub fn schedule(
        &self,
        at: At,
        command: impl Into<TransportCommand>,
    ) -> Result<(), ScheduleFull> {
        self.timed.send(at, command.into())
    }

    /// Takes back every command waiting for a frame or a beat that has not
    /// been applied, and frees their credit. [`At::NextBlock`] commands,
    /// [`try_send`](Self::try_send) among them, are not waiting for a time
    /// and stand: a stop sent just before a cancel still stops. A timed
    /// [`LoopEdit`](super::LoopEdit) is taken back too, so the loop region
    /// stays as the engine last applied it. (A rate change does not drop
    /// commands; it only moves `At::Frame` ones.) Takes effect at the
    /// engine's next block.
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
    pub fn late_command_count(&self) -> u64 {
        self.timed.late()
    }

    /// The command queue, for the engine.
    pub(crate) fn timed(&self) -> &Schedule {
        &self.timed
    }

    /// Requests a transition on the next block:
    /// [`schedule`](Self::schedule) at [`At::NextBlock`], through the same
    /// queue and credits.
    ///
    /// `Ok` means only that the event was *queued* — the FSM decides on the
    /// audio thread whether it actually applies, and that decision is not
    /// available synchronously. Observe the outcome by reading
    /// [`MotionFsm::motion`] on a later frame.
    ///
    /// With no engine running over the transport, events stay queued. Past
    /// [`SCHEDULE_CAPACITY`](super::SCHEDULE_CAPACITY) in flight (timed
    /// commands waiting for their time count) it may still use the
    /// [`IMMEDIATE_RESERVE`](super::IMMEDIATE_RESERVE) credits only
    /// `At::NextBlock` commands take, so a backlog of waiting commands cannot
    /// refuse a stop; past those too this returns [`QueueFull`].
    ///
    /// RT-safe: lock-free, no allocation.
    ///
    /// # Errors
    ///
    /// Returns [`QueueFull`] when the command queue is full. The event was
    /// not queued, and is handed back inside the error so it can be retried.
    pub fn try_send(&self, event: MotionEvent) -> Result<(), QueueFull> {
        self.timed
            .send(At::NextBlock, event.into())
            .map_err(|_| QueueFull { event })
    }

    /// Requests several transitions, in order.
    ///
    /// # Errors
    ///
    /// Returns [`QueueFull`] for the first event the full queue refused, and
    /// sends none after it. Events queued before that point still stand, so
    /// the transport may be left partway through the batch.
    pub fn try_send_all(
        &self,
        events: impl IntoIterator<Item = MotionEvent>,
    ) -> Result<(), QueueFull> {
        events.into_iter().try_for_each(|e| self.try_send(e))
    }

    /// The current motion, as published by the engine's last applied command.
    pub fn motion(&self) -> MotionState {
        MotionState::from(self.motion.load(Ordering::Acquire))
    }

    /// Whether the published motion is exactly [`MotionState::Rolling`] —
    /// `false` while scrubbing or mid-fade.
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.motion() == MotionState::Rolling
    }

    /// Whether the published motion is exactly [`MotionState::Stopped`] —
    /// `false` during a fade that has not yet completed.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.motion() == MotionState::Stopped
    }
}

/// The audio half of the motion machine: the FSM as a plain field, and the
/// outputs it publishes into. The engine owns it, in its render state, and
/// every method takes `&mut self`, so nothing else can be inside the FSM at
/// the same time.
///
/// Dropping it hands the FSM back to its [`MotionFsm`], so an engine built
/// later over the same transport resumes the same machine.
pub(crate) struct MotionAudio {
    fsm: TransportFsm,
    /// The loop region. `None` only once handed back.
    owner: Option<LoopOwner>,
    motion: Arc<AtomicU8>,
    seek: SeekSlot,
    settings: TransportSettings,
    /// Where the FSM goes back to; `None` once it has.
    home: Option<Arc<ArrayQueue<Home>>>,
}

impl Drop for MotionAudio {
    fn drop(&mut self) {
        self.hand_back();
    }
}

impl std::fmt::Debug for MotionAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MotionAudio").finish_non_exhaustive()
    }
}

impl MotionAudio {
    /// Hands the FSM back to its [`MotionFsm`] now, rather than at drop.
    /// Only for a holder about to drop this, which must return the FSM
    /// before something else it owns is released (the engine, before its
    /// clock's playhead claim). Idempotent.
    pub fn hand_back(&mut self) {
        if let (Some(home), Some(owner)) = (self.home.take(), self.owner.take()) {
            // Lock-free, so a drop on the audio thread is safe. Never full:
            // the FSM was popped from it, and only this value puts one back.
            let fsm = std::mem::take(&mut self.fsm);
            let _ = home.push(Home { fsm, owner });
        }
    }

    /// The FSM's own motion, which the published mirror follows.
    pub fn motion(&self) -> MotionState {
        self.fsm.motion()
    }

    /// Apply one scheduled command now: a motion change goes through the
    /// state machine.
    pub fn apply(&mut self, command: TransportCommand) {
        match command {
            TransportCommand::Motion(event) => self.step(event),
            // The audio thread cannot publish a map: the command is a scalar
            // the clock keeps until the next publish (doc 020, stage 6).
            TransportCommand::Tempo(bpm) => self.settings.tempo.store(bpm, Ordering::Release),
            TransportCommand::Loop(edit) => {
                if let Some(owner) = self.owner.as_mut() {
                    owner.apply(edit);
                }
            }
        }
    }

    /// The loop region the engine renders under.
    pub fn loop_region(&self) -> LoopRegion {
        self.owner
            .as_ref()
            .map_or_else(LoopRegion::default, LoopOwner::region)
    }

    fn step(&mut self, event: MotionEvent) {
        if let Some(result) = self.fsm.transition(event) {
            let outcome = self.fsm.declick_outcome();
            self.publish(result, outcome);
        }
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

    /// `outcome` is the FSM's parked fade outcome right after the transition
    /// that produced `result`.
    fn publish(&self, result: TransitionResult, outcome: Option<DeclickOutcome>) {
        match result {
            TransitionResult::MotionChanged(motion) if is_declicking(motion) => {
                // A fade retargeted in flight. Its ramp is mid-count and must
                // NOT restart (the gain would step back to full and click),
                // but its new outcome takes effect on the transport now, as a
                // fresh fade's does.
                self.set_mirror(motion);
                if let Some(outcome) = outcome {
                    self.apply_outcome(outcome);
                }
            }
            TransitionResult::MotionChanged(motion) => {
                // A settled state change cancels any fade.
                self.set_motion(motion);
            }
            TransitionResult::DeclickStarted {
                motion,
                on_complete,
            } => {
                // The transport stops or jumps **now**, on the command's
                // frame; the fade is an audio-only concern, the engine's. The
                // motion mirror reads the declick state until the fade completes.
                self.set_mirror(motion);
                self.apply_outcome(on_complete);
            }
            TransitionResult::Located { pos, motion } => {
                self.locate_to(pos);
                self.set_motion(motion);
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

    /// Called by the engine once it has planned a declick's fade: settle the
    /// motion the fade was heading for.
    ///
    /// The transport itself already stopped or jumped when the command was
    /// applied (see `publish`); what is left is the mirror and the parked
    /// outcome.
    /// Driven by the outcome the FSM parked, not by reading the published
    /// mirror back: the mirror is a projection, so dispatching on it lets an
    /// FSM/mirror disagreement pass unnoticed.
    pub fn complete_declick(&mut self) {
        let outcome = self.fsm.complete_declick();
        match outcome {
            None => {}
            Some(DeclickOutcome::Stop) => self.set_motion(MotionState::Stopped),
            Some(DeclickOutcome::Locate { motion, .. }) => self.set_motion(motion),
        }
    }
}

/// Whether `motion` is a fade in progress. A retarget between two declick
/// states must leave the ramp counting; only a settled state clears it.
#[inline]
pub(crate) fn is_declicking(motion: MotionState) -> bool {
    matches!(
        motion,
        MotionState::DeclickToStop | MotionState::DeclickToLocate
    )
}

/// The audio half as the engine's walk drives it, for tests of the machine
/// and of the handle over it.
#[cfg(test)]
pub(crate) mod harness {
    use super::*;
    use crate::timed::ScheduleAudio;

    /// The audio half the engine would own, with the queue its walk applies
    /// from: `drain` applies what was sent, in send order, as a block whose
    /// commands are all due at its first frame.
    pub(crate) struct Audio {
        motion: MotionAudio,
        queue: ScheduleAudio,
    }

    impl Audio {
        pub(crate) fn of(m: &MotionFsm) -> Self {
            Self {
                motion: m.audio().expect("no engine holds it"),
                queue: ScheduleAudio::new(m.timed().clone()),
            }
        }

        pub(crate) fn drain(&mut self) {
            self.queue.gather();
            while let Some(i) =
                (0..self.queue.pending().len()).min_by_key(|&i| self.queue.pending()[i].seq())
            {
                let command = self.queue.take(i, false).command();
                self.motion.apply(command);
            }
        }
    }

    impl std::ops::Deref for Audio {
        type Target = MotionAudio;
        fn deref(&self) -> &MotionAudio {
            &self.motion
        }
    }

    impl std::ops::DerefMut for Audio {
        fn deref_mut(&mut self) -> &mut MotionAudio {
            &mut self.motion
        }
    }
}

#[cfg(test)]
mod tests {
    use super::harness::Audio;
    use super::*;
    use crate::timed::{IMMEDIATE_RESERVE, SCHEDULE_CAPACITY};
    use tutti_types::Beat;

    /// The control handle and the audio half the engine would own.
    fn fsm() -> (MotionFsm, Audio) {
        let m = MotionFsm::new(TransportSettings::new());
        let a = Audio::of(&m);
        (m, a)
    }

    #[test]
    fn send_then_drain_applies_the_transition() {
        let (m, mut a) = fsm();
        assert!(m.is_stopped());

        assert!(m.try_send(MotionEvent::Play).is_ok());
        // Nothing happens until the audio thread drains.
        assert!(m.is_stopped(), "send must not apply the transition itself");

        a.drain();
        assert!(m.is_playing());
    }

    /// One FSM per machine: the audio half is out while held, by this
    /// handle or a clone, and dropping it hands the same FSM back, state and
    /// all.
    ///
    /// Mutations (run): `audio` building a fresh `TransportFsm` instead of
    /// popping `home` → a second half is handed out while one is held →
    /// fails; `MotionAudio::drop` not pushing the FSM back → no half after
    /// the drop → fails; pushing a fresh FSM back instead → the returned
    /// machine is `Stopped`, so `Stop` is a no-op and the mirror stays
    /// `Rolling` → fails.
    #[test]
    fn the_audio_half_is_one_and_comes_back_with_its_state() {
        let (m, mut a) = fsm();
        let clone = m.clone();
        assert!(m.audio().is_none(), "held: no second half");
        assert!(clone.audio().is_none(), "nor through a clone");

        let _ = m.try_send(MotionEvent::Play);
        a.drain();
        drop(a);

        let mut again = Audio::of(&clone);
        let _ = m.try_send(MotionEvent::stop_unfaded());
        again.drain();
        assert!(m.is_stopped(), "the returned FSM knew it was rolling");
    }

    /// A completed fade settles the machine itself, not only the mirror: a
    /// declicked seek while rolling ends rolling, so the next scrub-bar seek
    /// (`Then::Keep`) keeps rolling too.
    ///
    /// Mutation (run): `TransportFsm::complete_declick` taking the outcome
    /// without settling `motion` → the machine stays in `DeclickToLocate`,
    /// the second seek resolves `Keep` to stopped → fails.
    #[test]
    fn a_seek_after_a_completed_declicked_seek_keeps_rolling() {
        let (m, mut a) = fsm();
        let _ = m.try_send(MotionEvent::Play);
        a.drain();
        for beat in [4.0, 8.0] {
            let _ = m.try_send(MotionEvent::locate(Beat(beat)));
            a.drain();
            assert_eq!(m.motion(), MotionState::DeclickToLocate);
            a.complete_declick();
            assert!(m.is_playing(), "rolling after the seek to {beat}");
            assert!(!m.settings.is_paused());
        }
    }

    #[test]
    fn send_reports_a_full_queue() {
        let (m, _a) = fsm();
        for _ in 0..SCHEDULE_CAPACITY + IMMEDIATE_RESERVE {
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

    /// A cancel takes back what waits for a time, never an immediate command:
    /// a stop sent with `try_send` just before it still stops.
    ///
    /// Mutation (run): drop the `At::NextBlock` exemption in
    /// `ScheduleAudio::gather` → the stop is cancelled and the transport
    /// keeps rolling → fails.
    #[test]
    fn a_cancel_leaves_an_immediate_stop_standing() {
        let (m, mut a) = fsm();
        m.try_send(MotionEvent::Play).expect("room");
        a.drain();
        assert!(m.is_playing());
        m.schedule(At::Beat(Beat(1000.0)), MotionEvent::Play)
            .expect("room");
        m.try_send(MotionEvent::stop_unfaded()).expect("room");
        m.cancel_scheduled();
        a.drain();
        assert!(m.is_stopped(), "the immediate stop stood");
        assert_eq!(m.scheduled_outstanding(), 0, "the timed one was taken back");
    }

    /// A backlog of commands waiting for their time cannot refuse a stop:
    /// `try_send` has the immediate reserve past the timed limit, applies in
    /// send order after the waiting ones, and the reserve's own limit still
    /// hands the event back.
    ///
    /// Mutation (run): `Schedule::send` limiting every `At` to
    /// `SCHEDULE_CAPACITY` → the stop is refused → fails; `try_send` turning
    /// a refusal into `Ok` → the ninth immediate command is not reported →
    /// fails.
    #[test]
    fn a_backlog_of_timed_commands_does_not_refuse_a_stop() {
        let (m, mut a) = fsm();
        m.try_send(MotionEvent::Play).expect("room");
        a.drain();
        assert!(m.is_playing());
        for _ in 0..SCHEDULE_CAPACITY {
            m.schedule(At::Beat(Beat(1000.0)), MotionEvent::Play)
                .expect("room");
        }
        for _ in 1..IMMEDIATE_RESERVE {
            m.try_send(MotionEvent::Play).expect("reserve");
        }
        m.try_send(MotionEvent::stop_unfaded())
            .expect("the last reserve credit takes the stop");
        assert_eq!(
            m.try_send(MotionEvent::Play),
            Err(QueueFull {
                event: MotionEvent::Play
            })
        );
        a.drain();
        assert!(m.is_stopped(), "the stop applied, last in send order");
        m.try_send(MotionEvent::Play)
            .expect("the engine took the credits back");
    }

    #[test]
    fn stop_fades_and_stop_now_does_not() {
        let (m, mut a) = fsm();
        let _ = m.try_send(MotionEvent::Play);
        a.drain();

        let _ = m.try_send(MotionEvent::stop());
        a.drain();
        assert_eq!(m.motion(), MotionState::DeclickToStop);

        // The fade completing is what actually stops it.
        a.complete_declick();
        assert!(m.is_stopped());

        let _ = m.try_send(MotionEvent::Play);
        a.drain();
        let _ = m.try_send(MotionEvent::stop_unfaded());
        a.drain();
        assert!(m.is_stopped(), "StopNow stops immediately");
    }

    #[test]
    fn locate_requests_a_seek_and_moves_the_beat() {
        let (m, mut a) = fsm();
        let _ = m.try_send(MotionEvent::locate(Beat(8.0)));
        a.drain();

        assert_eq!(m.seek.take(), Some(Beat(8.0)), "the clock must see a seek");
        assert_eq!(m.settings.beat.load(Ordering::Acquire), 8.0);
    }

    #[test]
    fn locate_and_play_rolls_after_the_jump() {
        let (m, mut a) = fsm();
        let _ = m.try_send(MotionEvent::play_from(Beat(4.0)));
        a.drain();

        assert_eq!(m.seek.take(), Some(Beat(4.0)));
        assert!(m.is_playing());

        // The assertion above passes even when the FSM is desynced from its
        // published mirror — a mirror reading `Rolling` over an FSM holding
        // `Stopped` sends both stop arms into their `None` case, and the
        // transport cannot be stopped at all. Proving it *stops* is what pins
        // the real property.
        let _ = m.try_send(MotionEvent::stop_unfaded());
        a.drain();
        assert!(
            m.is_stopped(),
            "a transport that started rolling must be stoppable"
        );
    }

    /// The Stop button's fade and its return-to-zero are ONE event, and the
    /// transport acts on it at once: the playhead jumps (and stops) on the
    /// command's frame, and the fade only shapes the audio from there (a
    /// fade is an audio-only concern and never delays the transport state a
    /// graph sees; the seek does not wait for the fade). Completing the fade
    /// settles the motion mirror and does **not** jump again.
    ///
    /// Mutation: leave `apply_outcome` out of the `DeclickStarted` arm → no
    /// seek, playhead still 12 → fails. Call `locate_to` again in
    /// `complete_declick` → a second seek is requested → fails.
    #[test]
    fn stop_and_return_moves_the_playhead_at_once_and_fades_the_audio() {
        let (m, mut a) = fsm();
        let _ = m.try_send(MotionEvent::Play);
        a.drain();
        m.settings.set_beat(Beat::new(12.0));

        let _ = m.try_send(MotionEvent::stop_then_locate(Beat(0.0)));
        a.drain();

        assert_eq!(m.motion(), MotionState::DeclickToLocate);
        assert_eq!(m.seek.take(), Some(Beat(0.0)), "the jump lands now");
        assert_eq!(m.settings.beat.load(Ordering::Acquire), 0.0);
        assert!(m.settings.is_paused(), "and the playhead holds there");

        a.complete_declick();
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
        let (m, mut a) = fsm();
        let _ = m.try_send(MotionEvent::Play);
        a.drain();
        let _ = m.try_send(MotionEvent::stop());
        a.drain();
        assert_eq!(m.motion(), MotionState::DeclickToStop);
        assert!(m.settings.is_paused());
        a.complete_declick();
        assert!(m.is_stopped() && m.settings.is_paused());
    }

    /// Pressing Stop while already stopped has nothing to fade, so it returns
    /// to zero immediately — the standard DAW second-press-rewinds idiom.
    #[test]
    fn stop_and_return_from_a_stop_jumps_immediately() {
        let (m, mut a) = fsm();
        m.settings.set_beat(Beat::new(9.0));

        let _ = m.try_send(MotionEvent::stop_then_locate(Beat(0.0)));
        a.drain();

        assert!(m.is_stopped());
        assert_eq!(m.seek.take(), Some(Beat(0.0)));
    }

    #[test]
    fn paused_tracks_motion() {
        let (m, mut a) = fsm();
        assert!(m.settings.paused.load(Ordering::Acquire));

        let _ = m.try_send(MotionEvent::Play);
        a.drain();
        assert!(
            !m.settings.paused.load(Ordering::Acquire),
            "paused is derived from motion, not set separately"
        );
    }

    #[test]
    fn send_all_preserves_order() {
        let (m, mut a) = fsm();
        assert!(
            m.try_send_all([MotionEvent::locate(Beat(16.0)), MotionEvent::Play])
                .is_ok()
        );
        a.drain();

        assert_eq!(m.seek.take(), Some(Beat(16.0)));
        assert!(m.is_playing());
    }

    #[test]
    fn scrub_returns_to_the_previous_motion() {
        let (m, mut a) = fsm();
        let _ = m.try_send(MotionEvent::Play);
        a.drain();

        let _ = m.try_send(MotionEvent::FastForward);
        a.drain();
        assert_eq!(m.motion(), MotionState::FastForward);

        let _ = m.try_send(MotionEvent::EndScrub);
        a.drain();
        assert!(m.is_playing(), "EndScrub restores what was playing before");
    }
}
