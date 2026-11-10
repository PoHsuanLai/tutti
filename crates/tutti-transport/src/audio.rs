//! The audio-thread side of a transport, taken once.
//!
//! [`Transport::audio`] hands out a [`TransportAudio`]: the motion machine,
//! the playhead clock, the timed commands and the untimed inputs a block
//! reads, as one value owned by whoever renders under the transport. Its
//! methods are the block's steps in the order they are legal, and everything
//! they work on is private, so the engine cannot read the inputs twice,
//! settle a fade late, free a command's credit by hand or hold the playhead
//! writer apart from the machine.

use super::clock::{Control, TransportClock};
use super::handle::Transport;
use super::motion::{MotionAudio, is_declicking};
use super::settings::TransportSettings;
use super::timed::{ScheduleAudio, Scheduled, TransportCommand};
use std::sync::atomic::Ordering;
use tutti_rt::Reader;
use tutti_tempo::TempoMap;
use tutti_types::{Beat, Bpm, SampleRate};

/// [`Transport::audio`] refused: another [`TransportAudio`] over this
/// transport (or a clone of it) is alive. Drop it (the engine that holds it)
/// before building another.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioTaken;

impl std::fmt::Display for AudioTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the transport's audio side is held by another engine")
    }
}

impl std::error::Error for AudioTaken {}

/// What applying a command did to the output: the transport has moved at once
/// either way, and a declicked stop or seek leaves the fade to the caller,
/// as gain, on the frame the command landed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use]
pub enum Applied {
    /// No fade: the output continues.
    Plain,
    /// A declicked stop or seek landed: the fade-out ends, and the fade-in
    /// starts, here.
    Declicked,
}

impl Applied {
    /// Whether a declicked stop or seek landed.
    #[must_use]
    pub fn declicked(self) -> bool {
        self == Self::Declicked
    }
}

/// The audio thread's transport: see the module docs.
///
/// Not `Clone`. Dropping it hands the motion machine (with the loop region
/// and the claim on the playhead) back to the [`Transport`], so a later
/// engine resumes the same state. What it is built from stays out of reach,
/// so a second audio side cannot be assembled around it:
///
/// ```compile_fail,E0624
/// let t = tutti_transport::Transport::new(48_000.0);
/// let _ = t.motion.audio();
/// ```
///
/// ```compile_fail,E0277
/// fn is_clone<T: Clone>() {}
/// is_clone::<tutti_transport::TransportAudio>();
/// ```
pub struct TransportAudio {
    motion: MotionAudio,
    clock: TransportClock,
    /// The untimed inputs, read at a block's start and carried across the
    /// commands in it.
    control: Control,
    schedule: ScheduleAudio,
    settings: TransportSettings,
    /// The published tempo map, lent once per block and only when
    /// [`seen`](Self::seen) is behind the publisher's count.
    tempo_map: Reader<TempoMap>,
    /// The publisher's count at the last block that took the map.
    seen: u64,
}

impl std::fmt::Debug for TransportAudio {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportAudio").finish_non_exhaustive()
    }
}

/// The commands still waiting go back first, then the machine: the next
/// audio side can only be taken once the machine is home, and finds the
/// list there.
impl Drop for TransportAudio {
    fn drop(&mut self) {
        self.schedule.hand_back();
        self.motion.hand_back();
    }
}

impl TransportAudio {
    pub(crate) fn take(transport: &Transport, rate: SampleRate) -> Result<Self, AudioTaken> {
        let motion = transport.motion.audio().ok_or(AudioTaken)?;
        let settings = transport.settings.clone();
        let loop_region = motion.loop_region();
        // The count first, then the tempo: a publish between the two is taken
        // at the first block, never missed.
        let seen = settings.map_generation.load(Ordering::Acquire);
        let tempo = settings.tempo();
        Ok(Self {
            clock: TransportClock::new(transport.links(), rate),
            control: Control::read(&settings, loop_region, tempo),
            tempo_map: settings.tempo_reader(),
            seen,
            schedule: ScheduleAudio::new(transport.motion.timed().clone()),
            motion,
            settings,
        })
    }

    /// Moves the commands the control thread sent
    /// ([`schedule`](super::MotionFsm::schedule), and
    /// [`try_send`](super::MotionFsm::try_send), which is `At::NextBlock`)
    /// into the pending list, dropping what it cancelled. Call at the start
    /// of each walk.
    pub fn gather(&mut self) {
        self.schedule.gather();
    }

    /// The scheduled commands sent and not yet applied.
    #[must_use]
    pub fn pending(&self) -> &[Scheduled] {
        self.schedule.pending()
    }

    /// Reads the untimed inputs for a block and reports the transport at its
    /// first frame. Once per block: a store from the control thread lands at
    /// the next block, never at a cut in this one.
    pub fn begin_block(&mut self) -> tutti_graph::Transport {
        let tempo = self.tempo_for_block();
        self.control = Control::read(&self.settings, self.motion.loop_region(), tempo);
        self.clock.begin(&self.control, true)
    }

    /// The tempo this block renders at: the published map's when a publish
    /// landed since the last block, else the one in force (which a timed
    /// `Tempo` command may have moved). The map is lent only then, once, and
    /// the loan ends here.
    fn tempo_for_block(&mut self) -> Bpm {
        let published = self.settings.map_generation.load(Ordering::Acquire);
        if published != self.seen {
            self.seen = published;
            if let Some(tempo) = self.tempo_map.lend().constant_tempo() {
                return tempo;
            }
        }
        self.control.tempo
    }

    /// Applies pending command `i` (late or not: counted when late), then
    /// reports the transport from this frame on under it. The command's own
    /// effect only; the inputs read at the block's start stay.
    pub fn fire(&mut self, i: usize, late: bool) -> (Applied, tutti_graph::Transport) {
        let cmd = self.schedule.take(i, late).command();
        self.motion.apply(cmd);
        let applied = self.settle();
        self.control.apply(&cmd, &self.settings);
        // Only a motion command can have requested a seek.
        let seeks = matches!(cmd, TransportCommand::Motion(_));
        (applied, self.clock.begin(&self.control, seeks))
    }

    /// Advances the playhead `frames` under `from`, the transport the last
    /// [`begin_block`](Self::begin_block) or [`fire`](Self::fire) reported.
    pub fn advance(&mut self, frames: usize, from: &tutti_graph::Transport) {
        self.clock.advance(frames, from);
    }

    /// Publishes the playhead after the block it belongs to is rendered.
    pub fn publish_position(&self) {
        self.clock.publish_position();
    }

    /// Follows the executor's rate: a new segment at `rate`, and the timed
    /// commands written in frames move to the same wall-clock time. A no-op
    /// at the rate the clock already runs at.
    pub fn follow_rate(&mut self, rate: SampleRate) {
        let was = self.clock.sample_rate();
        if rate != was {
            self.clock.set_sample_rate(rate);
            self.schedule.rescale(rate.get() / was.get());
        }
    }

    /// Jumps the published playhead to `beat`, for a host that drives no
    /// engine (a headless tool, a test): a jump to a reader, so the segment
    /// generation moves on first.
    ///
    /// ```
    /// use tutti_types::Beat;
    /// use tutti_transport::{Timeline, Transport};
    /// let transport = Transport::new(48_000.0);
    /// let mut audio = transport.audio(48_000.0).expect("the only audio side");
    /// audio.set_playhead(Beat(4.0));
    /// assert_eq!(transport.beat(), Beat(4.0));
    /// // A jump: a reader keyed on the segment re-seats.
    /// assert_eq!(transport.segment_generation(), 1);
    /// // The audio side is held: nobody else may store.
    /// assert!(transport.audio(48_000.0).is_err());
    /// ```
    pub fn set_playhead(&mut self, beat: impl Into<Beat>) {
        self.clock.set_playhead(beat.into());
    }

    /// A declicked stop or seek parks the machine in a fade state; the fade
    /// is the caller's, as gain, so settle the machine now.
    fn settle(&mut self) -> Applied {
        if is_declicking(self.motion.motion()) {
            self.motion.complete_declick();
            Applied::Declicked
        } else {
            Applied::Plain
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MotionEvent, Timeline};
    use tutti_types::{At, Frame};

    fn constant(bpm: f64) -> std::sync::Arc<TempoMap> {
        std::sync::Arc::new(TempoMap::constant(Bpm::of(bpm)).expect("a tempo"))
    }

    /// One audio side per transport, through any clone, and it comes back
    /// with the machine when it drops.
    ///
    /// Mutation (run): `MotionAudio::hand_back` not pushing the home back →
    /// the second take after the drop is refused → fails.
    #[test]
    fn the_audio_side_is_taken_once_and_comes_back() {
        let t = Transport::new(48_000.0);
        let first = t.audio(48_000.0).expect("first");
        assert_eq!(t.clone().audio(48_000.0).err(), Some(AudioTaken));
        drop(first);
        t.audio(48_000.0).expect("handed back on drop");
    }

    /// A command still waiting for its beat outlives the audio side that
    /// gathered it, credit and all: the next one (a device restart) finds it
    /// pending, fires it, and the credits return to zero.
    ///
    /// Mutation (run): `TransportAudio::drop` not handing the pending list
    /// back → the new side has no pending command and the credit stays held
    /// → fails.
    #[test]
    fn a_waiting_command_outlives_its_audio_side() {
        let t = Transport::new(48_000.0);
        t.motion
            .schedule(At::Beat(tutti_types::Beat(1000.0)), MotionEvent::Play)
            .expect("room");
        let mut first = t.audio(48_000.0).expect("first");
        first.gather();
        assert_eq!(first.pending().len(), 1);
        drop(first);

        let mut next = t.audio(48_000.0).expect("handed back on drop");
        next.gather();
        assert_eq!(next.pending().len(), 1, "carried over");
        assert_eq!(t.motion.scheduled_outstanding(), 1);
        assert!(!t.is_rolling());
        let _ = next.fire(0, false);
        assert_eq!(t.motion.scheduled_outstanding(), 0);
        assert!(t.is_rolling(), "the carried command fired");
    }

    /// Firing a command gives its credit back and counts it late exactly
    /// when it is; nothing else frees a credit.
    ///
    /// Mutation (run): `ScheduleAudio::take` skipping `release` → the
    /// outstanding count stays 2 → fails; skipping `count_late` → 0 → fails.
    #[test]
    fn firing_a_command_frees_its_credit_and_counts_it_late() {
        let t = Transport::new(48_000.0);
        let mut a = t.audio(48_000.0).expect("first");
        t.motion
            .schedule(At::Frame(Frame(0)), MotionEvent::Play)
            .expect("room");
        t.motion
            .schedule(At::NextBlock, MotionEvent::stop_unfaded())
            .expect("room");
        a.gather();
        assert_eq!(a.pending().len(), 2);
        assert_eq!(t.motion.scheduled_outstanding(), 2);
        let _ = a.fire(0, true);
        assert_eq!(t.motion.scheduled_outstanding(), 1);
        assert_eq!(t.motion.late_command_count(), 1);
        let _ = a.fire(0, false);
        assert_eq!(t.motion.scheduled_outstanding(), 0);
        assert_eq!(t.motion.late_command_count(), 1);
        assert!(!t.is_rolling(), "play then stop");
    }

    /// The block renders at the published map's tempo, from the first block
    /// after the publish, and stays there.
    ///
    /// Mutation (run): `tempo_for_block` returning `self.control.tempo`
    /// without lending -> the block keeps 120 -> fails.
    #[test]
    fn a_published_constant_map_is_the_next_blocks_tempo() {
        let t = Transport::new(48_000.0);
        let mut a = t.audio(48_000.0).expect("first");
        assert_eq!(a.begin_block().tempo, Bpm::of(120.0));
        t.settings.set_tempo_map(constant(97.25)).expect("constant");
        assert_eq!(a.begin_block().tempo, Bpm::of(97.25));
        assert_eq!(a.begin_block().tempo, Bpm::of(97.25), "and stays");
        t.settings.set_tempo(Bpm::of(143.5));
        assert_eq!(a.begin_block().tempo, Bpm::of(143.5), "set_tempo is a map");
    }

    /// A timed `Tempo` command is applied on the audio thread, which cannot
    /// publish: its tempo holds across blocks until the next publish, which
    /// wins.
    ///
    /// Mutation (run): `tempo_for_block` lending the map every block -> the
    /// command's 90 reverts to the map's 120 at the next block -> fails.
    #[test]
    fn a_timed_tempo_holds_until_the_next_publish() {
        let t = Transport::new(48_000.0);
        let mut a = t.audio(48_000.0).expect("first");
        t.motion
            .schedule(At::NextBlock, TransportCommand::Tempo(Bpm::of(90.0)))
            .expect("room");
        a.gather();
        let _ = a.begin_block();
        let (_, after) = a.fire(0, false);
        assert_eq!(after.tempo, Bpm::of(90.0));
        assert_eq!(a.begin_block().tempo, Bpm::of(90.0), "held");
        assert_eq!(t.settings.tempo(), Bpm::of(90.0), "readers see it");
        t.settings.set_tempo(Bpm::of(100.0));
        assert_eq!(a.begin_block().tempo, Bpm::of(100.0), "a publish wins");
    }
}
