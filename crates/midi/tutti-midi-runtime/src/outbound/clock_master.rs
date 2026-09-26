//! Transport-driven MIDI **sync master**: generate outbound MIDI Beat Clock
//! (24 PPQN) and MIDI Time Code so external gear follows *this* transport.
//!
//! [`ClockMaster`] is the symmetric inverse of the sync *decoders*
//! ([`MidiClockDecoder`](tutti_midi_types::sync::MidiClockDecoder) /
//! [`MtcDecoder`](tutti_midi_types::sync::MtcDecoder)): where they read an
//! incoming stream to derive transport state, this reads the transport to
//! *produce* the stream. It is ticked once per audio block, on the audio
//! thread, and stamps each emitted event with a sample-accurate `frame_offset`
//! — the same discipline as [`MidiClipSource`](crate::MidiClipSource), so
//! receiving gear locks tightly instead of chasing frame-quantised jitter.
//!
//! It is **not** a [`MidiUnitIn`](tutti_midi_types::MidiUnitIn): the processor
//! input feeds internal synth routing keyed by
//! [`MidiUnitId`](tutti_midi_types::MidiUnitId), and System Real-Time messages
//! are not addressed to a unit — there is no id to route them by, so they would
//! be dropped there. Instead the master pushes into a
//! [`crate::MidiSender`] — the push half of a
//! [`MidiMailbox`](crate::MidiMailbox) mailbox — whose paired
//! [`MidiReceiver`](crate::MidiReceiver) an off-RT pump drains to hardware
//! MIDI-out. The sender's `queue(&self)` is lock-free, so there is no mutex on
//! the audio path.
//!
//! Emitted (all MIDI 2.0 UMP System messages, M2-104 §7.6):
//! - **Timing Clock** (0xF8) at every 1/24-beat while playing.
//! - **Start** (0xFA) / **Continue** (0xFB) / **Stop** (0xFC) on transport
//!   edges — Start when playback begins at beat 0, Continue otherwise.
//! - **Song Position** (0xF2) on Continue and on seek-while-playing, in MIDI
//!   beats (1/16 notes).
//! - **MTC quarter-frames** (0xF1) at frame-rate×4, cycling the 8 SMPTE nibbles
//!   (when `send_mtc` is set).

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use tutti_midi_types::MidiGroup;

use atomic_float::AtomicF64;
use tutti_core::transport::Timeline;
use tutti_core::{first_frame_at_or_after, Beat, BeatDuration, SampleRate};
use tutti_midi_types::sync::SmpteFrameRate;
use tutti_midi_types::ump::MidiEvent;

use crate::block::registry::MidiSender;

/// MIDI clocks per quarter-note (24 PPQN — the MIDI Beat Clock standard).
const PPQN: f64 = 24.0;
/// Below this beat, a play edge is treated as "from the top" → Start (0xFA)
/// rather than Continue (0xFB). Wide enough to absorb float jitter at beat 0.
const START_EPSILON_BEATS: f64 = 1e-6;
/// A same-frame beat move larger than this (while playing) is a seek, not the
/// normal forward creep of one block — triggers a fresh Song Position.
const SEEK_EPSILON_BEATS: f64 = 1e-3;

/// Generates outbound MIDI clock / timecode from a [`Timeline`].
///
/// Ticked once per audio block via [`ClockMaster::tick`]. RT-safe: reads the
/// transport, mutates only atomics, and pushes into a lock-free mailbox — no
/// allocation, no locks on the audio path ([`MidiSender::queue`] takes `&self`
/// and never blocks).
pub struct ClockMaster {
    transport: Arc<dyn Timeline>,
    /// The device rate, in Hz: what turns the transport's beats into frame
    /// offsets. An atomic because a device restart moves it
    /// ([`set_sample_rate`](Self::set_sample_rate)) on a master the audio
    /// thread shares; `tick` reads it once per block.
    sample_rate: AtomicF64,
    /// UMP group stamped on every emitted event.
    group: MidiGroup,
    /// The output mailbox's push half — lock-free `&self` queueing. The paired
    /// [`MidiReceiver`](crate::MidiReceiver) is drained off-RT by the hardware
    /// pump.
    out: MidiSender,

    /// Master enabled. A disabled master is a cheap early-return in `tick`.
    enabled: AtomicBool,
    /// Whether to also emit MTC quarter-frames.
    send_mtc: AtomicBool,
    /// SMPTE frame rate for MTC, as [`SmpteFrameRate`] discriminant.
    mtc_fps: AtomicU8,

    /// `is_playing()` at the previous tick — for stopped↔playing edge detection
    /// (the transport exposes no edges of its own).
    prev_playing: AtomicBool,
    /// `current_beat()` at the previous tick — for seek detection and to carry
    /// the fractional clock-tick phase across blocks.
    prev_beat: AtomicF64,
    /// How far the previous tick's block was due to move the beat, in beats:
    /// what the beat is compared with at the next tick to tell a locate from
    /// ordinary playback. The previous block's figure, not this one's — they
    /// differ when the block size, the tempo or (on a device restart) the
    /// sample rate moved in between, and comparing with this block's called
    /// that a seek.
    prev_advance: AtomicF64,
    /// The MTC quarter-frame grid, in closed form rather than accumulated
    /// (doc 013 §6, "the frame is the source of truth"): quarter-frame `k`
    /// (counted from the last transport edge or locate, so `k & 7` is its
    /// nibble) is due `mtc_lead + (k - mtc_base) × rate / (4 × fps)` frames
    /// into the current grid segment, whose frames `mtc_frames` counts. A
    /// rate or fps change starts a new segment at the next quarter-frame due,
    /// with its lead rescaled to the same wall-clock time; an edge or locate
    /// restarts at quarter-frame 0 on the block's first frame.
    mtc_base: AtomicU64,
    /// The next quarter-frame to emit (see `mtc_base`).
    mtc_next: AtomicU64,
    /// Frames rolled in the current grid segment.
    mtc_frames: AtomicU64,
    /// Where quarter-frame `mtc_base` is due in the segment, in frames.
    mtc_lead: AtomicF64,
}

impl crate::block::pre_block::BlockClock for ClockMaster {
    #[inline]
    fn tick(&self, block_size: usize) {
        ClockMaster::tick(self, block_size)
    }
}

impl ClockMaster {
    /// Build a clock master reading `transport`, emitting into `out`.
    /// Starts **disabled**; call [`set_enabled`](Self::set_enabled) once a
    /// hardware output is connected.
    pub fn new(
        transport: Arc<dyn Timeline>,
        sample_rate: impl Into<SampleRate>,
        out: MidiSender,
    ) -> Self {
        Self {
            transport,
            sample_rate: AtomicF64::new(sample_rate.into().get()),
            group: MidiGroup::FIRST,
            out,
            enabled: AtomicBool::new(false),
            send_mtc: AtomicBool::new(false),
            mtc_fps: AtomicU8::new(SmpteFrameRate::Fps25 as u8),
            prev_playing: AtomicBool::new(false),
            prev_beat: AtomicF64::new(0.0),
            prev_advance: AtomicF64::new(0.0),
            mtc_base: AtomicU64::new(0),
            mtc_next: AtomicU64::new(0),
            mtc_frames: AtomicU64::new(0),
            mtc_lead: AtomicF64::new(0.0),
        }
    }

    /// Turn clock generation on or off. Disabled masters emit nothing at all —
    /// [`tick`](Self::tick) returns immediately, without even reading the
    /// transport. `&self` and lock-free, so a control thread may flip it while
    /// the audio thread ticks.
    ///
    /// Enabling mid-playback does not synthesise a Start: the next tick sees
    /// `prev_playing` false against a playing transport and emits the
    /// Continue + Song Position that resyncs the receiver.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    /// Whether clock generation is on.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
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
        let base = self.mtc_base.load(Ordering::Acquire);
        let lead = self.mtc_lead.load(Ordering::Acquire);
        lead + (k.saturating_sub(base) as f64 * rate) / (fps.fps() * 4.0)
    }

    /// Start a new MTC grid segment at the next quarter-frame due, the
    /// frames left until it rescaled from `old_rate`/`old_fps` to the same
    /// wall-clock time at `new_rate` (a quarter-frame already due stays
    /// due). `old_fps` only places the pending one: a new fps spaces the
    /// ones after it.
    fn rebase_mtc(&self, old_rate: f64, old_fps: SmpteFrameRate, new_rate: f64) {
        let next = self.mtc_next.load(Ordering::Acquire);
        let at = self.mtc_frames.load(Ordering::Acquire) as f64;
        let left = self.mtc_due(next, old_rate, old_fps) - at;
        self.mtc_base.store(next, Ordering::Release);
        self.mtc_lead
            .store(left * new_rate / old_rate, Ordering::Release);
        self.mtc_frames.store(0, Ordering::Release);
    }

    /// Restart the MTC grid at quarter-frame 0 (piece 0, what a decoder
    /// needs to lock) on the next block's first frame: a transport edge or
    /// a locate.
    fn restart_mtc(&self) {
        self.mtc_base.store(0, Ordering::Release);
        self.mtc_next.store(0, Ordering::Release);
        self.mtc_frames.store(0, Ordering::Release);
        self.mtc_lead.store(0.0, Ordering::Release);
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

    /// Set the SMPTE frame rate the MTC quarter-frames are denominated in.
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

    /// Push an event into the output mailbox (drops if full — a bounded, benign
    /// backpressure, like a saturated hardware MIDI wire). Lock-free `&self`.
    #[inline]
    fn emit(&self, event: MidiEvent) {
        let _ = self.out.queue(&[event]);
    }

    /// Generate this block's clock/timecode. Call once per audio block with the
    /// block's frame count. Reads the transport internally.
    pub fn tick(&self, block_size: usize) {
        // Once per block: a rate moved mid-block would split it in two.
        let sample_rate = self.sample_rate();
        if !self.enabled.load(Ordering::Acquire) || block_size == 0 || sample_rate.get() <= 0.0 {
            // Keep prev_playing honest so re-enabling mid-playback emits a fresh
            // Start/Continue rather than silently assuming playback was already
            // under way.
            self.prev_playing
                .store(self.transport.is_rolling(), Ordering::Release);
            return;
        }

        let playing = self.transport.is_rolling();
        let was_playing = self.prev_playing.swap(playing, Ordering::AcqRel);
        let beat = self.transport.beat();
        let prev_beat = Beat(self.prev_beat.swap(beat.get(), Ordering::AcqRel));

        // --- transport edges -------------------------------------------------
        if playing && !was_playing {
            if beat.get().abs() <= START_EPSILON_BEATS {
                self.emit(MidiEvent::start(self.group));
            } else {
                self.emit(MidiEvent::song_position(
                    self.group,
                    beats_to_midi_beats(beat),
                ));
                self.emit(MidiEvent::continue_msg(self.group));
            }
            // Realign the MTC grid to the (possibly non-zero) start beat.
            self.restart_mtc();
        } else if !playing && was_playing {
            self.emit(MidiEvent::stop(self.group));
            return;
        }

        if !playing {
            return;
        }

        // --- seek while playing ---------------------------------------------
        // A block normally advances the beat by what it was due to (the
        // previous tick's `expected_advance`); anything else, either direction,
        // is a locate.
        let tempo = self.transport.tempo();
        if tempo.get() <= 0.0 {
            return;
        }
        // Kept as the `BeatDuration` the shared derivation returns rather than
        // unwrapped to f64: this is a *span* of beats per sample, while `beat`
        // below is a *position*. Both are beats-denominated and neither is an
        // `Hz` — a beat-synced rate is beats-per-cycle, the inverse of a
        // frequency. Unwrapping either lets the two mix silently.
        let beats_per_sample = tutti_core::transport::beats_per_sample(tempo, sample_rate);
        let expected_advance = beats_per_sample * block_size as f64;
        let prev_advance = BeatDuration(
            self.prev_advance
                .swap(expected_advance.get(), Ordering::AcqRel),
        );
        let is_edge = playing && !was_playing;
        let jumped = !is_edge
            && ((beat - prev_beat) - prev_advance).abs() > BeatDuration(SEEK_EPSILON_BEATS);
        if jumped {
            self.emit(MidiEvent::song_position(
                self.group,
                beats_to_midi_beats(beat),
            ));
            self.restart_mtc();
        }

        // --- 24-PPQN clock ticks --------------------------------------------
        // Emit a 0xF8 at every 1/24-beat boundary playback reaches inside this
        // block, on its frame. `tick_beats` is the clock-tick spacing in beats
        // (1/24).
        //
        // Placed by the engine's one beat→frame rule
        // (`first_frame_at_or_after`, doc 013 §6) and compared as integer
        // frames, not as beats against `beat + expected_advance`: a tick on
        // a block boundary is reached on the next block's first frame (offset
        // `block_size` here, 0 there), so it goes out once. Comparing beats,
        // the previous block's end and this block's start round
        // independently, and a boundary tick was sent twice or not at all.
        //
        // Where playback starts, continues or locates on a tick boundary, that
        // tick goes out on the first frame, after the Start / Continue / Song
        // Position sent above (same offset, later in the queue). MIDI 1.0
        // (MMA, "System Real Time Messages", Start and Continue): a receiver
        // begins on the first Timing Clock after Start / Continue, so the
        // clock of the start beat must be sent; skipping it leaves every
        // receiving device one tick behind. Off a boundary, the first tick is
        // the next boundary's.
        let tick_beats = BeatDuration(1.0 / PPQN);
        let max_offset = (block_size - 1) as u32;
        // The tick at or before `beat`: its frame is 0 (on `beat`, within the
        // rule's tolerance) or before this block (already sent, or behind an
        // off-tick start).
        let mut tick_idx = (beat.get() / tick_beats.get()).floor() as i64;
        loop {
            let tick_beat = Beat(tick_idx as f64 * tick_beats.get());
            let k = first_frame_at_or_after((tick_beat - beat) / beats_per_sample);
            tick_idx += 1;
            if k < 0 {
                continue;
            }
            match u32::try_from(k) {
                Ok(k) if k <= max_offset => {
                    self.emit(MidiEvent::timing_clock(self.group).with_frame_offset(k));
                }
                _ => break,
            }
        }

        // --- MTC quarter-frames ---------------------------------------------
        if self.send_mtc.load(Ordering::Acquire) {
            self.tick_mtc(block_size, sample_rate, beats_per_sample, beat, max_offset);
        }
    }

    /// Emit MTC quarter-frames that fall within this block. Quarter-frames go
    /// out at `fps * 4` Hz; each carries one of 8 nibbles of the current SMPTE
    /// time (M2-104 §7.6, MTC quarter-frame 0xF1).
    /// `beats_per_sample` is a rate and `beat` a position. They were adjacent
    /// bare `f64`s, so transposing them compiled and produced garbage timecode;
    /// the two types make that a compile error.
    fn tick_mtc(
        &self,
        block_size: usize,
        sample_rate: SampleRate,
        beats_per_sample: BeatDuration,
        beat: Beat,
        max_offset: u32,
    ) {
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
        let at = self.mtc_frames.load(Ordering::Acquire);
        let mut next = self.mtc_next.load(Ordering::Acquire);

        // Seconds-per-beat for converting beat → wall-clock SMPTE: the
        // reciprocal of `beats_per_sample`, the rate every reader in the
        // engine divides by, rather than `BeatDuration::to_seconds`, which
        // returns f32 `Seconds` (SMPTE is one of the named f64 carve-outs).
        let secs_per_beat = if beats_per_sample > BeatDuration(0.0) {
            1.0 / (beats_per_sample.get() * rate)
        } else {
            0.0
        };

        // Quarter-frames due inside [0, block_size), each on the frame it
        // falls in (the floor of its position, as the MTC grid always was).
        loop {
            let qf_sample = (self.mtc_due(next, rate, fps) - at as f64).max(0.0);
            if qf_sample >= block_size as f64 {
                break;
            }
            let sample_offset = (qf_sample as u32).min(max_offset);
            // Wall-clock time at this quarter-frame → SMPTE, then nibble.
            let block_beat = beat + beats_per_sample * qf_sample;
            let seconds = block_beat.get() * secs_per_beat;
            let tc = seconds_to_smpte(seconds, fps);
            let nibble = mtc_nibble(&tc, (next & 0x07) as u8, fps);
            self.emit(
                MidiEvent::mtc_quarter_frame(self.group, nibble).with_frame_offset(sample_offset),
            );
            next += 1;
        }

        self.mtc_next.store(next, Ordering::Release);
        self.mtc_frames
            .store(at + block_size as u64, Ordering::Release);
    }
}

/// Convert a beat position to MIDI beats (1/16 notes) for Song Position, which
/// counts sixteenth-notes from the start (M2-104 §7.6, 0xF2). Clamped to the
/// 14-bit field; the constructor masks, but clamping keeps semantics sane.
#[inline]
fn beats_to_midi_beats(beat: Beat) -> u16 {
    let sixteenths = (beat.get().max(0.0) * 4.0).round();
    sixteenths.min(0x3FFF as f64) as u16
}

/// Break `seconds` into an [`SmpteTimecode`]-style H:M:S:F tuple at `fps`.
fn seconds_to_smpte(seconds: f64, fps: SmpteFrameRate) -> (u8, u8, u8, u8) {
    let seconds = seconds.max(0.0);
    let total_frames = (seconds * fps.fps()).floor() as u64;
    let fps_int = fps.fps().round() as u64;
    let frames = (total_frames % fps_int) as u8;
    let total_secs = total_frames / fps_int;
    let s = (total_secs % 60) as u8;
    let m = ((total_secs / 60) % 60) as u8;
    let h = ((total_secs / 3600) % 24) as u8;
    (h, m, s, frames)
}

/// Encode one of the 8 MTC quarter-frame nibbles (piece 0-7): the piece index
/// in the upper 3 bits, 4 bits of timecode data in the lower nibble. Mirrors
/// the assembly in [`MtcDecoder`](tutti_midi_types::sync::MtcDecoder) in reverse.
fn mtc_nibble(tc: &(u8, u8, u8, u8), piece: u8, fps: SmpteFrameRate) -> u8 {
    let (h, m, s, f) = *tc;
    // MTC encodes the frame rate in 2 bits of the last nibble (0=24, 1=25,
    // 2=29.97-df, 3=30) — the same mapping `MtcDecoder::assemble` reads back.
    let rate2 = match fps {
        SmpteFrameRate::Fps24 => 0u8,
        SmpteFrameRate::Fps25 => 1,
        SmpteFrameRate::Fps2997Df | SmpteFrameRate::Fps2997Ndf => 2,
        SmpteFrameRate::Fps30 => 3,
    };
    let hours_high_and_rate = ((h >> 4) & 0x01) | (rate2 << 1);
    let data = match piece {
        0 => f & 0x0F,
        1 => (f >> 4) & 0x0F,
        2 => s & 0x0F,
        3 => (s >> 4) & 0x0F,
        4 => m & 0x0F,
        5 => (m >> 4) & 0x0F,
        6 => h & 0x0F,
        _ => hours_high_and_rate & 0x0F,
    };
    (piece << 4) | (data & 0x0F)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool as StdAtomicBool;
    use tutti_core::Bpm;
    use tutti_midi_types::sync::{ClockTransportState, MidiClockDecoder, MtcDecoder};

    /// Minimal `Timeline` for tests: tempo + beat + playing under a
    /// switch (mirrors the one in `clip_player.rs`).
    struct TestTransport {
        beat: AtomicF64,
        tempo: Bpm,
        playing: StdAtomicBool,
    }

    impl TestTransport {
        fn new(tempo: Bpm) -> Self {
            Self {
                beat: AtomicF64::new(0.0),
                tempo,
                playing: StdAtomicBool::new(false),
            }
        }
        fn set_beat(&self, b: f64) {
            self.beat.store(b, Ordering::Release);
        }
        fn set_playing(&self, p: bool) {
            self.playing.store(p, Ordering::Release);
        }
    }

    impl Timeline for TestTransport {
        fn beat(&self) -> tutti_core::Beat {
            tutti_core::Beat::new(self.beat.load(Ordering::Acquire))
        }
        fn is_rolling(&self) -> bool {
            self.playing.load(Ordering::Acquire)
        }
        fn tempo(&self) -> Bpm {
            self.tempo
        }
        fn segment_generation(&self) -> u64 {
            0
        }
    }

    fn master(
        tempo: Bpm,
        sample_rate: impl Into<SampleRate>,
    ) -> (ClockMaster, Arc<TestTransport>, crate::MidiReceiver) {
        use tutti_midi_types::MidiUnitId;
        let transport = Arc::new(TestTransport::new(tempo));
        let (sender, receiver) = crate::MidiMailbox::pair(MidiUnitId::next());
        let cm = ClockMaster::new(
            Arc::clone(&transport) as Arc<dyn Timeline>,
            sample_rate,
            sender,
        );
        cm.set_enabled(true);
        (cm, transport, receiver)
    }

    /// Drain a receiver fully into a `Vec` — the test-side stand-in for the old
    /// consumer's `drain_all`.
    fn drain_all(receiver: &crate::MidiReceiver) -> Vec<MidiEvent> {
        let mut out = Vec::new();
        let mut buf = [MidiEvent::noop(); 256];
        loop {
            let n = receiver.poll_into(&mut buf);
            out.extend_from_slice(&buf[..n]);
            if n < buf.len() {
                break;
            }
        }
        out
    }

    fn is_status(ev: &MidiEvent, status: u8) -> bool {
        let w0 = ev.data[0];
        (w0 >> 28) & 0x0F == 0x1 && ((w0 >> 16) & 0xFF) as u8 == status
    }

    #[test]
    fn emits_24_ticks_per_beat_at_120bpm() {
        let sr = 48_000.0;
        let (cm, transport, cons) = master(Bpm(120.0), sr);
        transport.set_playing(true);
        // 120 BPM → 0.5s/beat → 24000 samples/beat. Process one beat's worth of
        // audio in blocks of 512 and count 0xF8 ticks.
        let samples_per_beat = sr * 0.5;
        let block = 512usize;
        let blocks = (samples_per_beat / block as f64).ceil() as usize;

        let mut clock_ticks = 0;
        let mut beat = 0.0f64;
        let beats_per_sample = 120.0 / 60.0 / sr;
        for _ in 0..blocks {
            transport.set_beat(beat);
            cm.tick(block);
            beat += block as f64 * beats_per_sample;
        }
        let events = drain_all(&cons);
        for ev in &events {
            if is_status(ev, 0xF8) {
                clock_ticks += 1;
            }
        }
        // Over ~1 beat, 24 ticks are expected (allow ±1 for boundary rounding).
        assert!(
            (clock_ticks as i64 - 24).abs() <= 1,
            "expected ~24 clock ticks over one beat, got {clock_ticks}"
        );
    }

    #[test]
    fn play_at_zero_emits_start_not_continue() {
        let (cm, transport, cons) = master(Bpm(120.0), 48_000.0);
        transport.set_beat(0.0);
        transport.set_playing(true);
        cm.tick(512);
        let events = drain_all(&cons);
        assert!(
            events.iter().any(|e| is_status(e, 0xFA)),
            "expected Start (0xFA)"
        );
        assert!(
            !events.iter().any(|e| is_status(e, 0xFB)),
            "no Continue at beat 0"
        );
    }

    #[test]
    fn play_mid_song_emits_continue_and_song_position() {
        let (cm, transport, cons) = master(Bpm(120.0), 48_000.0);
        transport.set_beat(8.0); // 2 bars in
        transport.set_playing(true);
        cm.tick(512);
        let events = drain_all(&cons);
        assert!(
            events.iter().any(|e| is_status(e, 0xFB)),
            "expected Continue (0xFB)"
        );
        assert!(
            events.iter().any(|e| is_status(e, 0xF2)),
            "expected Song Position (0xF2)"
        );
        assert!(
            !events.iter().any(|e| is_status(e, 0xFA)),
            "no Start mid-song"
        );
    }

    #[test]
    fn stop_emits_stop_message() {
        let (cm, transport, cons) = master(Bpm(120.0), 48_000.0);
        transport.set_playing(true);
        cm.tick(512);
        let _ = drain_all(&cons);
        transport.set_playing(false);
        cm.tick(512);
        let events = drain_all(&cons);
        assert!(
            events.iter().any(|e| is_status(e, 0xFC)),
            "expected Stop (0xFC)"
        );
    }

    #[test]
    fn disabled_master_emits_nothing() {
        let (cm, transport, cons) = master(Bpm(120.0), 48_000.0);
        cm.set_enabled(false);
        transport.set_playing(true);
        cm.tick(512);
        assert!(
            drain_all(&cons).is_empty(),
            "disabled master must be silent"
        );
    }

    /// Ticks land on their frames, in order, and a start on a tick boundary
    /// sends that boundary's tick on frame 0.
    ///
    /// This pinned two ticks (1000, 2000) until the start tick was fixed:
    /// MIDI 1.0 (MMA, "System Real Time Messages", Start) has a receiver
    /// begin on the first Timing Clock after Start, so the clock of beat 0
    /// must be sent, or every receiver runs one tick behind.
    #[test]
    fn ticks_are_sample_accurate_within_block() {
        // A block that spans three clock ticks, the first on its first
        // frame, places them at distinct, ordered frame offsets.
        let sr = 48_000.0;
        let (cm, transport, cons) = master(Bpm(120.0), sr);
        transport.set_playing(true);
        transport.set_beat(0.0);
        // One beat = 24000 samples; 1/24 beat = 1000 samples. A 2500-sample
        // block starting at beat 0 covers tick boundaries at 0, 1000 and 2000.
        cm.tick(2500);
        let events: Vec<_> = drain_all(&cons)
            .into_iter()
            .filter(|e| is_status(e, 0xF8))
            .collect();
        assert_eq!(events.len(), 3, "expected 3 ticks in the block");
        assert!(
            events[0].frame_offset < events[1].frame_offset
                && events[1].frame_offset < events[2].frame_offset,
            "ticks ordered"
        );
        assert_eq!(events[0].frame_offset, 0, "the start beat's tick");
        assert!((events[1].frame_offset as i64 - 1000).abs() < 4);
        assert!((events[2].frame_offset as i64 - 2000).abs() < 4);
    }

    /// Where the first Timing Clock goes, and in what order, for a block that
    /// does not continue the previous one: the transport message(s), then an
    /// F8 on frame 0 when the beat sits on a tick boundary, else the first F8
    /// on the next boundary.
    fn first_clock(events: &[MidiEvent]) -> (usize, u32) {
        let i = events
            .iter()
            .position(|e| is_status(e, 0xF8))
            .expect("a timing clock");
        (i, events[i].frame_offset)
    }

    /// Start at beat 0, Continue at an on-tick beat, and a locate while
    /// playing to an on-tick beat: each sends an F8 on frame 0, queued after
    /// its Start / Continue / Song Position. MIDI 1.0 (MMA, "System Real
    /// Time Messages", Start, Continue): a receiver begins on the first
    /// Timing Clock after Start / Continue.
    ///
    /// Mutation (run): skip a tick on frame 0 of a block that does not
    /// continue the last (`k == 0 && (is_edge || jumped)` → `continue`, the
    /// old rule) → no F8 at offset 0 → fails.
    #[test]
    fn an_on_tick_start_continue_or_locate_clocks_its_first_frame() {
        let sr = 48_000.0;
        // Start at beat 0.
        let (cm, transport, cons) = master(Bpm(120.0), sr);
        transport.set_beat(0.0);
        transport.set_playing(true);
        cm.tick(512);
        let events = drain_all(&cons);
        let start = events
            .iter()
            .position(|e| is_status(e, 0xFA))
            .expect("Start");
        let (clock, offset) = first_clock(&events);
        assert_eq!(offset, 0, "Start at beat 0: F8 on frame 0");
        assert!(start < clock, "the F8 follows Start in the queue");

        // Continue at beat 8 (tick 192).
        let (cm, transport, cons) = master(Bpm(120.0), sr);
        transport.set_beat(8.0);
        transport.set_playing(true);
        cm.tick(512);
        let events = drain_all(&cons);
        let spp = events.iter().position(|e| is_status(e, 0xF2)).expect("SPP");
        let cont = events
            .iter()
            .position(|e| is_status(e, 0xFB))
            .expect("Continue");
        let (clock, offset) = first_clock(&events);
        assert_eq!(offset, 0, "Continue on a tick: F8 on frame 0");
        assert!(
            spp < clock && cont < clock,
            "the F8 follows SPP and Continue"
        );

        // A locate while playing to beat 4 (tick 96).
        let (cm, transport, cons) = master(Bpm(120.0), sr);
        transport.set_beat(0.0);
        transport.set_playing(true);
        cm.tick(512);
        let _ = drain_all(&cons);
        transport.set_beat(4.0);
        cm.tick(512);
        let events = drain_all(&cons);
        let spp = events.iter().position(|e| is_status(e, 0xF2)).expect("SPP");
        let (clock, offset) = first_clock(&events);
        assert_eq!(offset, 0, "a locate on a tick: F8 on frame 0");
        assert!(spp < clock, "the F8 follows Song Position");
    }

    /// Off a tick boundary, the first F8 after a locate or a Continue is the
    /// next boundary's, on its frame; nothing goes out on frame 0.
    ///
    /// Mutation (run): send a tick behind the block's start on frame 0
    /// (`k < 0` → `k = 0` instead of skipping it) → an F8 at offset 0 →
    /// fails.
    #[test]
    fn an_off_tick_locate_clocks_the_next_boundary() {
        let sr = 48_000.0;
        // Half a tick past beat 4: 500 frames to the next boundary at 120
        // BPM / 48 kHz (a tick is 1000 frames).
        let off = 4.0 + 0.5 / PPQN;
        let (cm, transport, cons) = master(Bpm(120.0), sr);
        transport.set_beat(0.0);
        transport.set_playing(true);
        cm.tick(512);
        let _ = drain_all(&cons);
        transport.set_beat(off);
        cm.tick(2048);
        let events = drain_all(&cons);
        assert!(events.iter().any(|e| is_status(e, 0xF2)), "a locate");
        let (_, offset) = first_clock(&events);
        assert_eq!(offset, 500, "the next boundary's frame");

        // The same off-tick beat as a Continue.
        let (cm, transport, cons) = master(Bpm(120.0), sr);
        transport.set_beat(off);
        transport.set_playing(true);
        cm.tick(2048);
        let (_, offset) = first_clock(&drain_all(&cons));
        assert_eq!(offset, 500, "Continue off a tick: the next boundary");
    }

    /// Run `blocks` blocks of `block` frames at `rate`, the transport rolling
    /// at 120 BPM from `*beat`, and return every event with its absolute
    /// frame (counted from `*frame`). Advances both.
    fn run_at(
        cm: &ClockMaster,
        transport: &TestTransport,
        cons: &crate::MidiReceiver,
        rate: f64,
        (block, blocks): (usize, usize),
        (beat, frame): (&mut f64, &mut u64),
    ) -> Vec<(MidiEvent, u64)> {
        let mut at = Vec::new();
        for _ in 0..blocks {
            transport.set_beat(*beat);
            cm.tick(block);
            at.extend(
                drain_all(cons)
                    .into_iter()
                    .map(|e| (e, *frame + u64::from(e.frame_offset))),
            );
            *beat += block as f64 * 2.0 / rate;
            *frame += block as u64;
        }
        at
    }

    /// The frames of the events in `events` with `status`.
    fn of(events: &[(MidiEvent, u64)], status: u8) -> Vec<u64> {
        events
            .iter()
            .filter(|(e, _)| is_status(e, status))
            .map(|&(_, at)| at)
            .collect()
    }

    /// Mean spacing, in frames, of the events at `at`.
    fn spacing(at: &[u64]) -> f64 {
        assert!(at.len() > 10, "events to measure, got {}", at.len());
        (at[at.len() - 1] - at[0]) as f64 / (at.len() - 1) as f64
    }

    /// **A rate change moves the tick spacing to the new rate's frames.** At
    /// 120 BPM a 24-PPQN tick is 1/48 s: 918.75 frames at 44.1 kHz, 1 000 at
    /// 48 kHz. A master built at 44.1 kHz and re-rated to 48 kHz (a device
    /// restart) must space its ticks 1 000 frames apart from the next block,
    /// or the receiving gear runs ~8.8% fast. (Offsets are whole frames, so
    /// each tick sits up to one frame early; the mean over ~48 ticks is within
    /// 0.05 of the exact spacing.)
    ///
    /// Mutation (run): `set_sample_rate` not storing the rate → the ticks
    /// after it stay 918.75 frames apart → fails.
    #[test]
    fn a_rate_change_moves_the_tick_spacing() {
        let (cm, transport, cons) = master(Bpm(120.0), 44_100.0);
        transport.set_playing(true);
        let (mut beat, mut frame) = (0.0, 0);
        let run = (&mut beat, &mut frame);
        let before = of(
            &run_at(&cm, &transport, &cons, 44_100.0, (441, 100), run),
            0xF8,
        );
        assert!(
            (spacing(&before) - 918.75).abs() < 0.05,
            "{}",
            spacing(&before)
        );

        cm.set_sample_rate(48_000.0);
        assert_eq!(cm.sample_rate(), SampleRate(48_000.0));
        let run = (&mut beat, &mut frame);
        let after = of(
            &run_at(&cm, &transport, &cons, 48_000.0, (480, 100), run),
            0xF8,
        );
        assert!(
            (spacing(&after) - 1_000.0).abs() < 0.05,
            "{}",
            spacing(&after)
        );
    }

    /// **The MTC quarter-frame due across a rate change lands on its
    /// wall-clock time.** At 25 fps a quarter-frame is 1/100 s: 441 frames at
    /// 44.1 kHz, 480 at 48 kHz. After 20 blocks of 512 at 44.1 kHz (10 240
    /// frames) the last quarter-frame was at 10 143 and the next is due 441
    /// frames later, 344 old frames after the restart: 374.4 new frames, and
    /// 480 apart from there. And the restart is not a locate: no Song
    /// Position goes out, and the phase is not reset.
    ///
    /// Mutations (run):
    /// - `set_sample_rate` not rescaling the carried phase → the first one
    ///   lands 344 frames after the restart, ~30 early → fails;
    /// - the seek check comparing the beat's move with *this* block's
    ///   advance (as it did) → the old-rate block's 0.0232 beats against the
    ///   new rate's 0.0213 is past the 0.001 epsilon → a Song Position, and
    ///   the phase reset to the restart's frame → fails.
    #[test]
    fn a_rate_change_keeps_the_mtc_quarter_frame_on_time() {
        let (cm, transport, cons) = master(Bpm(120.0), 44_100.0);
        cm.set_send_mtc(true);
        cm.set_mtc_fps(SmpteFrameRate::Fps25);
        transport.set_playing(true);
        let (mut beat, mut frame) = (0.0, 0);
        let run = (&mut beat, &mut frame);
        let before = of(
            &run_at(&cm, &transport, &cons, 44_100.0, (512, 20), run),
            0xF1,
        );
        assert_eq!(
            before.last(),
            Some(&10_143),
            "setup: every 441 frames from 0"
        );

        cm.set_sample_rate(48_000.0);
        let run = (&mut beat, &mut frame);
        let events = run_at(&cm, &transport, &cons, 48_000.0, (512, 20), run);
        assert_eq!(
            of(&events, 0xF2),
            Vec::<u64>::new(),
            "the restart is not a seek"
        );
        let after = of(&events, 0xF1);
        assert_eq!(after[0], 10_240 + 374, "344 old frames are 374.4 new ones");
        assert!(
            (spacing(&after) - 480.0).abs() < 0.05,
            "{}",
            spacing(&after)
        );
    }

    /// **The MTC grid is closed form.** Quarter-frame `k` goes out on frame
    /// `floor(k × rate / (4 × fps))`, however many blocks it took to get
    /// there: at 29.97 fps and 44.1 kHz a quarter-frame is 367.867… frames,
    /// which binary cannot represent, and a carried phase that adds it
    /// block after block drifts off the grid.
    ///
    /// Mutation (run): carry the phase (quarter-frame `k + 1` due at `k`'s
    /// position plus one quarter-frame's length, accumulated) → some
    /// quarter-frame whose exact position is a hair past a whole frame lands
    /// a frame early → fails.
    #[test]
    fn the_mtc_grid_is_closed_form() {
        let rate = 44_100.0;
        let fps = SmpteFrameRate::Fps2997Ndf;
        let (cm, transport, cons) = master(Bpm(120.0), rate);
        cm.set_send_mtc(true);
        cm.set_mtc_fps(fps);
        transport.set_playing(true);
        let (mut beat, mut frame) = (0.0, 0);
        let events = run_at(
            &cm,
            &transport,
            &cons,
            rate,
            (512, 6_000),
            (&mut beat, &mut frame),
        );
        let quarters = of(&events, 0xF1);
        assert!(quarters.len() > 8_000, "{} quarter-frames", quarters.len());
        for (k, &at) in quarters.iter().enumerate() {
            let due = ((k as f64 * rate) / (fps.fps() * 4.0)).floor() as u64;
            assert_eq!(at, due, "quarter-frame {k}");
        }
    }

    #[test]
    fn generated_clock_round_trips_through_decoder() {
        // The strongest check: feed this master's own output into the decoder and confirm
        // it recovers the tempo and beat — the in-repo loopback from the plan.
        let sr = 48_000.0;
        let (cm, transport, cons) = master(Bpm(120.0), sr);
        transport.set_playing(true);

        let block = 256usize;
        let beats_per_sample = 120.0 / 60.0 / sr;
        let us_per_sample = 1_000_000.0 / sr;
        let mut beat = 0.0f64;
        let mut sample_clock = 0u64;

        let mut decoder = MidiClockDecoder::new();
        // Run ~4 beats so the decoder's tempo window fills.
        for _ in 0..((sr * 2.0 / block as f64) as usize) {
            transport.set_beat(beat);
            cm.tick(block);
            for ev in drain_all(&cons) {
                let ts_us = ((sample_clock + ev.frame_offset as u64) as f64 * us_per_sample) as u64;
                decoder.feed(&ev, ts_us);
            }
            beat += block as f64 * beats_per_sample;
            sample_clock += block as u64;
        }

        assert_eq!(decoder.transport_state(), ClockTransportState::Playing);
        let tempo = decoder.tempo_bpm().expect("decoder derived a tempo");
        assert!(
            !tempo.differs_from(Bpm(120.0), 2.0),
            "round-trip tempo ~120, got {tempo:?}"
        );
    }

    #[test]
    fn mtc_quarter_frames_round_trip_to_a_timecode() {
        let sr = 48_000.0;
        let (cm, transport, cons) = master(Bpm(120.0), sr);
        cm.set_send_mtc(true);
        cm.set_mtc_fps(SmpteFrameRate::Fps25);
        transport.set_playing(true);

        // 25 fps → 100 quarter-frames/sec → one every 480 samples. Run enough
        // blocks to emit at least 8 consecutive quarter-frames (2 full frames).
        let block = 480usize;
        let beats_per_sample = 120.0 / 60.0 / sr;
        let mut beat = 0.0f64;
        let mut decoder = MtcDecoder::new();
        let mut got_tc = false;
        for _ in 0..40 {
            transport.set_beat(beat);
            cm.tick(block);
            for ev in drain_all(&cons) {
                if is_status(&ev, 0xF1) {
                    // The quarter-frame data byte is the 2nd wire byte; decode
                    // via the message view's TimeCode.
                    if let tutti_midi_types::MidiMessage::TimeCode { code, .. } = ev.message() {
                        decoder.feed(code);
                        if decoder.timecode().is_some() {
                            got_tc = true;
                        }
                    }
                }
            }
            beat += block as f64 * beats_per_sample;
        }
        assert!(got_tc, "MTC quarter-frames should assemble into a timecode");
    }
}
