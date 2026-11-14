//! MIDI Beat Clock decoder: 24-PPQN timing clock + transport messages →
//! transport state, beat position, and tempo inferred from clock intervals.
//!
//! Feed the relevant inbound System Real-Time events (Timing Clock 0xF8, Start
//! 0xFA, Continue 0xFB, Stop 0xFC) via [`MidiClockDecoder::feed`] — the
//! [`Ump`]-taking entry that mirrors
//! [`MtcDecoder::feed`](crate::MtcDecoder::feed) — or, if you've already
//! demultiplexed the stream, call [`tick`](MidiClockDecoder::tick) /
//! [`start_msg`](MidiClockDecoder::start_msg) etc. directly.

use tutti_types::{Beat, Bpm, Seconds};
use tutti_ump::{Message, System, Ump};

use super::period::PeriodFit;

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
///
/// # The tempo estimate
///
/// Timing Clocks come 24 to a quarter note (M1 p.30), so the tempo is the
/// spacing of their arrivals. The spacing is jittery (the transport and the OS
/// add a delay of their own to each arrival), so the decoder fits a line to the
/// arrival times of the last [`WINDOW_TICKS`](Self::WINDOW_TICKS) ticks and reads
/// the period from its slope: a least-squares fit, whose error for independent
/// arrival jitter of standard deviation `sigma` is `0.0104 * sigma` in period
/// over a full window, where the mean of the intervals gives `0.030 * sigma`.
/// The fit is what tutti chooses; the spec fixes only the 24 per quarter.
///
/// # Lock
///
/// The decoder is [`locked`](Self::locked) once
/// [`LOCK_TICKS`](Self::LOCK_TICKS) consecutive ticks have each arrived within
/// [`GAP_TOLERANCE`](Self::GAP_TOLERANCE) of the period the ticks before them
/// fit: a quarter note of steady clock. An arrival outside that tolerance is not
/// jitter (a missed tick, a restart, a tempo step too large to be a ramp): the
/// fit restarts from it and the lock is released. [`drop_out`](Self::drop_out)
/// releases it too, for a clock that stopped coming.
#[derive(Debug, Clone)]
pub struct MidiClockDecoder {
    tick_count: u64,
    transport: ClockTransportState,
    last_tick: Option<Seconds>,
    fit: PeriodFit,
    derived_tempo: Option<Bpm>,
}

impl MidiClockDecoder {
    /// Ticks the tempo fit looks back over: 48, two quarter notes. Tutti's
    /// choice: a window of `n` ticks has `0.0104 * sigma` period error at 48 and
    /// follows a tempo ramp with a lag of half the window, one beat, which a
    /// longer window would stretch.
    pub const WINDOW_TICKS: usize = super::period::WINDOW;

    /// Consecutive consistent ticks that lock the decoder: 24, one quarter note
    /// (tutti's choice; the spec gives none). It is long enough that a lock means
    /// a steady source rather than two ticks that happened to be close, and short
    /// enough that the transport follows within half a second at 120 bpm.
    pub const LOCK_TICKS: usize = 24;

    /// How far an interval may stray from the fitted period, as a fraction of
    /// it, and still be jitter: one half. Tutti's choice: a tick a whole period
    /// late is a missed tick and a tick half a period early is a second sender,
    /// while OS jitter is a small fraction of a period at every tempo the
    /// decoder reads.
    pub const GAP_TOLERANCE: f64 = 0.5;

    /// How long the clock may be silent before [`drop_out`](Self::drop_out)
    /// releases the lock: half a second (tutti's choice). The slowest tempo
    /// read, 20 bpm, ticks every 125 ms, so this is four missed ticks there and
    /// many more at any ordinary tempo.
    pub const DROPOUT: Seconds = Seconds::new(0.5);

    /// The slowest tempo the estimate reports, in bpm.
    const SLOWEST: Bpm = Bpm::of(20.0);
    /// The fastest tempo the estimate reports, in bpm.
    const FASTEST: Bpm = Bpm::of(300.0);

    /// Builds a stopped decoder at beat 0 with no tempo estimate.
    ///
    /// [`tempo_bpm`](Self::tempo_bpm) stays `None` until three timing clocks have
    /// arrived to fit a line through.
    #[must_use]
    pub fn new() -> Self {
        Self {
            tick_count: 0,
            transport: ClockTransportState::Stopped,
            last_tick: None,
            fit: PeriodFit::new(),
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
    pub fn feed(&mut self, ump: &Ump, at: Seconds) -> bool {
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

    /// Processes a timing clock message (0xF8) that arrived at `at`, on the one
    /// clock every `at` and `now` of this decoder is read on (its zero is the
    /// caller's; only differences are read). The tempo is measured from the
    /// spacing of these, so they must be arrival times, not a count.
    pub fn tick(&mut self, at: Seconds) {
        if self.transport == ClockTransportState::Playing {
            self.tick_count += 1;
        }

        if let Some(last) = self.last_tick {
            if at <= last {
                return;
            }
            let interval = (at - last).get();
            if let Some(period) = self.fit.period().map(Seconds::get)
                && (interval - period).abs() > Self::GAP_TOLERANCE * period
            {
                self.fit.clear();
                self.derived_tempo = None;
            }
        }
        self.fit.push(at);
        self.last_tick = Some(at);
        self.update_tempo();
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

    /// The tempo the fit reads from the arrival spacing, or `None` before three
    /// ticks have arrived, and when the fit lies outside 20 to 300 bpm.
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

    /// Whether the clock is steady: [`LOCK_TICKS`](Self::LOCK_TICKS) consecutive
    /// ticks have arrived within [`GAP_TOLERANCE`](Self::GAP_TOLERANCE) of the
    /// fitted period, and none has been missed since.
    pub fn locked(&self) -> bool {
        self.fit.len() >= Self::LOCK_TICKS
    }

    /// Releases the lock and restarts the estimate when no tick has arrived for
    /// [`DROPOUT`](Self::DROPOUT) before `now`, and says whether it did: `true`
    /// once per silence, with a clock that had ticked. The position and the
    /// transport state are kept: a clock that went quiet did not say Stop.
    pub fn drop_out(&mut self, now: Seconds) -> bool {
        let Some(last) = self.last_tick else {
            return false;
        };
        if now - last <= Self::DROPOUT {
            return false;
        }
        self.fit.clear();
        self.derived_tempo = None;
        self.last_tick = None;
        true
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
        self.fit.clear();
        self.derived_tempo = None;
    }

    fn update_tempo(&mut self) {
        self.derived_tempo = None;
        if self.fit.len() < 3 {
            return;
        }
        let Some(period) = self.fit.period().filter(|p| p.get() > 0.0) else {
            return;
        };
        // Seconds per tick, times the ticks in a quarter note, is seconds per beat.
        let beat = period * f64::from(System::TIMING_CLOCKS_PER_QUARTER);
        self.derived_tempo =
            Bpm::from_beat_period(beat).filter(|bpm| (Self::SLOWEST..=Self::FASTEST).contains(bpm));
    }
}

impl Default for MidiClockDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
