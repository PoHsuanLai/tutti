//! MIDI Beat Clock decoder: 24-PPQN timing clock + transport messages →
//! transport state, beat position, and tempo inferred from clock intervals.
//!
//! Feed the relevant inbound System Real-Time events (Timing Clock 0xF8, Start
//! 0xFA, Continue 0xFB, Stop 0xFC) via [`MidiClockDecoder::feed`] — the
//! [`MidiEvent`](crate::MidiEvent)-taking entry that mirrors
//! [`MtcDecoder::feed`](crate::sync::MtcDecoder::feed) — or, if you've already
//! demultiplexed the stream, call [`tick`](MidiClockDecoder::tick) /
//! [`start_msg`](MidiClockDecoder::start_msg) etc. directly.

use crate::ump::MidiEvent;
use tutti_types::Bpm;

const PPQN: u32 = 24;
const TEMPO_WINDOW: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockTransportState {
    Stopped,
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
    derived_tempo: Option<f64>,
}

impl MidiClockDecoder {
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

    /// Feed one inbound [`MidiEvent`], dispatching System Real-Time transport
    /// messages to the right handler — the ergonomic entry that mirrors
    /// [`MtcDecoder::feed`](crate::sync::MtcDecoder::feed), so you can route raw
    /// input events straight in without demultiplexing the stream yourself.
    ///
    /// Recognises Timing Clock (0xF8, uses `timestamp_us` for tempo), Start
    /// (0xFA), Continue (0xFB), and Stop (0xFC). Any other event is ignored.
    /// Returns `true` if the event was a recognised transport message.
    pub fn feed(&mut self, event: &MidiEvent, timestamp_us: u64) -> bool {
        // System Real-Time is UMP type 0x1; the status byte is in bits 16..24.
        let w0 = event.data[0];
        if (w0 >> 28) & 0x0F != 0x1 {
            return false;
        }
        match ((w0 >> 16) & 0xFF) as u8 {
            0xF8 => self.tick(timestamp_us),
            0xFA => self.start_msg(),
            0xFB => self.continue_msg(),
            0xFC => self.stop_msg(),
            _ => return false,
        }
        true
    }

    /// Process a timing clock message (0xF8). Call with the host timestamp in microseconds.
    pub fn tick(&mut self, timestamp_us: u64) {
        if self.transport == ClockTransportState::Playing {
            self.tick_count += 1;
        }

        if let Some(last) = self.last_timestamp_us {
            if timestamp_us > last {
                let interval = timestamp_us - last;
                self.interval_buf[self.interval_idx] = interval;
                self.interval_idx = (self.interval_idx + 1) % TEMPO_WINDOW;
                if self.interval_count < TEMPO_WINDOW {
                    self.interval_count += 1;
                }
                self.update_tempo();
            }
        }

        self.last_timestamp_us = Some(timestamp_us);
    }

    /// Handle MIDI Start (0xFA).
    pub fn start_msg(&mut self) {
        self.tick_count = 0;
        self.transport = ClockTransportState::Playing;
    }

    /// Handle MIDI Stop (0xFC).
    pub fn stop_msg(&mut self) {
        self.transport = ClockTransportState::Stopped;
    }

    /// Handle MIDI Continue (0xFB).
    pub fn continue_msg(&mut self) {
        self.transport = ClockTransportState::Playing;
    }

    /// Derived tempo from inter-tick timing, or None if insufficient data.
    pub fn tempo_bpm(&self) -> Option<Bpm> {
        self.derived_tempo.map(Bpm)
    }

    /// Current position in beats (based on tick count at 24 PPQN).
    pub fn beat_position(&self) -> f64 {
        self.tick_count as f64 / f64::from(PPQN)
    }

    pub fn transport_state(&self) -> ClockTransportState {
        self.transport
    }

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
                self.derived_tempo = Some(bpm);
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
mod tests {
    use super::*;

    fn us_per_tick(bpm: f64) -> u64 {
        // 1 beat = PPQN ticks; 1 beat at bpm = 60/bpm seconds = 60_000_000/bpm us
        (60_000_000.0 / (bpm * PPQN as f64)) as u64
    }

    #[test]
    fn test_basic_beat_counting() {
        let mut clock = MidiClockDecoder::new();
        clock.start_msg();
        assert_eq!(clock.transport_state(), ClockTransportState::Playing);
        assert!((clock.beat_position() - 0.0).abs() < 0.001);

        let interval = us_per_tick(120.0);
        let mut ts = 0u64;
        for _ in 0..24 {
            clock.tick(ts);
            ts += interval;
        }
        assert!((clock.beat_position() - 1.0).abs() < 0.001);
    }

    #[test]
    fn feed_dispatches_transport_events_like_the_direct_methods() {
        let mut clock = MidiClockDecoder::new();
        // Start via a real MidiEvent, then advance a beat of clock ticks.
        assert!(clock.feed(&MidiEvent::start(0), 0));
        assert_eq!(clock.transport_state(), ClockTransportState::Playing);

        let interval = us_per_tick(120.0);
        let mut ts = 0u64;
        for _ in 0..24 {
            assert!(clock.feed(&MidiEvent::timing_clock(0), ts));
            ts += interval;
        }
        assert!((clock.beat_position() - 1.0).abs() < 0.001);

        assert!(clock.feed(&MidiEvent::stop(0), ts));
        assert_eq!(clock.transport_state(), ClockTransportState::Stopped);

        // A non-transport event is ignored and reported as unrecognised.
        assert!(!clock.feed(&MidiEvent::note_on(0, 0, 60, 0x8000), ts));
    }

    #[test]
    fn test_tempo_derivation_120bpm() {
        let mut clock = MidiClockDecoder::new();
        clock.start_msg();

        let interval = us_per_tick(120.0);
        let mut ts = 0u64;
        // Feed enough ticks for tempo stabilization
        for _ in 0..48 {
            clock.tick(ts);
            ts += interval;
        }

        let tempo = clock.tempo_bpm().unwrap();
        assert!(
            !tempo.differs_from(Bpm(120.0), 1.0),
            "Expected ~120 BPM, got {tempo:?}"
        );
    }

    #[test]
    fn test_tempo_derivation_140bpm() {
        let mut clock = MidiClockDecoder::new();
        clock.start_msg();

        let interval = us_per_tick(140.0);
        let mut ts = 0u64;
        for _ in 0..48 {
            clock.tick(ts);
            ts += interval;
        }

        let tempo = clock.tempo_bpm().unwrap();
        assert!(
            !tempo.differs_from(Bpm(140.0), 1.0),
            "Expected ~140 BPM, got {tempo:?}"
        );
    }

    #[test]
    fn test_start_resets_position() {
        let mut clock = MidiClockDecoder::new();
        clock.start_msg();

        let interval = us_per_tick(120.0);
        let mut ts = 0u64;
        for _ in 0..48 {
            clock.tick(ts);
            ts += interval;
        }
        assert!(clock.beat_position() > 1.0);

        clock.start_msg();
        assert!((clock.beat_position() - 0.0).abs() < 0.001);
    }

    #[test]
    fn test_stop_pauses_counting() {
        let mut clock = MidiClockDecoder::new();
        clock.start_msg();

        let interval = us_per_tick(120.0);
        let mut ts = 0u64;
        for _ in 0..24 {
            clock.tick(ts);
            ts += interval;
        }
        let pos_before_stop = clock.beat_position();

        clock.stop_msg();
        for _ in 0..24 {
            clock.tick(ts);
            ts += interval;
        }
        // Position should not advance while stopped
        assert!(
            (clock.beat_position() - pos_before_stop).abs() < 0.001,
            "Position should not advance while stopped"
        );
    }

    #[test]
    fn test_continue_resumes() {
        let mut clock = MidiClockDecoder::new();
        clock.start_msg();

        let interval = us_per_tick(120.0);
        let mut ts = 0u64;
        for _ in 0..24 {
            clock.tick(ts);
            ts += interval;
        }
        let pos_at_stop = clock.beat_position();

        clock.stop_msg();
        // Tick while stopped
        for _ in 0..24 {
            clock.tick(ts);
            ts += interval;
        }

        clock.continue_msg();
        for _ in 0..24 {
            clock.tick(ts);
            ts += interval;
        }
        // Should have advanced by 1 beat from the stop position
        assert!(
            (clock.beat_position() - (pos_at_stop + 1.0)).abs() < 0.001,
            "Continue should resume from stop position"
        );
    }

    #[test]
    fn test_no_tempo_before_enough_ticks() {
        let clock = MidiClockDecoder::new();
        assert!(clock.tempo_bpm().is_none());
    }

    #[test]
    fn test_reset() {
        let mut clock = MidiClockDecoder::new();
        clock.start_msg();

        let interval = us_per_tick(120.0);
        let mut ts = 0u64;
        for _ in 0..48 {
            clock.tick(ts);
            ts += interval;
        }

        clock.reset();
        assert!(clock.tempo_bpm().is_none());
        assert!((clock.beat_position() - 0.0).abs() < 0.001);
        assert_eq!(clock.transport_state(), ClockTransportState::Stopped);
    }

    #[test]
    fn test_beat_position_at_4_beats() {
        let mut clock = MidiClockDecoder::new();
        clock.start_msg();

        let interval = us_per_tick(120.0);
        let mut ts = 0u64;
        for _ in 0..96 {
            clock.tick(ts);
            ts += interval;
        }
        // 96 ticks / 24 PPQN = 4 beats
        assert!((clock.beat_position() - 4.0).abs() < 0.001);
    }
}
