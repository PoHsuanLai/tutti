const PPQN: u32 = 24;
const TEMPO_WINDOW: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockTransportState {
    Stopped,
    Playing,
}

/// Decodes 24-PPQN MIDI timing clock messages into beat position and tempo.
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

    /// Derived tempo in BPM from inter-tick timing, or None if insufficient data.
    pub fn tempo_bpm(&self) -> Option<f64> {
        self.derived_tempo
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
            (tempo - 120.0).abs() < 1.0,
            "Expected ~120 BPM, got {tempo}"
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
            (tempo - 140.0).abs() < 1.0,
            "Expected ~140 BPM, got {tempo}"
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
