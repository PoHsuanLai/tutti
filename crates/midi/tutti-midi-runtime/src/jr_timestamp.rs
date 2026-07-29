//! MIDI 2.0 Jitter Reduction (JR) timestamps (M2-104 §2.1.4).
//!
//! JR timestamps let a receiver reconstruct the *original* spacing of a UMP
//! stream that a transport may have bunched up or spread out. The sender stamps
//! the stream with a monotonic 16-bit counter running at the JR reference clock
//! (31.25 kHz — one tick every 32 µs); the receiver reads the deltas between
//! successive stamps to recover inter-event intervals.
//!
//! [`tutti_midi_types::MidiEvent::jr_timestamp`] is the wire constructor. This
//! module is the *engine* around it:
//! - [`JrClock`] — the shared tick reference (samples ↔ 16-bit ticks).
//! - [`JrStamper`] — outbound: interleave a JR Timestamp before each event,
//!   derived from the event's sample-accurate `frame_offset`. Pure.
//! - [`JrStream`] — a stamper plus the running sample origin one outbound wire
//!   stamps against. This is what a pump holds; see its doc for why the origin
//!   is per-wire rather than per-caller.
//! - [`JrReceiver`] — inbound: read stamps, reconstruct the delay before the
//!   next event as a [`Duration`].
//!
//! The clock, stamper and receiver are pure (no interior transport, no I/O), so
//! a stamp → observe loopback recovers the injected spacing — see the tests.

use std::time::Duration;

use tutti_midi_types::ump::MidiEvent;

/// The JR timestamp reference clock: **31 250 ticks per second** (one tick every
/// 32 µs), a 16-bit counter that wraps roughly every 2.097 s (M2-104 §2.1.4).
pub const JR_TICKS_PER_SECOND: u32 = 31_250;

/// Seconds per JR tick — the reciprocal of [`JR_TICKS_PER_SECOND`].
pub const JR_SECONDS_PER_TICK: f64 = 1.0 / JR_TICKS_PER_SECOND as f64;

/// Converts sample positions to 16-bit JR ticks at a fixed sample rate. Cheap,
/// `Copy`, and stateless — the shared reference both [`JrStamper`] and
/// [`JrReceiver`] reason in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JrClock {
    sample_rate: f64,
}

impl JrClock {
    /// A clock for a stream running at `sample_rate` Hz.
    pub fn new(sample_rate: f64) -> Self {
        Self { sample_rate }
    }

    /// The 16-bit JR tick a sample offset maps to (wrapping at 0x1_0000). A whole
    /// stream is stamped relative to some origin, so pass offsets from that origin.
    #[inline]
    pub fn ticks_at(&self, sample_offset: u64) -> u16 {
        // ticks = samples * (JR_ticks/sec) / (samples/sec)
        let ticks = (sample_offset as f64) * JR_TICKS_PER_SECOND as f64 / self.sample_rate;
        (ticks as u64 & 0xFFFF) as u16
    }
}

/// Outbound JR stamping: turns a bare UMP stream into a JR-timestamped one by
/// prefixing each event with a [`MidiEvent::jr_timestamp`] derived from the
/// event's `frame_offset`. Stateless across calls (each `stamp_block` is relative
/// to the block it's given); carry a running sample origin in the caller if you
/// need cross-block continuity.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JrStamper {
    clock: JrClock,
}

impl JrStamper {
    /// A stamper for `sample_rate` Hz.
    ///
    /// There is no group: JR Timestamps are utility messages, which M2-104-UM
    /// §2.1.2 defines as groupless — a stamp applies to the stream, not to one
    /// group within it.
    pub fn new(sample_rate: f64) -> Self {
        Self {
            clock: JrClock::new(sample_rate),
        }
    }

    /// Return a new stream: each input event preceded by a JR Timestamp for its
    /// `frame_offset`. `origin_samples` is the absolute sample position of this
    /// block's frame-offset zero, so stamps stay monotonic across blocks.
    ///
    /// Prefer [`JrStream`] over calling this directly: the origin has to advance
    /// by exactly the right amount between blocks, and that is the part a caller
    /// gets wrong.
    pub fn stamp_block(&self, events: &[MidiEvent], origin_samples: u64) -> Vec<MidiEvent> {
        let mut out = Vec::with_capacity(events.len() * 2);
        for ev in events {
            let ticks = self.clock.ticks_at(origin_samples + ev.frame_offset as u64);
            out.push(MidiEvent::jr_timestamp(ticks).with_frame_offset(ev.frame_offset));
            out.push(*ev);
        }
        out
    }

    /// How far the origin must advance after stamping `events` — one past the
    /// furthest frame offset in the block, or zero for an empty block.
    ///
    /// Split out from [`JrStream::stamp`] so the arithmetic that keeps stamps
    /// monotonic is stated once and testable on its own.
    #[inline]
    pub fn block_span(events: &[MidiEvent]) -> u64 {
        events
            .iter()
            .map(|e| e.frame_offset as u64)
            .max()
            .map_or(0, |m| m + 1)
    }
}

/// One outbound JR-stamped stream: a [`JrStamper`] plus the running sample origin
/// its stamps are relative to.
///
/// [`JrStamper`] is deliberately pure, so the origin has to live *somewhere*, and
/// "somewhere" is per **wire**, not per caller. A single endpoint fed by two
/// pumps — the clock master and the track MIDI-out both reach the same hardware
/// port — must share one origin, or the two interleave stamps that walk
/// backwards and a receiver reconstructs the wrong spacing. Owning it here is
/// what makes that structural rather than a convention each pump has to keep.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JrStream {
    stamper: JrStamper,
    /// Absolute sample position of the next block's frame-offset zero.
    origin_samples: u64,
}

impl JrStream {
    /// A stream stamping at `sample_rate` Hz, starting at origin 0.
    pub fn new(sample_rate: f64) -> Self {
        Self::with_stamper(JrStamper::new(sample_rate))
    }

    /// A stream over an existing stamper.
    pub fn with_stamper(stamper: JrStamper) -> Self {
        Self {
            stamper,
            origin_samples: 0,
        }
    }

    /// Stamp one block and advance the origin past it, so the next call
    /// continues monotonically.
    pub fn stamp(&mut self, events: &[MidiEvent]) -> Vec<MidiEvent> {
        let out = self.stamper.stamp_block(events, self.origin_samples);
        self.origin_samples = self
            .origin_samples
            .wrapping_add(JrStamper::block_span(events));
        out
    }

    /// The absolute sample position the next [`stamp`](Self::stamp) will start
    /// from. Exposed for tests and diagnostics.
    pub fn origin_samples(&self) -> u64 {
        self.origin_samples
    }

    /// The underlying stamper.
    pub fn stamper(&self) -> &JrStamper {
        &self.stamper
    }
}

/// Inbound JR reconstruction: reads JR Timestamps out of a stream and reports the
/// intended delay before the *next* non-timestamp event. Stateful — it remembers
/// the previous stamp to compute a 16-bit-wrap-correct delta.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct JrReceiver {
    /// The last-seen JR tick, `None` until the first timestamp arrives.
    prev: Option<u16>,
    /// The tick of the timestamp most recently seen but not yet "spent" on an
    /// event — the delay applied to the next event that follows it.
    pending: Option<u16>,
}

impl JrReceiver {
    /// A fresh receiver with no history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one event. On a JR Timestamp, records it and returns `None` (its delay
    /// applies to what follows). On any other event, returns the reconstructed
    /// delay since the previous timestamp — `Some(Duration::ZERO)` when two events
    /// share a timestamp, `None` before any timestamp has been seen.
    pub fn observe(&mut self, event: &MidiEvent) -> Option<Duration> {
        if let Some(ticks) = event.jr_timestamp_value() {
            // Delta from the previous timestamp, honoring the 16-bit wrap.
            let delay = self.prev.map(|p| wrap_delta(p, ticks));
            self.prev = Some(ticks);
            self.pending = Some(ticks);
            return delay.map(ticks_to_duration);
        }
        // A non-timestamp event: it happens at the pending timestamp. The interval
        // to report is the gap from the previous event's timestamp, already
        // returned when that timestamp arrived, so a bare event carries no *new*
        // delay — return ZERO if we have a running clock, else None.
        self.pending.map(|_| Duration::ZERO)
    }
}

/// Forward distance from `from` to `to` on a 16-bit wrapping counter.
#[inline]
fn wrap_delta(from: u16, to: u16) -> u16 {
    to.wrapping_sub(from)
}

/// Convert a JR tick count to a wall-clock [`Duration`].
#[inline]
fn ticks_to_duration(ticks: u16) -> Duration {
    Duration::from_secs_f64(ticks as f64 * JR_SECONDS_PER_TICK)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_maps_samples_to_ticks_monotonically() {
        let clock = JrClock::new(48_000.0);
        // 48000 samples = 1 s = 31250 ticks, which wraps: 31250 & 0xFFFF = 31250.
        assert_eq!(clock.ticks_at(0), 0);
        assert_eq!(clock.ticks_at(48_000), 31_250);
        // Half a second → half the ticks.
        assert_eq!(clock.ticks_at(24_000), 15_625);
    }

    #[test]
    fn clock_wraps_at_16_bits() {
        // At exactly the JR reference rate, one sample is one tick, so the wrap
        // boundary is integer-exact (no float rounding at the edge).
        let clock = JrClock::new(JR_TICKS_PER_SECOND as f64);
        assert_eq!(clock.ticks_at(0xFFFF), 0xFFFF);
        assert_eq!(clock.ticks_at(0x1_0000), 0, "wraps back to zero");
        assert_eq!(clock.ticks_at(0x1_0001), 1);
    }

    /// The span is one *past* the furthest offset, so the next block's origin
    /// does not re-stamp the sample the last event sat on.
    #[test]
    fn a_blocks_span_is_one_past_its_furthest_offset() {
        let events = [
            MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0),
            MidiEvent::note_off(0, 0, 60, 0).with_frame_offset(511),
        ];
        assert_eq!(JrStamper::block_span(&events), 512);
        assert_eq!(
            JrStamper::block_span(&[]),
            0,
            "an empty block spans nothing"
        );
    }

    /// A stream advances its own origin, so successive blocks keep climbing.
    #[test]
    fn a_stream_advances_its_origin_across_blocks() {
        let mut stream = JrStream::new(48_000.0);
        let events = [
            MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0),
            MidiEvent::note_off(0, 0, 60, 0).with_frame_offset(24_000),
        ];

        let first = stream.stamp(&events);
        assert_eq!(first[0].jr_timestamp_value(), Some(0));
        assert_eq!(stream.origin_samples(), 24_001);

        // The second block's first event is stamped from the new origin, not zero.
        let second = stream.stamp(&events);
        assert_eq!(second[0].jr_timestamp_value(), Some(15_625));
        assert_eq!(stream.origin_samples(), 48_002);
    }

    /// The reason the origin lives on the stream: two producers feeding one wire
    /// must share it. Stamping both through one `JrStream` keeps the sequence
    /// climbing; a per-producer origin would restart each at zero and hand the
    /// receiver stamps that walk backwards.
    ///
    /// The events sit at a *block-sized* offset, not zero: a JR tick is 32 µs —
    /// about 1.5 samples at 48 kHz — so a one-sample advance rounds to the same
    /// tick and would prove nothing either way.
    #[test]
    fn two_producers_on_one_stream_keep_stamps_monotonic() {
        let mut wire = JrStream::new(48_000.0);

        // One 512-frame block each, the event at the block's end.
        let clock_block = [MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(511)];
        let track_block = [MidiEvent::note_on(0, 0, 64, 0x8000).with_frame_offset(511)];

        let from_clock = wire.stamp(&clock_block);
        let from_track = wire.stamp(&track_block);

        let first = from_clock[0].jr_timestamp_value().unwrap();
        let second = from_track[0].jr_timestamp_value().unwrap();
        assert!(
            second > first,
            "the second producer must stamp later than the first: {first} then {second}"
        );
        assert_eq!(wire.origin_samples(), 1024, "two 512-frame blocks");
    }

    #[test]
    fn stamp_block_prefixes_each_event() {
        let stamper = JrStamper::new(48_000.0);
        let events = [
            MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0),
            MidiEvent::note_off(0, 0, 60, 0).with_frame_offset(24_000),
        ];
        let out = stamper.stamp_block(&events, 0);
        assert_eq!(out.len(), 4, "one timestamp + one event, twice");
        assert_eq!(out[0].jr_timestamp_value(), Some(0));
        assert!(out[1].is_note_on());
        assert_eq!(out[2].jr_timestamp_value(), Some(15_625));
        assert!(out[3].is_note_off());
    }

    #[test]
    fn stamp_then_observe_recovers_spacing() {
        // Two events 0.25 s apart at 48 kHz → the receiver reconstructs ~0.25 s.
        let sr = 48_000.0;
        let stamper = JrStamper::new(sr);
        let quarter_second = (sr * 0.25) as u32; // 12000 samples
        let events = [
            MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0),
            MidiEvent::note_on(0, 0, 64, 0x8000).with_frame_offset(quarter_second),
        ];
        let stream = stamper.stamp_block(&events, 0);

        let mut rx = JrReceiver::new();
        let mut recovered = Vec::new();
        for ev in &stream {
            if let Some(d) = rx.observe(ev) {
                recovered.push(d);
            }
        }
        // First timestamp: no prior → no delay. Its note: ZERO. Second timestamp:
        // delta ~0.25 s. Its note: ZERO.
        // The meaningful reconstruction is the second timestamp's delta.
        let nonzero: Vec<_> = recovered.iter().filter(|d| **d > Duration::ZERO).collect();
        assert_eq!(nonzero.len(), 1, "exactly one inter-event gap");
        let gap = *nonzero[0];
        let expected = Duration::from_secs_f64(0.25);
        let diff = if gap > expected {
            gap - expected
        } else {
            expected - gap
        };
        assert!(
            diff < Duration::from_secs_f64(JR_SECONDS_PER_TICK * 2.0),
            "recovered {gap:?} within one tick of {expected:?}"
        );
    }

    #[test]
    fn observe_before_any_timestamp_is_none() {
        let mut rx = JrReceiver::new();
        assert_eq!(rx.observe(&MidiEvent::note_on(0, 0, 60, 0x8000)), None);
    }

    #[test]
    fn observe_handles_16bit_wrap() {
        // Two timestamps straddling the wrap: 0xFFF0 → 0x0010 is a delta of 0x20.
        let mut rx = JrReceiver::new();
        assert_eq!(rx.observe(&MidiEvent::jr_timestamp(0xFFF0)), None);
        let d = rx
            .observe(&MidiEvent::jr_timestamp(0x0010))
            .expect("has a prior stamp");
        assert_eq!(d, ticks_to_duration(0x20));
    }
}
