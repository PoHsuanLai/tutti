//! Transport-driven MIDI **sync master**: generate outbound MIDI Beat Clock
//! (24 PPQN) and MIDI Time Code so external gear follows *this* transport.
//!
//! [`ClockMaster`] (with its [`ClockControls`]) is the symmetric inverse of the sync *decoders*
//! ([`MidiClockDecoder`](crate::MidiClockDecoder) /
//! [`MtcDecoder`](crate::MtcDecoder)): where they read an
//! incoming stream to derive transport state, this reads the transport to
//! *produce* the stream. It is ticked once per transport segment of a block,
//! on the audio thread, by a [`ClockNode`](crate::ClockNode), and stamps each
//! event it emits with a sample-accurate [`Offset`], so receiving gear
//! locks tightly instead of chasing frame-quantised jitter. The node sends
//! them out of an event port; tap it (`Graph::taps().open_event`)
//! for a hardware pump to drain.
//!
//! **The tempo is the published map's**: the engine hands
//! the node a segment for every cell where the chord tempo changes, seated on
//! the map's exact beat, and `tick` reads `transport.tempo` of each. A
//! segment that starts where the previous ended (within `SEEK_EPSILON_BEATS`)
//! is a continuation, so a ramp's cell edge or a step is no locate; the
//! ticks are the first frames at or after beat `k / 24` on the playhead, which
//! is on the map's frame at every cell start and within a frame of it inside
//! one (`tests/clock_tempo_map.rs`).
//!
//! Emitted (all MIDI 2.0 UMP System messages, M2-104 §7.6):
//! - **Timing Clock** (0xF8) at every 1/24-beat while playing.
//! - **Start** (0xFA) / **Continue** (0xFB) / **Stop** (0xFC) on transport
//!   edges — Start when playback begins at beat 0, Continue otherwise.
//! - **Song Position** (0xF2) on Continue, on seek-while-playing and on the
//!   frame a loop wraps, in MIDI beats (1/16 notes).
//!
//! A loop wrap is a locate on its own frame: a Song Position there (also on a
//! block's first frame, when the previous block ended on the wrap), the next
//! ticks from the wrapped beat, and the MTC grid restarts. A Song Position
//! counts sixteenths and rounds to the nearest, so a loop start off a
//! sixteenth leaves a receiver up to 1/8 beat behind the sender's tick grid,
//! as after any locate. The restarted grid puts its first piece 0 on the next
//! even frame of the timeline from the wrapped beat, so a loop start that is
//! not on an even frame leaves up to two frames with no quarter-frames.
//! - **MTC quarter-frames** (0xF1) at frame-rate×4, cycling the 8 SMPTE nibbles
//!   (when `send_mtc` is set).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use tutti_graph::{Losses, Offset, TransportPosition, UmpAt};
use tutti_types::{
    AtomicSampleRate, Beat, BeatDuration, LoopRange, Pass, PassEnd, SampleRate, Samples, Seconds,
    first_frame_at_or_after,
};
use tutti_ump::{Message, MidiGroup, System, U7, U14, Ump};

use tutti_midi_protocol::{MtcRate, SmpteFrameRate, SmpteTimecode};

/// Below this beat, a play edge is treated as "from the top" → Start (0xFA)
/// rather than Continue (0xFB). Wide enough to absorb float jitter at beat 0.
const START_EPSILON_BEATS: f64 = 1e-6;
/// A start beat further than this from where the previous block ended (while
/// playing) is a seek, not the normal run-on of the next block: it triggers a
/// fresh Song Position.
const SEEK_EPSILON_BEATS: f64 = 1e-3;

/// What a control thread sets on a clock: enabled, MTC on or off, and the
/// SMPTE frame rate. One value in one word (an `AtomicU32`), so the audio
/// thread reads the three of one store, never a mix of two.
///
/// Layout of the word: bit 0 is enabled, bit 1 is MTC, bits 8 to 15 are the
/// MTC rate's two-bit code ([`MtcRate::code`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Settings {
    enabled: bool,
    send_mtc: bool,
    fps: MtcRate,
}

impl Settings {
    const ENABLED: u32 = 1;
    const SEND_MTC: u32 = 1 << 1;
    const FPS_SHIFT: u32 = 8;

    const OFF: Self = Self {
        enabled: false,
        send_mtc: false,
        fps: MtcRate::Fps25,
    };

    fn bits(self) -> u32 {
        let mut word = u32::from(self.fps.code()) << Self::FPS_SHIFT;
        if self.enabled {
            word |= Self::ENABLED;
        }
        if self.send_mtc {
            word |= Self::SEND_MTC;
        }
        word
    }

    /// The settings `bits` is the word of. Total: the rate is the low two bits
    /// of its byte (`MtcRate::masked`), so a word that no `bits` wrote reads as
    /// some rate instead of needing an arm that cannot run for the words that do.
    fn from_bits(word: u32) -> Self {
        Self {
            enabled: word & Self::ENABLED != 0,
            send_mtc: word & Self::SEND_MTC != 0,
            fps: MtcRate::masked((word >> Self::FPS_SHIFT) as u8),
        }
    }
}

/// What the control side and the audio side of a clock share: the settings
/// word, the rate the master last prepared at, and the count of what it lost.
#[derive(Debug)]
struct Shared {
    settings: AtomicU32,
    /// The rate the master was last given. Written by the audio half
    /// ([`ClockMaster::set_sample_rate`]), read by [`ClockControls`].
    sample_rate: AtomicSampleRate,
    /// Events a [`ClockNode`](crate::ClockNode) could not send because one
    /// block produced more than its
    /// [`CLOCK_EVENT_CAPACITY`](crate::CLOCK_EVENT_CAPACITY). Counted, never
    /// logged: the count is bumped on the audio thread.
    dropped: AtomicU64,
}

/// The control side of a clock: turn it on, pick MTC and its frame rate, read
/// what it lost. Cheap to clone; every clone is the same clock.
///
/// Every call is wait-free and takes effect at the audio half's next
/// [`tick`](ClockMaster::tick); none of them touches the audio half's state.
#[derive(Debug, Clone)]
pub struct ClockControls {
    shared: Arc<Shared>,
    group: MidiGroup,
}

impl ClockControls {
    /// Turn clock generation on or off. A disabled clock emits nothing at all:
    /// [`tick`](ClockMaster::tick) returns without reading the transport.
    ///
    /// Enabling mid-playback does not synthesise a Start: the next tick sees
    /// a stopped clock against a playing transport and emits the Continue and
    /// Song Position that resync the receiver.
    pub fn set_enabled(&self, enabled: bool) {
        self.update(|s| s.enabled = enabled);
    }

    /// Whether clock generation is on.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.settings().enabled
    }

    /// Turn MTC quarter-frame emission on or off, independently of the 24-PPQN
    /// Beat Clock. A receiver that wants only one is served by disabling the
    /// other here rather than filtering downstream.
    pub fn set_send_mtc(&self, send: bool) {
        self.update(|s| s.send_mtc = send);
    }

    /// Sets the SMPTE frame rate the MTC quarter-frames are denominated in.
    ///
    /// It appears twice in the stream: as the quarter-frame cadence
    /// (frame-rate × 4) and encoded into the hours nibble. Changing it does not
    /// reset the quarter-frame phase, so a receiver sees the new rate from the
    /// next complete 8-piece cycle.
    pub fn set_mtc_fps(&self, fps: MtcRate) {
        self.update(|s| s.fps = fps);
    }

    /// The MTC frame rate last set.
    #[must_use]
    pub fn mtc_fps(&self) -> MtcRate {
        self.settings().fps
    }

    /// The rate the clock places events at: what the master was last prepared
    /// at.
    #[must_use]
    pub fn sample_rate(&self) -> SampleRate {
        self.shared.sample_rate.load(Ordering::Acquire)
    }

    /// The Group every event is sent on.
    #[must_use]
    pub fn group(&self) -> MidiGroup {
        self.group
    }

    /// What the [`ClockNode`](crate::ClockNode) ticking this clock has lost,
    /// all of it `refused` (no room in its block): events dropped, since it was
    /// created, because a single block produced more than
    /// [`CLOCK_EVENT_CAPACITY`](crate::CLOCK_EVENT_CAPACITY) of them (a very
    /// long block at a very high tempo with MTC on). A host polls it; nonzero
    /// means the receiver missed ticks and should be resynced.
    #[must_use]
    pub fn losses(&self) -> Losses {
        Losses::refused(self.shared.dropped.load(Ordering::Relaxed))
    }

    fn settings(&self) -> Settings {
        Settings::from_bits(self.shared.settings.load(Ordering::Acquire))
    }

    /// Replaces one field of the settings: one atomic read-modify-write of
    /// the whole word, so two control threads setting two fields both land.
    fn update(&self, change: impl Fn(&mut Settings)) {
        self.shared
            .settings
            .update(Ordering::Release, Ordering::Acquire, |word| {
                let mut settings = Settings::from_bits(word);
                change(&mut settings);
                settings.bits()
            });
    }
}

/// Generates outbound MIDI clock / timecode from the transport: the audio
/// half of a clock, ticked by the [`ClockNode`](crate::ClockNode) that owns it.
///
/// Everything here is plain state only [`tick`](Self::tick) uses, so it takes
/// `&mut self`: no atomics, no locks, no allocation. What a control thread
/// sets is read from the shared settings word at the start of a tick, and a
/// frame-rate change it finds there rebases the MTC grid on the audio thread
/// (a request in effect, never a store from the control side).
#[derive(Debug)]
pub struct ClockMaster {
    shared: Arc<Shared>,
    /// UMP group stamped on every emitted event.
    group: MidiGroup,
    /// The device rate: what turns the transport's beats into frame offsets.
    sample_rate: SampleRate,
    /// The MTC frame rate in force, as of the last tick.
    fps: MtcRate,
    /// What the previous tick saw of the transport.
    prev: PrevTick,
    /// The MTC quarter-frame grid.
    mtc: MtcGrid,
}

/// The transport as the previous tick saw it, for edge and locate detection.
#[derive(Debug)]
struct PrevTick {
    /// `playing` at the previous tick, for stopped to playing edge detection
    /// (the transport exposes no edges of its own).
    playing: bool,
    /// The beat the previous tick's block ended on, a loop wrap in it
    /// included: what the next block's start is compared with to tell a
    /// locate from ordinary playback. The previous block's end, not this
    /// block's figure: the block size, the tempo or (on a device restart) the
    /// sample rate may have moved in between, and a wrap moves the beat
    /// backwards.
    end: Beat,
    /// The previous tick's block ended on a loop wrap, on its last frame's
    /// successor: that frame is the next block's first, whose clock already
    /// stands on the wrapped beat, so the wrap shows in no pass of its own
    /// and `end` equals the start. The next tick sends its Song Position.
    wrapped: bool,
}

/// The MTC quarter-frame grid.
#[derive(Debug)]
struct MtcGrid {
    /// The MTC quarter-frame grid, in closed form rather than accumulated
    /// (the frame, not the beat, is the source of truth): quarter-frame `k`
    /// (counted from the last transport edge or locate, so `k & 7` is its
    /// nibble) is due `lead + (k - base) × rate / (4 × fps)` frames
    /// into the current grid segment, whose frames `frames` counts. A
    /// rate or fps change starts a new segment at the next quarter-frame due,
    /// with its lead rescaled to the same wall-clock time; an edge or locate
    /// restarts at quarter-frame 0 on the block's first frame.
    base: u64,
    /// The next quarter-frame to emit (see `base`).
    next: u64,
    /// Frames rolled in the current grid segment.
    frames: u64,
    /// Where quarter-frame `base` is due in the segment, in frames.
    lead: f64,
    /// The eight nibbles of the timecode being sent, piece `p` in bits
    /// `4p..4p+4`. Set once at piece 0, so one cycle carries one position.
    cycle: u32,
    /// The grid was just restarted: its first piece 0 is still to be placed
    /// on an even frame ([`ClockMaster::tick_mtc`]).
    align: bool,
}

impl MtcGrid {
    const RESTARTED: Self = Self {
        base: 0,
        next: 0,
        frames: 0,
        lead: 0.0,
        cycle: 0,
        align: true,
    };
}

impl ClockMaster {
    /// A System message on this master's group.
    fn system(&self, message: System) -> Ump {
        Message::System(self.group, message).into()
    }

    /// A clock sending on `group` (every event it emits is addressed to it: a
    /// clock is the Group's, M2-104 section 7.6) and placing events at
    /// `sample_rate`: the audio half, and the controls that configure it.
    /// Starts **disabled**; call [`ClockControls::set_enabled`] once a hardware
    /// output is connected.
    #[must_use]
    pub fn new(group: MidiGroup, sample_rate: SampleRate) -> (Self, ClockControls) {
        let shared = Arc::new(Shared {
            settings: AtomicU32::new(Settings::OFF.bits()),
            sample_rate: AtomicSampleRate::new(sample_rate),
            dropped: AtomicU64::new(0),
        });
        let controls = ClockControls {
            shared: Arc::clone(&shared),
            group,
        };
        let master = Self {
            shared,
            group,
            sample_rate,
            fps: Settings::OFF.fps,
            prev: PrevTick {
                playing: false,
                end: Beat(0.0),
                wrapped: false,
            },
            mtc: MtcGrid::RESTARTED,
        };
        (master, controls)
    }

    /// Adds `n` to [`ClockControls::losses`].
    pub(crate) fn count_dropped(&self, n: u64) {
        if n > 0 {
            self.shared.dropped.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Move the master to a device running at `sample_rate`: what a device
    /// restart at a new rate does, with no callback running.
    ///
    /// The 24-PPQN ticks are placed from the transport's beat, so from the
    /// next block they land on the new rate's frames: at 120 BPM a tick
    /// every 1 000 frames at 48 kHz, not the 918.75 of 44.1 kHz, which would
    /// run the receiving gear about 8.8% fast. The MTC quarter-frame grid is
    /// counted in frames, so it starts a new segment at the next
    /// quarter-frame due, rescaled to the same wall-clock time: that
    /// quarter-frame stays where it was due.
    pub fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        let old = std::mem::replace(&mut self.sample_rate, sample_rate);
        self.rebase_mtc(old, self.fps, sample_rate);
        self.shared
            .sample_rate
            .store(sample_rate, Ordering::Release);
    }

    /// The rate [`tick`](Self::tick) places events at.
    #[must_use]
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// The Group every event is sent on.
    #[must_use]
    pub fn group(&self) -> MidiGroup {
        self.group
    }

    /// Frames from the MTC grid segment's first frame to quarter-frame `k`,
    /// at `rate` and `fps`: closed form, never accumulated.
    fn mtc_due(&self, k: u64, rate: SampleRate, fps: MtcRate) -> f64 {
        self.mtc.lead
            + (k.saturating_sub(self.mtc.base) as f64 * rate.get())
                / (SmpteFrameRate::from(fps).fps() * 4.0)
    }

    /// Start a new MTC grid segment at the next quarter-frame due, the
    /// frames left until it rescaled from `old_rate`/`old_fps` to the same
    /// wall-clock time at `new_rate` (a quarter-frame already due stays
    /// due). `old_fps` only places the pending one: a new fps spaces the
    /// ones after it.
    fn rebase_mtc(&mut self, old_rate: SampleRate, old_fps: MtcRate, new_rate: SampleRate) {
        let next = self.mtc.next;
        let left = self.mtc_due(next, old_rate, old_fps) - self.mtc.frames as f64;
        self.mtc.base = next;
        self.mtc.lead = left * new_rate.get() / old_rate.get();
        self.mtc.frames = 0;
    }

    /// Restart the MTC grid at quarter-frame 0 (piece 0, what a decoder
    /// needs to lock) on the next block's first frame: a transport edge or
    /// a locate.
    fn restart_mtc(&mut self) {
        let cycle = self.mtc.cycle;
        self.mtc = MtcGrid {
            cycle,
            ..MtcGrid::RESTARTED
        };
    }

    /// Generate the clock/timecode of `block` frames starting with the
    /// transport at `transport`, handing each event (its [`Offset`] from
    /// the first of those frames) to `emit`. Call once per stretch of
    /// unchanging transport: a block, or each of its segments.
    ///
    /// Events come out in the order generated, not by offset: in each pass
    /// (the block cut at a loop wrap) the MTC quarter-frames follow its ticks.
    pub fn tick(
        &mut self,
        transport: &TransportPosition,
        block: Samples,
        emit: &mut impl FnMut(UmpAt),
    ) {
        // The passes below index and split by frame count.
        let block_size = block.get();
        // Once per block: a rate moved mid-block would split it in two. A
        // counted transport carries the rate its block runs at, and that is
        // the one its clock is rebuilt at (a master re-rated between the
        // host's block and this tick would otherwise mix the two).
        let sample_rate = transport
            .origin()
            .map_or(self.sample_rate, |o| o.sample_rate);
        // What the control side set, as of one store. A frame rate it moved
        // is a new MTC grid segment, started here on the audio thread.
        let settings = Settings::from_bits(self.shared.settings.load(Ordering::Acquire));
        if settings.fps != self.fps {
            self.rebase_mtc(self.sample_rate, self.fps, self.sample_rate);
            self.fps = settings.fps;
        }
        if !settings.enabled || block_size == 0 {
            // Keep prev.playing honest so re-enabling mid-playback emits a fresh
            // Start/Continue rather than silently assuming playback was already
            // under way.
            self.prev.playing = transport.playing;
            self.prev.wrapped = false;
            return;
        }

        let playing = transport.playing;
        let was_playing = std::mem::replace(&mut self.prev.playing, playing);
        let beat = transport.beat();

        // --- transport edges -------------------------------------------------
        if playing && !was_playing {
            if beat.get().abs() <= START_EPSILON_BEATS {
                emit((Offset::ZERO, self.system(System::Start)));
            } else {
                emit((
                    Offset::ZERO,
                    self.system(System::SongPosition(beats_to_midi_beats(beat))),
                ));
                emit((Offset::ZERO, self.system(System::Continue)));
            }
            // Realign the MTC grid to the (possibly non-zero) start beat.
            self.restart_mtc();
        } else if !playing && was_playing {
            emit((Offset::ZERO, self.system(System::Stop)));
            self.prev.wrapped = false;
            return;
        }

        if !playing {
            self.prev.wrapped = false;
            return;
        }

        let tempo = transport.tempo;
        // Kept as the `BeatDuration` the shared derivation returns rather than
        // unwrapped to f64: this is a *span* of beats per sample, while `beat`
        // is a *position*. Both are beats-denominated and neither is an
        // `Hz` — a beat-synced rate is beats-per-cycle, the inverse of a
        // frequency. Unwrapping either lets the two mix silently.
        let beats_per_sample = tutti_types::beats_per_sample(tempo, sample_rate);

        // --- seek while playing ---------------------------------------------
        // A block starts where the previous one ended; any other start, either
        // direction, is a locate. So is a block that starts on a wrap: the
        // previous block ended on it, and one Song Position serves it and a
        // locate on the same frame.
        let prev_end = self.prev.end;
        let is_edge = playing && !was_playing;
        let located = (beat - prev_end).abs() > BeatDuration(SEEK_EPSILON_BEATS);
        let wrapped = self.prev.wrapped;
        if !is_edge && (located || wrapped) {
            emit((
                Offset::ZERO,
                self.system(System::SongPosition(beats_to_midi_beats(beat))),
            ));
            self.restart_mtc();
        }

        // --- per pass: 24-PPQN clock ticks, MTC ------------------------------
        // The block is cut at each loop wrap (`FrameClock::passes`) and each
        // pass is placed from its own first beat, so the ticks after a wrap
        // follow the wrapped playhead instead of running on past the loop's
        // end. A wrap is a locate on its own frame: a Song Position there, and
        // the MTC grid restarts on it.
        let region = transport
            .looping
            .and_then(|l| LoopRange::new(l.start, l.end));
        let mut clock = transport.clock(sample_rate);
        let mut ends_on_wrap = false;
        for pass in clock.passes(Samples(block_size), region) {
            ends_on_wrap = pass.end == PassEnd::Wrap;
            if !pass.start.is_zero() {
                emit((
                    frame_offset(pass.start.get(), block_size),
                    self.system(System::SongPosition(beats_to_midi_beats(pass.beat))),
                ));
                self.restart_mtc();
            }
            self.tick_clock(&pass, block_size, beats_per_sample, emit);
            if settings.send_mtc {
                self.tick_mtc(&pass, block_size, sample_rate, beats_per_sample, emit);
            }
        }
        self.prev.end = clock.beat();
        self.prev.wrapped = ends_on_wrap;
    }

    /// Emit a 0xF8 at every 1/24-beat boundary playback reaches inside `pass`,
    /// on its frame. `tick_beats` is the clock-tick spacing in beats (1/24).
    ///
    /// Placed by the engine's one beat→frame rule
    /// (`first_frame_at_or_after`) and compared as integer
    /// frames, not as beats against the pass's end: a tick on a block
    /// boundary is reached on the next block's first frame (offset `len`
    /// here, 0 there), so it goes out once. Comparing beats, the previous
    /// block's end and this block's start round independently, and a
    /// boundary tick was sent twice or not at all.
    ///
    /// Where playback starts, continues, locates or wraps on a tick boundary,
    /// that tick goes out on the first frame, after the Start / Continue /
    /// Song Position sent for it (same offset, later in the queue). MIDI 1.0
    /// (MMA, System Real Time Messages, Start and Continue): a receiver
    /// begins on the first Timing Clock after Start / Continue, so the
    /// clock of the start beat must be sent; skipping it leaves every
    /// receiving device one tick behind. Off a boundary, the first tick is
    /// the next boundary's.
    fn tick_clock(
        &self,
        pass: &Pass,
        block_size: usize,
        beats_per_sample: BeatDuration,
        emit: &mut impl FnMut(UmpAt),
    ) {
        let tick_beats = BeatDuration(1.0 / f64::from(System::TIMING_CLOCKS_PER_QUARTER));
        // The tick at or before the pass's first beat: its frame is 0 (on the
        // beat, within the rule's tolerance) or before the pass (already
        // sent, or behind an off-tick start).
        let mut tick_idx = (pass.beat.get() / tick_beats.get()).floor() as i64;
        loop {
            let tick_beat = Beat(tick_idx as f64 * tick_beats.get());
            let k = first_frame_at_or_after((tick_beat - pass.beat) / beats_per_sample);
            tick_idx += 1;
            if k < 0 {
                continue;
            }
            match usize::try_from(k) {
                Ok(k) if k < pass.len.get() => emit((
                    frame_offset(pass.start.get() + k, block_size),
                    self.system(System::TimingClock),
                )),
                _ => break,
            }
        }
    }

    /// Emit MTC quarter-frames that fall within `pass`. Quarter-frames go
    /// out at `fps * 4` Hz; each carries one of 8 nibbles of the current SMPTE
    /// time (M2-104 §7.6, MTC quarter-frame 0xF1).
    /// `beats_per_sample` is a rate and `beat` a position. They were adjacent
    /// bare `f64`s, so transposing them compiled and produced garbage timecode;
    /// the two types make that a compile error.
    fn tick_mtc(
        &mut self,
        pass: &Pass,
        block_size: usize,
        sample_rate: SampleRate,
        beats_per_sample: BeatDuration,
        emit: &mut impl FnMut(UmpAt),
    ) {
        let (beat, len) = (pass.beat, pass.len.get());
        let fps = self.fps;
        let rate = sample_rate;

        // Where the block starts on the grid segment, and the next
        // quarter-frame to send. Each one's frame is derived from its index
        // in closed form (`mtc_due`), never by adding a quarter-frame's
        // length to a carried phase, so the grid does not drift however
        // long it runs. An edge or locate restarts it at quarter-frame 0 on
        // the block's first frame: piece 0 first, which is what a decoder
        // needs to lock.
        let at = self.mtc.frames;
        let mut next = self.mtc.next;

        // The sample position of the block's first frame, from beat 0.
        let block_pos = if beats_per_sample > BeatDuration(0.0) {
            (beat.get() / beats_per_sample.get()).max(0.0)
        } else {
            0.0
        };
        // A restarted grid puts its first piece 0 on the next even-frame
        // boundary of the timeline (a multiple of two frames' samples from
        // beat 0), so the label is the frame it goes out on. The grid then
        // runs in frames from there: exact at a steady tempo, while a tempo
        // change moves the timeline under a grid fixed in samples, and the
        // label (read from the beat at each piece 0) follows within a frame.
        if std::mem::take(&mut self.mtc.align) {
            let (num, den) = SmpteFrameRate::from(fps).ratio();
            let period = 2.0 * rate.get() * den as f64 / num as f64;
            let cycles = block_pos / period;
            let k = if (cycles - cycles.round()).abs() < 1e-6 {
                cycles.round()
            } else {
                cycles.ceil()
            };
            self.mtc.lead = (k * period - block_pos).max(0.0);
        }
        let mut cycle = self.mtc.cycle;

        // Quarter-frames due inside [0, len), each on the frame it falls in
        // (the floor of its position, as the MTC grid always was).
        loop {
            let qf_sample = (self.mtc_due(next, rate, fps) - at as f64).max(0.0);
            if qf_sample >= len as f64 {
                break;
            }
            let sample_offset = pass.start.get() + (qf_sample as usize).min(len - 1);
            let piece = (next & 0x07) as u8;
            // A cycle's timecode is that of the sample piece 0 goes out on;
            // the receiver adds the two frames the cycle takes.
            if piece == 0 {
                let tc = timecode_at(Seconds::new((block_pos + qf_sample) / rate.get()), fps);
                cycle = mtc_cycle_nibbles(&tc, fps);
            }
            let nibble = (piece << 4) | ((cycle >> (4 * u32::from(piece))) & 0x0F) as u8;
            emit((
                frame_offset(sample_offset, block_size),
                // Framed wire bits: the piece is below 8 and the nibble 4 bits.
                self.system(System::TimeCode(U7::masked(nibble))),
            ));
            next += 1;
        }

        self.mtc.cycle = cycle;
        self.mtc.next = next;
        self.mtc.frames = at + len as u64;
    }
}

/// A frame index as an event's offset in a block of `block_size` frames. Every
/// frame placed is inside the block, so the clamp onto the last frame (what the
/// node did to a stray offset) is unreachable.
fn frame_offset(frames: usize, block_size: usize) -> Offset {
    Offset::new(
        Samples(frames.min(block_size.saturating_sub(1))),
        Samples(block_size),
    )
    .unwrap_or(Offset::ZERO)
}

/// Convert a beat position to MIDI beats (1/16 notes) for Song Position, which
/// counts sixteenth-notes from the start (M2-104 §7.6, 0xF2). Clamped to the
/// 14-bit field; the constructor masks, but clamping keeps semantics sane.
#[inline]
fn beats_to_midi_beats(beat: Beat) -> U14 {
    let sixteenths = (beat.get().max(0.0) * 4.0).round();
    // `as u16` saturates, and `max(0.0)` above turned NaN and negatives into 0;
    // a song position past the 14-bit range sticks at the largest.
    U14::new(sixteenths as u16).unwrap_or(U14::MAX)
}

/// The timecode at `position` from transport beat 0: the
/// nearest frame, on an even one (a cycle's eight pieces span two frames).
/// Rounding, not flooring, so a position a float's width short of a frame
/// boundary still reads as that frame.
fn timecode_at(position: Seconds, fps: MtcRate) -> SmpteTimecode {
    let fps = SmpteFrameRate::from(fps);
    let (num, den) = fps.ratio();
    let frames = (position.get() * num as f64 / den as f64).round().max(0.0) as u64;
    SmpteTimecode::from_frame_count(frames & !1, fps)
}

/// The eight MTC nibbles of `tc`, piece `p` in bits `4p..4p+4`: the inverse of
/// the assembly in [`MtcDecoder`](crate::MtcDecoder).
///
fn mtc_cycle_nibbles(tc: &SmpteTimecode, fps: MtcRate) -> u32 {
    let rate2 = fps.code();
    let (h, m, s, f) = (tc.hours(), tc.minutes(), tc.seconds(), tc.frames());
    let nibbles = [
        f & 0x0F,
        f >> 4,
        s & 0x0F,
        s >> 4,
        m & 0x0F,
        m >> 4,
        h & 0x0F,
        ((h >> 4) & 0x01) | (rate2 << 1),
    ];
    nibbles
        .iter()
        .enumerate()
        .fold(0, |acc, (p, &n)| acc | (u32::from(n & 0x0F) << (4 * p)))
}

#[cfg(test)]
mod tests;
