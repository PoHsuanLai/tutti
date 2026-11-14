//! [`ClockFollower`]: what a transport that follows an outside MIDI clock does,
//! as a pure state machine.
//!
//! One follower reads one source's UMPs, each with the time it arrived, and
//! says what the source asks of a transport: a tempo, play and stop, a locate,
//! and whether it is locked to the source ([`ClockEvent`]). It holds no clock
//! and no transport: whoever owns the transport places each event (the session,
//! in `tutti-session`).
//!
//! **Time** is [`Seconds`] on one clock, whose zero the caller picks (the
//! session's is graph frame 0): every `at` and `now` of a follower is read on
//! it, and only differences are read. It is the base of [`MidiClockDecoder`] and
//! [`MtcDecoder`] too, so a source stamped by a hardware clock and one stamped
//! by the graph's frame count take the same call.
//!
//! A follower is one of two state machines, one per [`ClockKind`], each holding
//! only its own state.
//!
//! # Beat Clock
//!
//! [`ClockKind::BeatClock`] (`BeatClockFollower`) reads System Real-Time Timing Clock, Start, Continue
//! and Stop and the System Common Song Position Pointer (M1 pp.27 to 31), through
//! [`MidiClockDecoder`].
//!
//! - **Tempo** is the decoder's least-squares fit of the tick spacing. It is
//!   announced when the clock locks, and again only when it has moved by
//!   [`TEMPO_STEP`](ClockFollower::TEMPO_STEP) and a quarter note of ticks has
//!   passed since the last: a source whose estimate wanders by jitter grows a
//!   tempo map by one point a beat at most, and by none while it holds still.
//! - **Start** locates to beat 0 and plays; **Continue** locates to where the
//!   decoder counted to and plays; **Stop** stops and keeps the position.
//! - **Song Position Pointer** locates to its value in sixteenth notes: six
//!   clocks is a sixteenth, so a pointer of `n` is beat `n / 4`.
//! - The clock is **locked** per [`MidiClockDecoder::locked`], and **unlocked**
//!   by a gap in the ticks or by [`MidiClockDecoder::DROPOUT`] of silence, which
//!   also stops a transport the source had started.
//!
//! # MIDI Time Code
//!
//! [`ClockKind::Mtc`] (`MtcFollower`) reads Quarter Frames through [`MtcDecoder`] and the Full
//! Message (Universal Real Time `01 01`, M1 p.53 and T-9) through
//! `tutti-midi-protocol`'s reader. MTC has no Start and no Stop: time is running
//! once Quarter Frames arrive after a Full Message, and stopped when they stop.
//!
//! - A completed cycle of eight Quarter Frames is a position, two frames after
//!   the last. A cycle is **consistent** when it is two frames after the one
//!   before at the same rate, which is what a generator running forward sends
//!   and needs no arrival time to judge.
//! - The follower is **locked** after [`MTC_LOCK_CYCLES`](ClockFollower::MTC_LOCK_CYCLES)
//!   consistent cycles, and plays, locating to the position first. A cycle that
//!   is not consistent while it plays is a jump of the source, and locates.
//! - A Full Message **locates** and leaves the motion as it was: the generator
//!   pauses for the receiver to reach the position and resumes with Quarter
//!   Frames. The next cycle after it is taken as a first one, not a jump.
//! - **Unlocked** when [`MTC_DROPOUT`](ClockFollower::MTC_DROPOUT) passes with no
//!   Quarter Frame or Full Message, which also stops the transport.

use tutti_midi_protocol::{Addressed, Read, SmpteTimecode, Universal, read};
use tutti_types::{Beat, Bpm, Seconds};
use tutti_ump::{Message, SysexAssembler, SysexStep, System, Ump};

use super::{ClockTransportState, MidiClockDecoder, MtcDecoder};

/// Which outside clock a [`ClockFollower`] reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ClockKind {
    /// MIDI Beat Clock: Timing Clock, Start, Continue, Stop and Song Position
    /// Pointer. A musical position and a tempo.
    BeatClock,
    /// MIDI Time Code: Quarter Frames and the Full Message. A position in
    /// time, and no tempo.
    Mtc,
}

/// A position a source locates to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClockPosition {
    /// A musical position: Beat Clock counts beats.
    Beat(Beat),
    /// A position in time since `00:00:00:00`: MTC counts frames.
    Time(Seconds),
}

/// What a source asks of the transport, or says of its own state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClockEvent {
    /// The tempo the source's clock fits, in beats per minute.
    Tempo(Bpm),
    /// Roll from the position (a [`Locate`](Self::Locate) comes first when the
    /// source says where).
    Play,
    /// Stop where the transport stands.
    Stop,
    /// The source is at `position` as of `at`, the arrival of the message that
    /// said so. A transport that applies it later is `later - at` behind a
    /// source that kept rolling.
    Locate {
        /// Where the source is.
        position: ClockPosition,
        /// When the message arrived, on the follower's clock.
        at: Seconds,
    },
    /// The follower locked to the source.
    Locked,
    /// The follower lost the source: a gap, or silence for the kind's dropout.
    Unlocked,
}

/// Follows one outside MIDI clock; the rules are in the module docs.
///
/// ```
/// use tutti_midi_runtime::{ClockEvent, ClockFollower, ClockKind};
/// use tutti_types::{Bpm, Seconds};
/// use tutti_ump::{Message, MidiGroup, System, Ump};
///
/// let mut follower = ClockFollower::new(ClockKind::BeatClock);
/// let tick = Ump::from(Message::System(MidiGroup::FIRST, System::TimingClock));
/// let mut heard = Vec::new();
/// // 120 bpm is 24 ticks per half second.
/// for i in 0..48 {
///     let at = Seconds::new(f64::from(i) * 0.5 / 24.0);
///     follower.step(tick, at, &mut |e| heard.push(e));
/// }
/// assert!(heard.contains(&ClockEvent::Locked));
/// assert!(heard.iter().any(|e| matches!(e, ClockEvent::Tempo(t) if (t.get() - 120.0).abs() < 1e-6)));
/// ```
#[derive(Debug)]
pub struct ClockFollower(Follower);

/// The follower of a kind: the state of that kind alone.
#[derive(Debug)]
enum Follower {
    BeatClock(BeatClockFollower),
    Mtc(MtcFollower),
}

/// The Beat Clock state machine.
#[derive(Debug)]
struct BeatClockFollower {
    decoder: MidiClockDecoder,
    /// The tempo last announced, and the ticks since.
    announced: Option<Bpm>,
    ticks_since_announce: usize,
    locked: bool,
}

/// The MTC state machine.
#[derive(Debug)]
struct MtcFollower {
    decoder: MtcDecoder,
    sysex: SysexAssembler<[u8; SYSEX_ROOM]>,
    /// The position of the last completed cycle.
    last_cycle: Option<SmpteTimecode>,
    /// Consecutive consistent cycles.
    run: usize,
    locked: bool,
    /// Whether a play was announced and no stop since.
    running: bool,
    last_heard: Option<Seconds>,
}

/// Longest Universal message read: the Full Message is ten bytes with its
/// delimiters. Room for one open message and one finished.
const SYSEX_MAX: usize = 16;
const SYSEX_ROOM: usize = 2 * SYSEX_MAX + 8;

impl ClockFollower {
    /// The smallest move of the tempo estimate announced again: 0.1 bpm.
    /// Tutti's choice: over a full window the fit's own noise is a hundredth of
    /// a bpm for a millisecond of arrival jitter, so a move this size is the
    /// source's tempo moving; and it is a tenth of a percent at 120 bpm, a beat
    /// of drift in a thousand.
    pub const TEMPO_STEP: Bpm = Bpm::of(0.1);

    /// Consistent Quarter Frame cycles that lock the follower: 3, which is
    /// two intervals of agreement over six frames, twelve to ninety-six
    /// quarter frames by rate (tutti's choice; the spec gives none, and says a
    /// reader needs a full cycle of eight messages to know the time).
    pub const MTC_LOCK_CYCLES: usize = 3;

    /// How long MTC may be silent before the follower unlocks and stops: a
    /// quarter of a second (tutti's choice), three cycles at 24 frames per
    /// second, the slowest rate, and a hundred times the spacing of its Quarter
    /// Frames.
    pub const MTC_DROPOUT: Seconds = Seconds::new(0.25);

    /// A follower of `kind`, locked to nothing.
    #[must_use]
    pub fn new(kind: ClockKind) -> Self {
        Self(match kind {
            ClockKind::BeatClock => Follower::BeatClock(BeatClockFollower::new()),
            ClockKind::Mtc => Follower::Mtc(MtcFollower::new()),
        })
    }

    /// Which clock it reads.
    #[must_use]
    pub fn kind(&self) -> ClockKind {
        match &self.0 {
            Follower::BeatClock(_) => ClockKind::BeatClock,
            Follower::Mtc(_) => ClockKind::Mtc,
        }
    }

    /// Whether the follower is locked to the source now.
    #[must_use]
    pub fn locked(&self) -> bool {
        match &self.0 {
            Follower::BeatClock(f) => f.locked,
            Follower::Mtc(f) => f.locked,
        }
    }

    /// Takes in one UMP of the source's stream, which arrived at `at`, and
    /// passes each event it makes to `out`, in order. A message the kind does
    /// not read makes none. Allocates nothing.
    pub fn step(&mut self, ump: Ump, at: Seconds, out: &mut impl FnMut(ClockEvent)) {
        match &mut self.0 {
            Follower::BeatClock(f) => f.step(ump, at, out),
            Follower::Mtc(f) => f.step(ump, at, out),
        }
    }

    /// Notes that `now` has come, with no message: a source silent for its
    /// dropout is unlocked, and a transport it was rolling is stopped. Call it
    /// once per control tick, with a `now` on the clock of `step`'s `at`.
    pub fn poll(&mut self, now: Seconds, out: &mut impl FnMut(ClockEvent)) {
        match &mut self.0 {
            Follower::BeatClock(f) => f.poll(now, out),
            Follower::Mtc(f) => f.poll(now, out),
        }
    }

    /// Forgets the source: unlocked, at beat 0, no tempo, no partial frame.
    pub fn reset(&mut self) {
        match &mut self.0 {
            Follower::BeatClock(f) => f.reset(),
            Follower::Mtc(f) => f.reset(),
        }
    }
}

impl BeatClockFollower {
    fn new() -> Self {
        Self {
            decoder: MidiClockDecoder::new(),
            announced: None,
            ticks_since_announce: 0,
            locked: false,
        }
    }

    fn poll(&mut self, now: Seconds, out: &mut impl FnMut(ClockEvent)) {
        if self.decoder.drop_out(now) {
            self.announced = None;
            if std::mem::take(&mut self.locked) {
                out(ClockEvent::Unlocked);
            }
            if self.decoder.transport_state() == ClockTransportState::Playing {
                self.decoder.stop_msg();
                out(ClockEvent::Stop);
            }
        }
    }

    fn reset(&mut self) {
        self.decoder.reset();
        self.announced = None;
        self.ticks_since_announce = 0;
        self.locked = false;
    }

    fn step(&mut self, ump: Ump, at: Seconds, out: &mut impl FnMut(ClockEvent)) {
        let Ok(Message::System(_, system)) = ump.decode() else {
            return;
        };
        let here = |beat: Beat| ClockEvent::Locate {
            position: ClockPosition::Beat(beat),
            at,
        };
        match system {
            System::TimingClock => {
                self.decoder.tick(at);
                self.after_tick(out);
            }
            System::Start => {
                self.decoder.start_msg();
                out(here(Beat::new(0.0)));
                out(ClockEvent::Play);
            }
            System::Continue => {
                self.decoder.continue_msg();
                out(here(self.decoder.beat_position()));
                out(ClockEvent::Play);
            }
            System::Stop => {
                self.decoder.stop_msg();
                out(ClockEvent::Stop);
            }
            System::SongPosition(position) => {
                self.decoder.song_position_msg(position.get());
                out(here(self.decoder.beat_position()));
            }
            _ => {}
        }
    }

    fn after_tick(&mut self, out: &mut impl FnMut(ClockEvent)) {
        let locked = self.decoder.locked();
        if locked != self.locked {
            self.locked = locked;
            if locked {
                out(ClockEvent::Locked);
            } else {
                self.announced = None;
                out(ClockEvent::Unlocked);
            }
        }
        if !locked {
            return;
        }
        self.ticks_since_announce += 1;
        let Some(tempo) = self.decoder.tempo_bpm() else {
            return;
        };
        let due = match self.announced {
            None => true,
            Some(last) => {
                self.ticks_since_announce >= MidiClockDecoder::LOCK_TICKS
                    && tempo.differs_from(last, ClockFollower::TEMPO_STEP)
            }
        };
        if due {
            self.announced = Some(tempo);
            self.ticks_since_announce = 0;
            out(ClockEvent::Tempo(tempo));
        }
    }
}

impl MtcFollower {
    fn new() -> Self {
        Self {
            decoder: MtcDecoder::new(),
            sysex: SysexAssembler::new([0; SYSEX_ROOM], SYSEX_MAX, 1),
            last_cycle: None,
            run: 0,
            locked: false,
            running: false,
            last_heard: None,
        }
    }

    fn poll(&mut self, now: Seconds, out: &mut impl FnMut(ClockEvent)) {
        let silent = self
            .last_heard
            .is_some_and(|at| now - at > ClockFollower::MTC_DROPOUT);
        if silent {
            if std::mem::take(&mut self.running) {
                out(ClockEvent::Stop);
            }
            if std::mem::take(&mut self.locked) {
                out(ClockEvent::Unlocked);
            }
            self.forget_position();
        }
    }

    fn reset(&mut self) {
        self.locked = false;
        self.running = false;
        self.forget_position();
    }

    fn forget_position(&mut self) {
        self.decoder.reset();
        self.sysex.reset();
        self.last_cycle = None;
        self.run = 0;
        self.last_heard = None;
    }

    fn step(&mut self, ump: Ump, at: Seconds, out: &mut impl FnMut(ClockEvent)) {
        self.sysex.begin_block();
        if let SysexStep::Complete(done) = self.sysex.push(ump) {
            if let Read::Universal(Addressed {
                message: Universal::MtcFullMessage(full),
                ..
            }) = read(self.sysex.message(done))
            {
                self.last_heard = Some(at);
                // The partial cycle and the last one belong to the old position.
                self.decoder.reset();
                self.last_cycle = None;
                self.run = 0;
                out(ClockEvent::Locate {
                    position: ClockPosition::Time(full.time_code().timecode().to_seconds()),
                    at,
                });
            }
            return;
        }
        let before = self.decoder.timecode();
        if !self.decoder.feed(&ump) {
            return;
        }
        self.last_heard = Some(at);
        let after = self.decoder.timecode();
        if after != before
            && let Some(code) = after
        {
            self.cycle(code, at, out);
        }
    }

    fn cycle(&mut self, code: SmpteTimecode, at: Seconds, out: &mut impl FnMut(ClockEvent)) {
        let consistent = self.last_cycle.is_some_and(|last| {
            last.frame_rate() == code.frame_rate() && code.frame_count() == last.frame_count() + 2
        });
        let first = self.last_cycle.is_none();
        self.last_cycle = Some(code);
        let here = ClockEvent::Locate {
            position: ClockPosition::Time(code.to_seconds()),
            at,
        };
        if consistent {
            self.run += 1;
        } else {
            self.run = 1;
            if self.running && !first {
                out(here);
            }
        }
        if self.run >= ClockFollower::MTC_LOCK_CYCLES {
            if !self.locked {
                self.locked = true;
                out(ClockEvent::Locked);
            }
            if !self.running {
                self.running = true;
                out(here);
                out(ClockEvent::Play);
            }
        }
    }
}
