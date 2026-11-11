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
use super::tempo_reader::TempoReader;
use super::timed::{ScheduleAudio, Scheduled, TransportCommand};
use super::walk::TempoAhead;
use std::sync::atomic::Ordering;
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
/// let _ = t.motion().audio();
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
    tempo_map: TempoReader,
    /// The publisher's count at the last block that took the map.
    seen: u64,
    /// Whether the map in force changes tempo: the clock then follows it
    /// (doc 020 stage 3), lending it for each step that needs it and
    /// dropping the loan at the end of the step. A constant map leaves this
    /// off and costs nothing.
    stepped: bool,
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
        let mut tempo_map = settings.tempo_reader();
        // A map with steps published before this audio side was taken (an
        // engine built after the project's tempo was set, or rebuilt by a
        // device restart) is followed from the first block, as a later publish
        // would be.
        let (clock, tempo, stepped) = {
            let map = tempo_map.lend();
            let tempo = map.points()[0].tempo;
            let mut clock = TransportClock::new(transport.links(), rate, tempo);
            let stepped = map.constant_tempo().is_none();
            if stepped {
                clock.follow(&map);
            }
            (clock, tempo, stepped)
        };
        Ok(Self {
            clock,
            control: Control::read(&settings, loop_region, tempo),
            tempo_map,
            seen,
            stepped,
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
        self.begin(true)
    }

    /// The tempo this block renders at: the published map's when a publish
    /// landed since the last block, else the one in force. The map is lent
    /// only then, once, and the loan ends here.
    ///
    /// A map that changes tempo is taken here too: the clock re-seats on it at
    /// the current beat (doc 020, section 3), and from then on follows it, the
    /// tempo of the block being the walk's and not this scalar.
    fn tempo_for_block(&mut self) -> Bpm {
        let published = self.settings.map_generation.load(Ordering::Acquire);
        if published != self.seen {
            self.seen = published;
            let map = self.tempo_map.lend();
            match map.constant_tempo() {
                Some(tempo) => {
                    self.stepped = false;
                    return tempo;
                }
                None => {
                    self.stepped = true;
                    self.clock.follow(&map);
                }
            }
        }
        self.control.tempo
    }

    /// Runs `f` on the clock with the map when it changes tempo, lent for the
    /// call and no longer.
    fn with_map<R>(&mut self, f: impl FnOnce(&mut TransportClock, Option<&TempoMap>) -> R) -> R {
        if self.stepped {
            let map = self.tempo_map.lend();
            f(&mut self.clock, Some(&map))
        } else {
            f(&mut self.clock, None)
        }
    }

    fn begin(&mut self, take_seek: bool) -> tutti_graph::Transport {
        let control = self.control;
        self.with_map(|clock, map| clock.begin(&control, take_seek, map))
    }

    /// The tempo changes inside the next `frames` of the block that began
    /// under `from`, and where the block must end if one does not fit
    /// (doc 020, section 2.3). `free` is how many changes the block can still
    /// take: `MAX_TRANSPORT_CHANGES` less those already cut. Moves nothing.
    ///
    /// Nothing for a constant map (the common case: no lend, no walk) and for
    /// a stopped transport. The changes are offsets from the clock's current
    /// frame, so a caller that has cut the block calls this again from its
    /// cut.
    pub fn tempo_ahead(
        &mut self,
        frames: usize,
        from: &tutti_graph::Transport,
        free: usize,
    ) -> TempoAhead {
        if !self.stepped {
            return TempoAhead::NONE;
        }
        self.with_map(|clock, map| clock.ahead(frames, from, map, free))
    }

    /// The transport at the clock's current frame, after
    /// [`advance`](Self::advance) rolled to a tempo change from
    /// [`tempo_ahead`](Self::tempo_ahead): the tempo and segment the walk
    /// took there. The inputs read at the block's start stay.
    pub fn restate(&mut self) -> tutti_graph::Transport {
        self.begin(false)
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
        (applied, self.begin(seeks))
    }

    /// Advances the playhead `frames` under `from`, the transport the last
    /// [`begin_block`](Self::begin_block) or [`fire`](Self::fire) reported.
    pub fn advance(&mut self, frames: usize, from: &tutti_graph::Transport) {
        self.with_map(|clock, map| clock.advance(frames, from, map));
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
            self.with_map(|clock, map| clock.set_sample_rate(rate, map));
            self.schedule.rescale(rate.get() / was.get());
        }
    }

    /// Jumps the published playhead to `beat`, for a host that drives no
    /// engine (a headless tool, a test): a jump to a reader, so the segment
    /// generation moves on first.
    ///
    /// ```
    /// use tutti_types::{Beat, SampleRate};
    /// use tutti_transport::{Timeline, Transport};
    /// let transport = Transport::new(SampleRate(48_000.0));
    /// let mut audio = transport.audio(SampleRate(48_000.0)).expect("the only audio side");
    /// audio.set_playhead(Beat(4.0));
    /// assert_eq!(transport.beat(), Beat(4.0));
    /// // A jump: a reader keyed on the segment re-seats.
    /// assert_eq!(transport.segment_generation(), 1);
    /// // The audio side is held: nobody else may store.
    /// assert!(transport.audio(SampleRate(48_000.0)).is_err());
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
    #[test]
    fn the_audio_side_is_taken_once_and_comes_back() {
        let t = Transport::new(SampleRate::new(48_000.0));
        let first = t.audio(SampleRate::new(48_000.0)).expect("first");
        assert_eq!(
            t.clone().audio(SampleRate::new(48_000.0)).err(),
            Some(AudioTaken)
        );
        drop(first);
        t.audio(SampleRate::new(48_000.0))
            .expect("handed back on drop");
    }

    /// A command still waiting for its beat outlives the audio side that
    /// gathered it, credit and all: the next one (a device restart) finds it
    /// pending, fires it, and the credits return to zero.
    #[test]
    fn a_waiting_command_outlives_its_audio_side() {
        let t = Transport::new(SampleRate::new(48_000.0));
        t.motion
            .schedule(At::Beat(tutti_types::Beat(1000.0)), MotionEvent::Play)
            .expect("room");
        let mut first = t.audio(SampleRate::new(48_000.0)).expect("first");
        first.gather();
        assert_eq!(first.pending().len(), 1);
        drop(first);

        let mut next = t
            .audio(SampleRate::new(48_000.0))
            .expect("handed back on drop");
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
    #[test]
    fn firing_a_command_frees_its_credit_and_counts_it_late() {
        let t = Transport::new(SampleRate::new(48_000.0));
        let mut a = t.audio(SampleRate::new(48_000.0)).expect("first");
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
    #[test]
    fn a_published_constant_map_is_the_next_blocks_tempo() {
        let t = Transport::new(SampleRate::new(48_000.0));
        let mut a = t.audio(SampleRate::new(48_000.0)).expect("first");
        assert_eq!(a.begin_block().tempo, Bpm::of(120.0));
        t.settings.set_tempo_map(constant(97.25));
        assert_eq!(a.begin_block().tempo, Bpm::of(97.25));
        assert_eq!(a.begin_block().tempo, Bpm::of(97.25), "and stays");
        t.settings.set_tempo(Bpm::of(143.5));
        assert_eq!(a.begin_block().tempo, Bpm::of(143.5), "set_tempo is a map");
    }

    /// A rolling transport over `points` (steps), at `rate`.
    fn rolling(rate: f64, points: &[(f64, f64)]) -> (Transport, TransportAudio) {
        rolling_on(rate, steps(points))
    }

    /// A rolling transport over `map`, at `rate`.
    fn rolling_on(rate: f64, map: std::sync::Arc<TempoMap>) -> (Transport, TransportAudio) {
        let t = Transport::new(SampleRate::new(rate));
        t.settings.set_tempo_map(map);
        let mut a = t.audio(SampleRate::new(rate)).expect("first");
        t.motion
            .schedule(At::NextBlock, MotionEvent::Play)
            .expect("room");
        a.gather();
        let _ = a.begin_block();
        let _ = a.fire(0, false);
        (t, a)
    }

    /// Roll `frames` as one block, and report the transport at its start.
    fn roll(a: &mut TransportAudio, frames: usize) -> tutti_graph::Transport {
        let t = a.begin_block();
        a.advance(frames, &t);
        t
    }

    /// The look ahead finds the step on its frame and with its transport: 100
    /// frames before beat 4 of 120 to 60 BPM at 48 kHz, a 512-frame block has
    /// one change at offset 100, on beat 4 exactly at 60 BPM. With no room the
    /// block ends there; a constant map has none.
    #[test]
    fn the_look_ahead_finds_a_step_and_cuts_a_block_that_cannot_hold_it() {
        let (_t, mut a) = rolling(48_000.0, &[(0.0, 120.0), (4.0, 60.0)]);
        let _ = roll(&mut a, 96_000 - 100);
        let t = a.begin_block();
        let ahead = a.tempo_ahead(512, &t, 8);
        assert_eq!(ahead.limit, None);
        let [change] = ahead.changes.as_slice() else {
            panic!("one change, got {:?}", ahead.changes);
        };
        assert_eq!(change.at.index(), 100);
        assert_eq!(change.to.tempo, Bpm::of(60.0));
        assert_eq!(change.to.beat(), Beat(4.0));
        assert_eq!(change.to.tempo, Bpm::of(60.0));
        let full = a.tempo_ahead(512, &t, 0);
        assert!(full.changes.is_empty());
        assert_eq!(
            full.limit,
            Some(100),
            "the block ends where it would be 1 too many"
        );
        // Moving nothing: the same look again.
        assert_eq!(a.tempo_ahead(512, &t, 8).changes, ahead.changes);

        let t = Transport::new(SampleRate::new(48_000.0));
        let mut c = t.audio(SampleRate::new(48_000.0)).expect("first");
        let blk = c.begin_block();
        assert!(c.tempo_ahead(512, &blk, 8).changes.is_empty());
    }

    /// A map published while rolling is taken at the next block start without
    /// moving the playhead: the beat of that block is the beat the last one
    /// ended on (bit for bit), at the tempo the map has there, and the step
    /// that follows lands on its frame.
    #[test]
    fn a_swapped_in_step_map_re_seats_at_the_current_beat() {
        let t = Transport::new(SampleRate::new(48_000.0));
        let mut a = t.audio(SampleRate::new(48_000.0)).expect("first");
        t.motion
            .schedule(At::NextBlock, MotionEvent::Play)
            .expect("room");
        a.gather();
        let _ = a.begin_block();
        let _ = a.fire(0, false);
        let _ = roll(&mut a, 24_000); // beat 1 at 120
        let ended = {
            let blk = a.begin_block();
            blk.beat()
        };
        assert_eq!(ended, Beat(1.0));
        t.settings.set_tempo_map(steps(&[(0.0, 90.0), (2.0, 60.0)]));
        let blk = a.begin_block();
        assert_eq!(blk.beat(), Beat(1.0), "no jump");
        assert_eq!(blk.tempo, Bpm::of(90.0), "the map's tempo here");
        // Beat 2 is 32 000 frames on at 90 BPM.
        let _ = roll(&mut a, 31_999);
        assert_eq!(a.begin_block().tempo, Bpm::of(90.0));
        let _ = roll(&mut a, 1);
        let blk = a.begin_block();
        assert_eq!(blk.tempo, Bpm::of(60.0));
        assert_eq!(blk.beat(), Beat(2.0));
    }

    fn steps(points: &[(f64, f64)]) -> std::sync::Arc<TempoMap> {
        use tutti_tempo::{Shape, TempoPoint};
        std::sync::Arc::new(
            TempoMap::new(points.iter().map(|&(beat, bpm)| TempoPoint {
                beat: Beat(beat),
                tempo: Bpm::of(bpm),
                shape: Shape::Step,
            }))
            .expect("a map"),
        )
    }

    /// A rate change re-seats at the current beat on the map's timeline at the
    /// new rate: beat 6 of a 60-to-240 step at beat 8 is 2 beats of 60 BPM, or
    /// 192 000 frames, from the step at 96 kHz (and the clock had counted 144 000
    /// at 48 kHz to get here).
    #[test]
    fn a_rate_change_re_seats_on_the_maps_timeline() {
        let (_t, mut a) = rolling(48_000.0, &[(0.0, 120.0), (4.0, 60.0), (8.0, 240.0)]);
        // 4 beats at 120, then 2 at 60: beat 6.
        let _ = roll(&mut a, 96_000 + 96_000);
        assert_eq!(a.begin_block().beat(), Beat(6.0));
        a.follow_rate(SampleRate(96_000.0));
        let blk = a.begin_block();
        assert_eq!((blk.beat(), blk.tempo), (Beat(6.0), Bpm::of(60.0)));
        let _ = roll(&mut a, 191_999);
        assert_eq!(a.begin_block().tempo, Bpm::of(60.0));
        let _ = roll(&mut a, 1);
        let blk = a.begin_block();
        assert_eq!((blk.beat(), blk.tempo), (Beat(8.0), Bpm::of(240.0)));
    }

    /// A rate change inside a ramp re-seats at the current beat on the map's
    /// timeline at the new rate, and takes the chord of the cell it lands in
    /// there (doc 020, section 10: `chord_cell` takes the new rate). The ramp is
    /// 120 to 60 BPM over 4 beats (`r0` 2, `r1` 1 beats/s, `T = 8 / 3` s,
    /// `a = -3 / 8`, `B(s) = 2 s - 3 s^2 / 16`); after 60 000 frames at 48 kHz
    /// the playhead keeps its beat, the map frame at 96 kHz is the first at or
    /// after `s(beat) x 96 000` (`s = (sqrt(4 - 3 b / 4) - 2) / a`), the block's
    /// tempo is the chord of `B` over that frame's 128-frame cell, and at the
    /// cell's edge the playhead is on `B` exactly, at the next chord.
    #[test]
    fn a_rate_change_inside_a_ramp_takes_the_chord_at_the_new_rate() {
        use tutti_tempo::{Shape, TempoPoint};
        let map = std::sync::Arc::new(
            TempoMap::new([
                TempoPoint {
                    beat: Beat(0.0),
                    tempo: Bpm::of(120.0),
                    shape: Shape::RampTime,
                },
                TempoPoint {
                    beat: Beat(4.0),
                    tempo: Bpm::of(60.0),
                    shape: Shape::Step,
                },
            ])
            .expect("a map"),
        );
        let b_of = |s: f64| 2.0 * s - 3.0 * s * s / 16.0;
        let (_t, mut a) = rolling_on(48_000.0, map);
        let _ = roll(&mut a, 60_000);
        let beat = a.begin_block().beat();
        assert!(beat.get() > 1.0 && beat.get() < 3.0, "mid-ramp: {beat:?}");

        a.follow_rate(SampleRate(96_000.0));
        let blk = a.begin_block();
        assert_eq!(blk.beat(), beat, "no jump");
        let s = ((4.0 - 0.75 * beat.get()).sqrt() - 2.0) / (-3.0 / 8.0);
        let m = tutti_types::first_frame_at_or_after(s * 96_000.0) as u64;
        let k = m / 128;
        let at = |frame: u64| b_of(frame as f64 / 96_000.0);
        let chord = |k: u64| (at((k + 1) * 128) - at(k * 128)) * 60.0 * 96_000.0 / 128.0;
        assert!((blk.tempo.get() - chord(k)).abs() < 1e-6, "{:?}", blk.tempo);
        // The 48 kHz cell of the same beat has another chord.
        assert!((blk.tempo.get() - chord(k / 2 + k % 2)).abs() > 1e-3);

        // To the cell's edge: on the map's beat, at the next chord.
        let _ = roll(&mut a, ((k + 1) * 128 - m) as usize);
        let blk = a.begin_block();
        assert!((blk.beat().get() - at((k + 1) * 128)).abs() < 1e-10);
        assert!((blk.tempo.get() - chord(k + 1)).abs() < 1e-6);
    }

    /// A map has no hysteresis: a step from 120 to 120.0005 BPM (inside the
    /// clock's 0.001 band for a scalar tempo) is taken on its frame.
    #[test]
    fn a_map_is_followed_without_the_scalar_hysteresis() {
        let (_t, mut a) = rolling(48_000.0, &[(0.0, 120.0), (1.0, 120.0005)]);
        let _ = roll(&mut a, 24_000);
        assert_eq!(a.begin_block().tempo, Bpm::of(120.0005));
    }

    /// A map with steps published before the audio side is taken (an engine
    /// built after the project set its tempo, or rebuilt by a device restart)
    /// is followed from its first block: the step lands on its frame.
    #[test]
    fn a_map_published_before_the_audio_side_is_taken_is_followed() {
        let (t, a) = rolling(48_000.0, &[(0.0, 120.0), (4.0, 60.0)]);
        drop(a);
        let mut a = t
            .audio(SampleRate::new(48_000.0))
            .expect("handed back on drop");
        let mut seen = Vec::new();
        for _ in 0..3 {
            let blk = a.begin_block();
            seen.push((blk.tempo, blk.beat()));
            a.advance(96_000, &blk);
        }
        // The seat survives the restart on the beat the clock stood on (0:
        // the drop kept the machine rolling at the start).
        assert_eq!(seen[0].0, Bpm::of(120.0));
        assert_eq!(
            seen[1],
            (Bpm::of(60.0), Beat(4.0)),
            "the step, on its frame"
        );
        assert_eq!(seen[2].1, Beat(6.0), "96 000 frames at 60 BPM are 2 beats");
    }
}
