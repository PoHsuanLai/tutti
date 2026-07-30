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
//! It is **not** a [`MidiIn`](tutti_midi_types::MidiIn): the processor input
//! feeds internal synth routing (keyed by [`MidiUnitId`]), and System
//! Real-Time messages aren't addressed to a unit, so they'd be dropped there.
//! Instead the master pushes into a [`MidiSender`](crate::MidiSender) — the
//! push half of a [`MidiMailbox`](crate::MidiMailbox) mailbox — whose paired
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

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use atomic_float::AtomicF64;
use tutti_core::transport::Timeline;
use tutti_core::SampleRate;
use tutti_midi_types::sync::SmpteFrameRate;
use tutti_midi_types::ump::MidiEvent;

use crate::registry::MidiSender;

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
    sample_rate: SampleRate,
    /// UMP group nibble stamped on every emitted event (0-15).
    group: u8,
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
    /// Fractional SMPTE-frame phase carried across blocks (in quarter-frames),
    /// so MTC emission stays on the wall-clock grid regardless of block size.
    mtc_qf_phase: AtomicF64,
    /// Which of the 8 MTC quarter-frame nibbles is emitted next (0-7).
    mtc_piece: AtomicU8,
}

impl crate::pre_block::BlockClock for ClockMaster {
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
            sample_rate: sample_rate.into(),
            group: 0,
            out,
            enabled: AtomicBool::new(false),
            send_mtc: AtomicBool::new(false),
            mtc_fps: AtomicU8::new(SmpteFrameRate::Fps25 as u8),
            prev_playing: AtomicBool::new(false),
            prev_beat: AtomicF64::new(0.0),
            mtc_qf_phase: AtomicF64::new(0.0),
            mtc_piece: AtomicU8::new(0),
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    pub fn set_send_mtc(&self, send: bool) {
        self.send_mtc.store(send, Ordering::Release);
    }

    pub fn set_mtc_fps(&self, fps: SmpteFrameRate) {
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
        if !self.enabled.load(Ordering::Acquire) || block_size == 0 || self.sample_rate.get() <= 0.0
        {
            // Keep prev_playing honest so re-enabling mid-playback emits a fresh
            // Start/Continue rather than silently assuming we were already going.
            self.prev_playing
                .store(self.transport.is_rolling(), Ordering::Release);
            return;
        }

        let playing = self.transport.is_rolling();
        let was_playing = self.prev_playing.swap(playing, Ordering::AcqRel);
        let beat = self.transport.beat().get();
        let prev_beat = self.prev_beat.swap(beat, Ordering::AcqRel);

        // --- transport edges -------------------------------------------------
        if playing && !was_playing {
            if beat.abs() <= START_EPSILON_BEATS {
                self.emit(MidiEvent::start(self.group));
            } else {
                self.emit(MidiEvent::song_position(
                    self.group,
                    beats_to_midi_beats(beat),
                ));
                self.emit(MidiEvent::continue_msg(self.group));
            }
            // Realign the tick + MTC phase to the (possibly non-zero) start beat.
            self.mtc_qf_phase.store(0.0, Ordering::Release);
            self.mtc_piece.store(0, Ordering::Release);
        } else if !playing && was_playing {
            self.emit(MidiEvent::stop(self.group));
            return;
        }

        if !playing {
            return;
        }

        // --- seek while playing ---------------------------------------------
        // A block normally advances the beat by ~block_size * beats_per_sample;
        // anything beyond that (either direction) is a locate.
        let tempo_bpm = self.transport.tempo().get();
        if tempo_bpm <= 0.0 {
            return;
        }
        // The shared derivation, rather than a third hand-rolled copy.
        let beats_per_sample =
            tutti_core::transport::beats_per_sample(tempo_bpm, self.sample_rate).get();
        let expected_advance = block_size as f64 * beats_per_sample;
        let is_edge = playing && !was_playing;
        if !is_edge && (beat - prev_beat).abs() > expected_advance + SEEK_EPSILON_BEATS {
            self.emit(MidiEvent::song_position(
                self.group,
                beats_to_midi_beats(beat),
            ));
            self.mtc_qf_phase.store(0.0, Ordering::Release);
            self.mtc_piece.store(0, Ordering::Release);
        }

        // --- 24-PPQN clock ticks --------------------------------------------
        // Emit a 0xF8 at every 1/24-beat boundary inside the block window
        // [beat, beat + expected_advance), sample-accurate. `tick_beats` is the
        // clock-tick spacing in beats (1/24). We find the first tick boundary at
        // or after `beat` and walk forward.
        let tick_beats = 1.0 / PPQN;
        let end_beat = beat + expected_advance;
        let max_offset = (block_size - 1) as u32;
        // First tick index strictly *after* `beat`. `floor + 1` guarantees we
        // skip a boundary sitting exactly on `beat` (already emitted at the tail
        // of the previous block) — using `ceil` would re-send that boundary.
        let mut tick_idx = (beat / tick_beats).floor() as i64 + 1;
        loop {
            let tick_beat = tick_idx as f64 * tick_beats;
            if tick_beat >= end_beat {
                break;
            }
            let sample_offset = ((tick_beat - beat) / beats_per_sample) as u32;
            self.emit(
                MidiEvent::timing_clock(self.group)
                    .with_frame_offset(sample_offset.min(max_offset)),
            );
            tick_idx += 1;
        }

        // --- MTC quarter-frames ---------------------------------------------
        if self.send_mtc.load(Ordering::Acquire) {
            self.tick_mtc(block_size, beats_per_sample, beat, max_offset);
        }
    }

    /// Emit MTC quarter-frames that fall within this block. Quarter-frames go
    /// out at `fps * 4` Hz; each carries one of 8 nibbles of the current SMPTE
    /// time (M2-104 §7.6, MTC quarter-frame 0xF1).
    fn tick_mtc(&self, block_size: usize, beats_per_sample: f64, beat: f64, max_offset: u32) {
        let fps = SmpteFrameRate::from_u8(self.mtc_fps.load(Ordering::Acquire));
        let qf_per_sec = fps.fps() * 4.0;
        let samples_per_qf = self.sample_rate.get() / qf_per_sec;
        if samples_per_qf <= 0.0 {
            return;
        }

        // `mtc_qf_phase` carries *samples remaining until the next quarter-frame
        // boundary*. It's reset to 0 on transport edges/seeks, so the first
        // boundary after a (re)start lands at sample 0 — guaranteeing the first
        // emitted piece is 0, which is exactly what the decoder needs to lock.
        let mut next_qf = self.mtc_qf_phase.load(Ordering::Acquire);
        let mut piece = self.mtc_piece.load(Ordering::Acquire);

        // Seconds-per-beat for converting beat → wall-clock SMPTE.
        let secs_per_beat = if beats_per_sample > 0.0 {
            1.0 / (beats_per_sample * self.sample_rate.get())
        } else {
            0.0
        };

        // Walk quarter-frame boundaries within [0, block_size).
        while next_qf < block_size as f64 {
            let qf_sample = next_qf.max(0.0);
            let sample_offset = (qf_sample as u32).min(max_offset);
            // Wall-clock time at this quarter-frame → SMPTE, then nibble.
            let block_beat = beat + qf_sample * beats_per_sample;
            let seconds = block_beat * secs_per_beat;
            let tc = seconds_to_smpte(seconds, fps);
            let nibble = mtc_nibble(&tc, piece, fps);
            self.emit(
                MidiEvent::mtc_quarter_frame(self.group, nibble).with_frame_offset(sample_offset),
            );
            piece = (piece + 1) & 0x07;
            next_qf += samples_per_qf;
        }

        // Roll the boundary into the next block's sample frame.
        self.mtc_qf_phase
            .store(next_qf - block_size as f64, Ordering::Release);
        self.mtc_piece.store(piece, Ordering::Release);
    }
}

/// Convert a beat position to MIDI beats (1/16 notes) for Song Position, which
/// counts sixteenth-notes from the start (M2-104 §7.6, 0xF2). Clamped to the
/// 14-bit field; the constructor masks, but clamping keeps semantics sane.
#[inline]
fn beats_to_midi_beats(beat: f64) -> u16 {
    let sixteenths = (beat.max(0.0) * 4.0).round();
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
    use tutti_core::params::Bpm;
    use tutti_midi_types::sync::{ClockTransportState, MidiClockDecoder, MtcDecoder};

    /// Minimal `Timeline` for tests: tempo + beat + playing under a
    /// switch (mirrors the one in `clip_player.rs`).
    struct TestTransport {
        beat: AtomicF64,
        tempo: f64,
        playing: StdAtomicBool,
    }

    impl TestTransport {
        fn new(tempo: f64) -> Self {
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
            Bpm(self.tempo)
        }
    }

    fn master(
        tempo: f64,
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
        let (cm, transport, cons) = master(120.0, sr);
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
        // Over ~1 beat we expect 24 ticks (allow ±1 for boundary rounding).
        assert!(
            (clock_ticks as i64 - 24).abs() <= 1,
            "expected ~24 clock ticks over one beat, got {clock_ticks}"
        );
    }

    #[test]
    fn play_at_zero_emits_start_not_continue() {
        let (cm, transport, cons) = master(120.0, 48_000.0);
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
        let (cm, transport, cons) = master(120.0, 48_000.0);
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
        let (cm, transport, cons) = master(120.0, 48_000.0);
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
        let (cm, transport, cons) = master(120.0, 48_000.0);
        cm.set_enabled(false);
        transport.set_playing(true);
        cm.tick(512);
        assert!(
            drain_all(&cons).is_empty(),
            "disabled master must be silent"
        );
    }

    #[test]
    fn ticks_are_sample_accurate_within_block() {
        // A block that spans exactly two clock ticks should place them at
        // distinct, ordered frame offsets inside the block.
        let sr = 48_000.0;
        let (cm, transport, cons) = master(120.0, sr);
        transport.set_playing(true);
        transport.set_beat(0.0);
        // One beat = 24000 samples; 1/24 beat = 1000 samples. A 2500-sample
        // block starting at beat 0 covers tick boundaries at 1000 and 2000.
        cm.tick(2500);
        let events: Vec<_> = drain_all(&cons)
            .into_iter()
            .filter(|e| is_status(e, 0xF8))
            .collect();
        assert_eq!(events.len(), 2, "expected 2 ticks in the block");
        assert!(
            events[0].frame_offset < events[1].frame_offset,
            "ticks ordered"
        );
        assert!((events[0].frame_offset as i64 - 1000).abs() < 4);
        assert!((events[1].frame_offset as i64 - 2000).abs() < 4);
    }

    #[test]
    fn generated_clock_round_trips_through_decoder() {
        // The strongest check: feed our own output into the decoder and confirm
        // it recovers the tempo and beat — the in-repo loopback from the plan.
        let sr = 48_000.0;
        let (cm, transport, cons) = master(120.0, sr);
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
            (tempo - 120.0).abs() < 2.0,
            "round-trip tempo ~120, got {tempo}"
        );
    }

    #[test]
    fn mtc_quarter_frames_round_trip_to_a_timecode() {
        let sr = 48_000.0;
        let (cm, transport, cons) = master(120.0, sr);
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
