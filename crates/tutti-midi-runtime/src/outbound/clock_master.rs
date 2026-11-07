//! Transport-driven MIDI **sync master**: generate outbound MIDI Beat Clock
//! (24 PPQN) and MIDI Time Code so external gear follows *this* transport.
//!
//! [`ClockMaster`] is the symmetric inverse of the sync *decoders*
//! ([`MidiClockDecoder`](tutti_midi_types::sync::MidiClockDecoder) /
//! [`MtcDecoder`](tutti_midi_types::sync::MtcDecoder)): where they read an
//! incoming stream to derive transport state, this reads the transport to
//! *produce* the stream. It is ticked once per transport segment of a block,
//! on the audio thread, by a [`ClockNode`](crate::ClockNode), and stamps each
//! event it emits with a sample-accurate `frame_offset`, so receiving gear
//! locks tightly instead of chasing frame-quantised jitter. The node sends
//! them out of an event port; wire it to a [`MidiOutNode`](crate::MidiOutNode)
//! for a hardware pump to drain.
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

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use tutti_ump::MidiGroup;

use atomic_float::AtomicF64;
use tutti_graph::Transport;
use tutti_midi_types::sync::{SmpteFrameRate, SmpteTimecode};
use tutti_midi_types::ump::MidiEvent;
use tutti_types::{
    Beat, BeatDuration, LoopRange, Pass, PassEnd, SampleRate, Samples, first_frame_at_or_after,
};

/// MIDI clocks per quarter-note (24 PPQN — the MIDI Beat Clock standard).
const PPQN: f64 = 24.0;
/// Below this beat, a play edge is treated as "from the top" → Start (0xFA)
/// rather than Continue (0xFB). Wide enough to absorb float jitter at beat 0.
const START_EPSILON_BEATS: f64 = 1e-6;
/// A start beat further than this from where the previous block ended (while
/// playing) is a seek, not the normal run-on of the next block: it triggers a
/// fresh Song Position.
const SEEK_EPSILON_BEATS: f64 = 1e-3;

/// Generates outbound MIDI clock / timecode from the transport.
///
/// Ticked via [`ClockMaster::tick`]. RT-safe: mutates only atomics and hands
/// each event to its caller — no allocation, no locks. Shared (`Arc`) between
/// the [`ClockNode`](crate::ClockNode) that ticks it and the control thread
/// that configures it.
#[derive(Debug)]
pub struct ClockMaster {
    /// The device rate, in Hz: what turns the transport's beats into frame
    /// offsets. An atomic because a device restart moves it
    /// ([`set_sample_rate`](Self::set_sample_rate)) on a master the audio
    /// thread shares; `tick` reads it once per block.
    sample_rate: AtomicF64,
    /// UMP group stamped on every emitted event.
    group: MidiGroup,

    /// Master enabled. A disabled master is a cheap early-return in `tick`.
    enabled: AtomicBool,
    /// Whether to also emit MTC quarter-frames.
    send_mtc: AtomicBool,
    /// Events a [`ClockNode`](crate::ClockNode) could not send because one
    /// block produced more than its
    /// [`CLOCK_EVENT_CAPACITY`](crate::CLOCK_EVENT_CAPACITY). Counted, never
    /// logged: the count is bumped on the audio thread.
    dropped: AtomicU64,
    /// SMPTE frame rate for MTC, as [`SmpteFrameRate`] discriminant.
    mtc_fps: AtomicU8,

    /// What the previous tick saw of the transport.
    prev: PrevTick,
    /// The MTC quarter-frame grid.
    mtc: MtcGrid,
}

/// The transport as the previous tick saw it, for edge and locate detection.
#[derive(Debug)]
struct PrevTick {
    /// `is_playing()` at the previous tick — for stopped↔playing edge detection
    /// (the transport exposes no edges of its own).
    playing: AtomicBool,
    /// The beat the previous tick's block ended on, a loop wrap in it
    /// included: what the next block's start is compared with to tell a
    /// locate from ordinary playback. The previous block's end, not this
    /// block's figure: the block size, the tempo or (on a device restart) the
    /// sample rate may have moved in between, and a wrap moves the beat
    /// backwards.
    end: AtomicF64,
    /// The previous tick's block ended on a loop wrap, on its last frame's
    /// successor: that frame is the next block's first, whose clock already
    /// stands on the wrapped beat, so the wrap shows in no pass of its own
    /// and `end` equals the start. The next tick sends its Song Position.
    wrapped: AtomicBool,
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
    base: AtomicU64,
    /// The next quarter-frame to emit (see `base`).
    next: AtomicU64,
    /// Frames rolled in the current grid segment.
    frames: AtomicU64,
    /// Where quarter-frame `base` is due in the segment, in frames.
    lead: AtomicF64,
    /// The eight nibbles of the timecode being sent, piece `p` in bits
    /// `4p..4p+4`. Set once at piece 0, so one cycle carries one position.
    cycle: AtomicU32,
    /// The grid was just restarted: its first piece 0 is still to be placed
    /// on an even frame ([`ClockMaster::tick_mtc`]).
    align: AtomicBool,
}

impl ClockMaster {
    /// A clock master placing events at `sample_rate`. Starts **disabled**;
    /// call [`set_enabled`](Self::set_enabled) once a hardware output is
    /// connected.
    pub fn new(sample_rate: impl Into<SampleRate>) -> Self {
        Self {
            sample_rate: AtomicF64::new(sample_rate.into().get()),
            group: MidiGroup::FIRST,
            enabled: AtomicBool::new(false),
            send_mtc: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            mtc_fps: AtomicU8::new(SmpteFrameRate::Fps25 as u8),
            prev: PrevTick {
                playing: AtomicBool::new(false),
                end: AtomicF64::new(0.0),
                wrapped: AtomicBool::new(false),
            },
            mtc: MtcGrid {
                base: AtomicU64::new(0),
                next: AtomicU64::new(0),
                frames: AtomicU64::new(0),
                lead: AtomicF64::new(0.0),
                cycle: AtomicU32::new(0),
                align: AtomicBool::new(true),
            },
        }
    }

    /// Turn clock generation on or off. Disabled masters emit nothing at all —
    /// [`tick`](Self::tick) returns immediately, without even reading the
    /// transport. `&self` and lock-free, so a control thread may flip it while
    /// the audio thread ticks.
    ///
    /// Enabling mid-playback does not synthesise a Start: the next tick sees
    /// `prev.playing` false against a playing transport and emits the
    /// Continue + Song Position that resyncs the receiver.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    /// Whether clock generation is on.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// How many events the [`ClockNode`](crate::ClockNode) ticking this master
    /// has dropped, since it was created, because a single block produced
    /// more than [`CLOCK_EVENT_CAPACITY`](crate::CLOCK_EVENT_CAPACITY) of
    /// them (a very long block at a very high tempo with MTC on). A host polls
    /// it; nonzero means the receiver missed ticks and should be resynced.
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Adds `n` to [`dropped_events`](Self::dropped_events). Audio thread.
    pub(crate) fn count_dropped(&self, n: u64) {
        if n > 0 {
            self.dropped.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// Move the master to a device running at `sample_rate`: what a device
    /// restart at a new rate does, with no callback running.
    ///
    /// The 24-PPQN ticks are placed from the transport's beat, so from the
    /// next block they land on the new rate's frames — at 120 BPM a tick
    /// every 1 000 frames at 48 kHz, not the 918.75 of 44.1 kHz, which would
    /// run the receiving gear ~8.8% fast. The MTC quarter-frame grid is
    /// counted in frames, so it starts a new segment at the next
    /// quarter-frame due, rescaled to the same wall-clock time: that
    /// quarter-frame stays where it was due.
    ///
    /// `&self`, lock-free: the master is shared with the audio thread. A
    /// non-positive rate is stored as given, and `tick` then emits nothing.
    pub fn set_sample_rate(&self, sample_rate: impl Into<SampleRate>) {
        let new = sample_rate.into().get();
        let old = self.sample_rate.swap(new, Ordering::AcqRel);
        if old > 0.0 && new > 0.0 {
            self.rebase_mtc(old, self.fps(), new);
        }
    }

    /// The MTC frame rate in force.
    fn fps(&self) -> SmpteFrameRate {
        SmpteFrameRate::from_u8(self.mtc_fps.load(Ordering::Acquire))
    }

    /// Frames from the MTC grid segment's first frame to quarter-frame `k`,
    /// at `rate` and `fps`: closed form, never accumulated.
    fn mtc_due(&self, k: u64, rate: f64, fps: SmpteFrameRate) -> f64 {
        let base = self.mtc.base.load(Ordering::Acquire);
        let lead = self.mtc.lead.load(Ordering::Acquire);
        lead + (k.saturating_sub(base) as f64 * rate) / (fps.fps() * 4.0)
    }

    /// Start a new MTC grid segment at the next quarter-frame due, the
    /// frames left until it rescaled from `old_rate`/`old_fps` to the same
    /// wall-clock time at `new_rate` (a quarter-frame already due stays
    /// due). `old_fps` only places the pending one: a new fps spaces the
    /// ones after it.
    fn rebase_mtc(&self, old_rate: f64, old_fps: SmpteFrameRate, new_rate: f64) {
        let next = self.mtc.next.load(Ordering::Acquire);
        let at = self.mtc.frames.load(Ordering::Acquire) as f64;
        let left = self.mtc_due(next, old_rate, old_fps) - at;
        self.mtc.base.store(next, Ordering::Release);
        self.mtc
            .lead
            .store(left * new_rate / old_rate, Ordering::Release);
        self.mtc.frames.store(0, Ordering::Release);
    }

    /// Restart the MTC grid at quarter-frame 0 (piece 0, what a decoder
    /// needs to lock) on the next block's first frame: a transport edge or
    /// a locate.
    fn restart_mtc(&self) {
        self.mtc.base.store(0, Ordering::Release);
        self.mtc.next.store(0, Ordering::Release);
        self.mtc.frames.store(0, Ordering::Release);
        self.mtc.lead.store(0.0, Ordering::Release);
        self.mtc.align.store(true, Ordering::Release);
    }

    /// The rate [`tick`](Self::tick) places events at.
    pub fn sample_rate(&self) -> SampleRate {
        SampleRate(self.sample_rate.load(Ordering::Acquire))
    }

    /// Turn MTC quarter-frame emission on or off, independently of the 24-PPQN
    /// Beat Clock. Both ride the same mailbox; a receiver that wants only one
    /// is served by disabling the other here rather than filtering downstream.
    pub fn set_send_mtc(&self, send: bool) {
        self.send_mtc.store(send, Ordering::Release);
    }

    /// Sets the SMPTE frame rate the MTC quarter-frames are denominated in.
    ///
    /// It appears twice in the stream: as the quarter-frame cadence
    /// (frame-rate × 4) and encoded into the hours nibble. Changing it does not
    /// reset the quarter-frame phase, so a receiver sees the new rate from the
    /// next complete 8-piece cycle.
    pub fn set_mtc_fps(&self, fps: SmpteFrameRate) {
        let old = self.fps();
        let rate = self.sample_rate.load(Ordering::Acquire);
        if rate > 0.0 && old != fps {
            // The pending quarter-frame keeps its time; the new fps spaces
            // the ones after it.
            self.rebase_mtc(rate, old, rate);
        }
        self.mtc_fps.store(fps as u8, Ordering::Release);
    }

    /// Generate the clock/timecode of `block_size` frames starting with the
    /// transport at `transport`, handing each event (its `frame_offset` from
    /// the first of those frames) to `emit`. Call once per stretch of
    /// unchanging transport: a block, or each of its segments.
    ///
    /// Events come out in the order generated, not by offset: in each pass
    /// (the block cut at a loop wrap) the MTC quarter-frames follow its ticks.
    pub fn tick(&self, transport: &Transport, block_size: usize, emit: &mut impl FnMut(MidiEvent)) {
        // Once per block: a rate moved mid-block would split it in two. A
        // counted transport carries the rate its block runs at, and that is
        // the one its clock is rebuilt at (a master re-rated between the
        // host's block and this tick would otherwise mix the two).
        let sample_rate = transport
            .origin()
            .map_or_else(|| self.sample_rate(), |o| o.sample_rate);
        if !self.enabled.load(Ordering::Acquire) || block_size == 0 || sample_rate.get() <= 0.0 {
            // Keep prev.playing honest so re-enabling mid-playback emits a fresh
            // Start/Continue rather than silently assuming playback was already
            // under way.
            self.prev
                .playing
                .store(transport.playing, Ordering::Release);
            self.prev.wrapped.store(false, Ordering::Release);
            return;
        }

        let playing = transport.playing;
        let was_playing = self.prev.playing.swap(playing, Ordering::AcqRel);
        let beat = transport.beat();

        // --- transport edges -------------------------------------------------
        if playing && !was_playing {
            if beat.get().abs() <= START_EPSILON_BEATS {
                emit(MidiEvent::start(self.group));
            } else {
                emit(MidiEvent::song_position(
                    self.group,
                    beats_to_midi_beats(beat),
                ));
                emit(MidiEvent::continue_msg(self.group));
            }
            // Realign the MTC grid to the (possibly non-zero) start beat.
            self.restart_mtc();
        } else if !playing && was_playing {
            emit(MidiEvent::stop(self.group));
            self.prev.wrapped.store(false, Ordering::Release);
            return;
        }

        if !playing {
            self.prev.wrapped.store(false, Ordering::Release);
            return;
        }

        let tempo = transport.tempo;
        if tempo.get() <= 0.0 {
            // Not moving: the next block starts where this one did.
            self.prev.end.store(beat.get(), Ordering::Release);
            self.prev.wrapped.store(false, Ordering::Release);
            return;
        }
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
        let prev_end = Beat(self.prev.end.load(Ordering::Acquire));
        let is_edge = playing && !was_playing;
        let located = (beat - prev_end).abs() > BeatDuration(SEEK_EPSILON_BEATS);
        let wrapped = self.prev.wrapped.load(Ordering::Acquire);
        if !is_edge && (located || wrapped) {
            emit(MidiEvent::song_position(
                self.group,
                beats_to_midi_beats(beat),
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
                emit(
                    MidiEvent::song_position(self.group, beats_to_midi_beats(pass.beat))
                        .with_frame_offset(frame_offset(pass.start.get())),
                );
                self.restart_mtc();
            }
            self.tick_clock(&pass, beats_per_sample, emit);
            if self.send_mtc.load(Ordering::Acquire) {
                self.tick_mtc(&pass, sample_rate, beats_per_sample, emit);
            }
        }
        self.prev.end.store(clock.beat().get(), Ordering::Release);
        self.prev.wrapped.store(ends_on_wrap, Ordering::Release);
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
    /// (MMA, "System Real Time Messages", Start and Continue): a receiver
    /// begins on the first Timing Clock after Start / Continue, so the
    /// clock of the start beat must be sent; skipping it leaves every
    /// receiving device one tick behind. Off a boundary, the first tick is
    /// the next boundary's.
    fn tick_clock(
        &self,
        pass: &Pass,
        beats_per_sample: BeatDuration,
        emit: &mut impl FnMut(MidiEvent),
    ) {
        let tick_beats = BeatDuration(1.0 / PPQN);
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
                Ok(k) if k < pass.len.get() => emit(
                    MidiEvent::timing_clock(self.group)
                        .with_frame_offset(frame_offset(pass.start.get() + k)),
                ),
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
        &self,
        pass: &Pass,
        sample_rate: SampleRate,
        beats_per_sample: BeatDuration,
        emit: &mut impl FnMut(MidiEvent),
    ) {
        let (beat, len) = (pass.beat, pass.len.get());
        let fps = self.fps();
        let rate = sample_rate.get();
        if !(fps.fps() > 0.0 && rate > 0.0) {
            return;
        }

        // Where the block starts on the grid segment, and the next
        // quarter-frame to send. Each one's frame is derived from its index
        // in closed form (`mtc_due`), never by adding a quarter-frame's
        // length to a carried phase, so the grid does not drift however
        // long it runs. An edge or locate restarts it at quarter-frame 0 on
        // the block's first frame: piece 0 first, which is what a decoder
        // needs to lock.
        let at = self.mtc.frames.load(Ordering::Acquire);
        let mut next = self.mtc.next.load(Ordering::Acquire);

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
        if self.mtc.align.swap(false, Ordering::AcqRel) {
            let (num, den) = fps.ratio();
            let period = 2.0 * rate * den as f64 / num as f64;
            let cycles = block_pos / period;
            let k = if (cycles - cycles.round()).abs() < 1e-6 {
                cycles.round()
            } else {
                cycles.ceil()
            };
            self.mtc
                .lead
                .store((k * period - block_pos).max(0.0), Ordering::Release);
        }
        let mut cycle = self.mtc.cycle.load(Ordering::Acquire);

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
                let tc = timecode_at(block_pos + qf_sample, rate, fps);
                cycle = mtc_cycle_nibbles(&tc, fps);
            }
            let nibble = (piece << 4) | ((cycle >> (4 * u32::from(piece))) & 0x0F) as u8;
            emit(
                MidiEvent::mtc_quarter_frame(self.group, nibble)
                    .with_frame_offset(frame_offset(sample_offset)),
            );
            next += 1;
        }

        self.mtc.cycle.store(cycle, Ordering::Release);
        self.mtc.next.store(next, Ordering::Release);
        self.mtc.frames.store(at + len as u64, Ordering::Release);
    }
}

/// A frame index as an event's offset; a block is never near `u32::MAX`
/// frames, so the saturation is unreachable.
fn frame_offset(frames: usize) -> u32 {
    u32::try_from(frames).unwrap_or(u32::MAX)
}

/// Convert a beat position to MIDI beats (1/16 notes) for Song Position, which
/// counts sixteenth-notes from the start (M2-104 §7.6, 0xF2). Clamped to the
/// 14-bit field; the constructor masks, but clamping keeps semantics sane.
#[inline]
fn beats_to_midi_beats(beat: Beat) -> u16 {
    let sixteenths = (beat.get().max(0.0) * 4.0).round();
    sixteenths.min(0x3FFF as f64) as u16
}

/// The timecode at `position` samples from transport beat 0 at `rate` Hz: the
/// nearest frame, on an even one (a cycle's eight pieces span two frames).
/// Rounding, not flooring, so a position a float's width short of a frame
/// boundary still reads as that frame.
fn timecode_at(position: f64, rate: f64, fps: SmpteFrameRate) -> SmpteTimecode {
    let (num, den) = fps.ratio();
    let frames = (position * num as f64 / (den as f64 * rate))
        .round()
        .max(0.0) as u64;
    SmpteTimecode::from_frame_count(frames & !1, fps)
}

/// The eight MTC nibbles of `tc`, piece `p` in bits `4p..4p+4`: the inverse of
/// the assembly in [`MtcDecoder`](tutti_midi_types::sync::MtcDecoder).
///
/// MTC has four rate codes (24, 25, 29.97 drop-frame, 30) and no 29.97
/// non-drop one. [`SmpteFrameRate::Fps2997Ndf`] goes out with the 29.97
/// code, so a receiver reads its non-drop labels as drop-frame.
fn mtc_cycle_nibbles(tc: &SmpteTimecode, fps: SmpteFrameRate) -> u32 {
    let rate2 = match fps {
        SmpteFrameRate::Fps24 => 0u8,
        SmpteFrameRate::Fps25 => 1,
        SmpteFrameRate::Fps2997Df | SmpteFrameRate::Fps2997Ndf => 2,
        SmpteFrameRate::Fps30 => 3,
    };
    let (h, m, s, f) = (tc.hours, tc.minutes, tc.seconds, tc.frames);
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
