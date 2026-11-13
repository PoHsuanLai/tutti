//! MIDI Beat Clock decoder: 24-PPQN timing clock + transport messages →
//! transport state, beat position, and tempo inferred from clock intervals.
//!
//! Feed the relevant inbound System Real-Time events (Timing Clock 0xF8, Start
//! 0xFA, Continue 0xFB, Stop 0xFC) via [`MidiClockDecoder::feed`] — the
//! [`Ump`]-taking entry that mirrors
//! [`MtcDecoder::feed`](crate::MtcDecoder::feed) — or, if you've already
//! demultiplexed the stream, call [`tick`](MidiClockDecoder::tick) /
//! [`start_msg`](MidiClockDecoder::start_msg) etc. directly.

use std::time::{Duration, Instant};

use tutti_types::{Beat, Bpm};
use tutti_ump::{Message, System, Ump};

const TEMPO_WINDOW: usize = 24;

/// Whether the decoded transport is running.
///
/// Only two states: MIDI Beat Clock has no "paused" — a Stop followed by a
/// Continue is how a sender expresses one, and the position is held in the tick
/// count rather than in this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    last_tick: Option<Instant>,
    interval_buf: [Duration; TEMPO_WINDOW],
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
            last_tick: None,
            interval_buf: [Duration::ZERO; TEMPO_WINDOW],
            interval_idx: 0,
            interval_count: 0,
            derived_tempo: None,
        }
    }

    /// Feeds one inbound [`Ump`], dispatching System Real-Time transport
    /// messages to the right handler — the ergonomic entry that mirrors
    /// [`MtcDecoder::feed`](crate::MtcDecoder::feed), so you can route raw
    /// input packets straight in without demultiplexing the stream yourself.
    ///
    /// Recognises Timing Clock (0xF8, uses `at` for tempo), Song
    /// Position Pointer (0xF2), Start (0xFA), Continue (0xFB), and Stop (0xFC).
    /// Any other packet is ignored.
    /// Returns `true` if the packet was a recognised transport message.
    pub fn feed(&mut self, ump: &Ump, at: Instant) -> bool {
        let Ok(Message::System(_, system)) = (*ump).decode() else {
            return false;
        };
        match system {
            System::TimingClock => self.tick(at),
            System::SongPosition(position) => self.song_position_msg(position.get()),
            System::Start => self.start_msg(),
            System::Continue => self.continue_msg(),
            System::Stop => self.stop_msg(),
            _ => return false,
        }
        true
    }

    /// Processes a timing clock message (0xF8) that arrived at `at`, the
    /// instant the OS delivered it. The tempo is measured from the spacing of
    /// these, so they must be arrival times, not a count.
    pub fn tick(&mut self, at: Instant) {
        if self.transport == ClockTransportState::Playing {
            self.tick_count += 1;
        }

        if let Some(last) = self.last_tick
            && at > last
        {
            let interval = at - last;
            self.interval_buf[self.interval_idx] = interval;
            self.interval_idx = (self.interval_idx + 1) % TEMPO_WINDOW;
            if self.interval_count < TEMPO_WINDOW {
                self.interval_count += 1;
            }
            self.update_tempo();
        }

        self.last_tick = Some(at);
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
    pub fn beat_position(&self) -> Beat {
        Beat::new(self.tick_count as f64 / f64::from(System::TIMING_CLOCKS_PER_QUARTER))
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
        self.last_tick = None;
        self.interval_buf = [Duration::ZERO; TEMPO_WINDOW];
        self.interval_idx = 0;
        self.interval_count = 0;
        self.derived_tempo = None;
    }

    fn update_tempo(&mut self) {
        if self.interval_count < 2 {
            return;
        }
        let sum: Duration = self.interval_buf[..self.interval_count].iter().sum();
        let avg = sum.as_secs_f64() / self.interval_count as f64;
        if avg > 0.0 {
            // avg = seconds per tick
            // beats_per_second = 1 / (avg * PPQN)
            // BPM = beats_per_second * 60
            let bpm = 60.0 / (avg * f64::from(System::TIMING_CLOCKS_PER_QUARTER));
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
