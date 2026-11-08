//! MIDI Beat Clock decoder: 24-PPQN timing clock + transport messages →
//! transport state, beat position, and tempo inferred from clock intervals.
//!
//! Feed the relevant inbound System Real-Time events (Timing Clock 0xF8, Start
//! 0xFA, Continue 0xFB, Stop 0xFC) via [`MidiClockDecoder::feed`] — the
//! [`MidiEvent`]-taking entry that mirrors
//! [`MtcDecoder::feed`](crate::sync::MtcDecoder::feed) — or, if you've already
//! demultiplexed the stream, call [`tick`](MidiClockDecoder::tick) /
//! [`start_msg`](MidiClockDecoder::start_msg) etc. directly.

use crate::ump::MidiEvent;
use tutti_types::Bpm;

const PPQN: u32 = 24;
const TEMPO_WINDOW: usize = 24;

/// Whether the decoded transport is running.
///
/// Only two states: MIDI Beat Clock has no "paused" — a Stop followed by a
/// Continue is how a sender expresses one, and the position is held in the tick
/// count rather than in this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockTransportState {
    /// Not advancing. Incoming clocks still refine the tempo estimate but do not
    /// move the beat position.
    Stopped,
    /// Advancing, with the beat position tracking incoming clocks.
    Playing,
}

/// Decodes 24-PPQN MIDI timing clock messages into beat position and tempo.
#[derive(Debug, Clone)]
pub struct MidiClockDecoder {
    tick_count: u64,
    transport: ClockTransportState,
    last_timestamp_us: Option<u64>,
    interval_buf: [u64; TEMPO_WINDOW],
    interval_idx: usize,
    interval_count: usize,
    derived_tempo: Option<Bpm>,
}

impl MidiClockDecoder {
    /// Builds a stopped decoder at beat 0 with no tempo estimate.
    ///
    /// [`tempo_bpm`](Self::tempo_bpm) stays `None` until two timing clocks have
    /// arrived to measure an interval between.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tick_count: 0,
            transport: ClockTransportState::Stopped,
            last_timestamp_us: None,
            interval_buf: [0; TEMPO_WINDOW],
            interval_idx: 0,
            interval_count: 0,
            derived_tempo: None,
        }
    }

    /// Feeds one inbound [`MidiEvent`], dispatching System Real-Time transport
    /// messages to the right handler — the ergonomic entry that mirrors
    /// [`MtcDecoder::feed`](crate::sync::MtcDecoder::feed), so you can route raw
    /// input events straight in without demultiplexing the stream yourself.
    ///
    /// Recognises Timing Clock (0xF8, uses `timestamp_us` for tempo), Song
    /// Position Pointer (0xF2), Start (0xFA), Continue (0xFB), and Stop (0xFC).
    /// Any other event is ignored.
    /// Returns `true` if the event was a recognised transport message.
    pub fn feed(&mut self, event: &MidiEvent, timestamp_us: u64) -> bool {
        // System Real-Time is UMP type 0x1; the status byte is in bits 16..24.
        let w0 = event.data[0];
        if (w0 >> 28) & 0x0F != 0x1 {
            return false;
        }
        match ((w0 >> 16) & 0xFF) as u8 {
            0xF8 => self.tick(timestamp_us),
            // LSB first, then MSB: two 7-bit halves of the position.
            0xF2 => self.song_position_msg((((w0 & 0x7F) as u16) << 7) | ((w0 >> 8) & 0x7F) as u16),
            0xFA => self.start_msg(),
            0xFB => self.continue_msg(),
            0xFC => self.stop_msg(),
            _ => return false,
        }
        true
    }

    /// Processes a timing clock message (0xF8). Call with the host timestamp in microseconds.
    pub fn tick(&mut self, timestamp_us: u64) {
        if self.transport == ClockTransportState::Playing {
            self.tick_count += 1;
        }

        if let Some(last) = self.last_timestamp_us
            && timestamp_us > last
        {
            let interval = timestamp_us - last;
            self.interval_buf[self.interval_idx] = interval;
            self.interval_idx = (self.interval_idx + 1) % TEMPO_WINDOW;
            if self.interval_count < TEMPO_WINDOW {
                self.interval_count += 1;
            }
            self.update_tempo();
        }

        self.last_timestamp_us = Some(timestamp_us);
    }

    /// Handles MIDI Start (0xFA).
    pub fn start_msg(&mut self) {
        self.tick_count = 0;
        self.transport = ClockTransportState::Playing;
    }

    /// Handles Song Position Pointer (0xF2): `midi_beats` is the position in
    /// sixteenth notes (14 bits), six clock ticks each. The next Continue
    /// resumes there; a Start still begins at 0.
    pub fn song_position_msg(&mut self, midi_beats: u16) {
        self.tick_count = u64::from(midi_beats & 0x3FFF) * 6;
    }

    /// Handles MIDI Stop (0xFC).
    pub fn stop_msg(&mut self) {
        self.transport = ClockTransportState::Stopped;
    }

    /// Handles MIDI Continue (0xFB).
    pub fn continue_msg(&mut self) {
        self.transport = ClockTransportState::Playing;
    }

    /// Derived tempo from inter-tick timing, or None if insufficient data.
    pub fn tempo_bpm(&self) -> Option<Bpm> {
        self.derived_tempo
    }

    /// Current position in beats (based on tick count at 24 PPQN).
    pub fn beat_position(&self) -> f64 {
        self.tick_count as f64 / f64::from(PPQN)
    }

    /// Whether the decoded transport is currently running.
    pub fn transport_state(&self) -> ClockTransportState {
        self.transport
    }

    /// Returns the decoder to its constructed state: stopped, beat 0, no tempo.
    ///
    /// Discards the tempo estimate as well as the position, so the next two
    /// clocks must re-establish it. Call on a source change, not on a stop —
    /// [`stop_msg`](Self::stop_msg) already keeps the position for a Continue.
    pub fn reset(&mut self) {
        self.tick_count = 0;
        self.transport = ClockTransportState::Stopped;
        self.last_timestamp_us = None;
        self.interval_buf = [0; TEMPO_WINDOW];
        self.interval_idx = 0;
        self.interval_count = 0;
        self.derived_tempo = None;
    }

    fn update_tempo(&mut self) {
        if self.interval_count < 2 {
            return;
        }
        let sum: u64 = self.interval_buf[..self.interval_count].iter().sum();
        let avg_us = sum as f64 / self.interval_count as f64;
        if avg_us > 0.0 {
            // avg_us = microseconds per tick
            // beats_per_second = 1_000_000 / (avg_us * PPQN)
            // BPM = beats_per_second * 60
            let bpm = 60_000_000.0 / (avg_us * f64::from(PPQN));
            if (20.0..=300.0).contains(&bpm) {
                self.derived_tempo = Bpm::new(bpm);
            }
        }
    }
}

impl Default for MidiClockDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
