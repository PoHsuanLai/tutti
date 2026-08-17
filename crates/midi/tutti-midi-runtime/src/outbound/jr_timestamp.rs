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
//! - [`JrClockEmitter`] — outbound: the periodic JR *Clock* (§7.2.2.1), which
//!   is what makes the stamps usable at all. §7.2.2.3: a receiver that has seen
//!   no JR Clock renders messages "as soon as possible", ignoring every stamp.
//! - [`JrStream`] — a stamper plus the running sample origin one outbound wire
//!   stamps against, and optionally the clock cadence. This is what a pump
//!   holds; see its doc for why the origin is per-wire rather than per-caller.
//! - [`JrReceiver`] — inbound: read stamps, reconstruct the delay before the
//!   next event as a [`Duration`].
//!
//! The clock, stamper and receiver are pure (no interior transport, no I/O), so
//! a stamp → observe loopback recovers the injected spacing — see the tests.

use std::time::Duration;

use tutti_core::SampleRate;
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
    sample_rate: SampleRate,
}

impl JrClock {
    /// A clock for a stream running at `sample_rate` Hz.
    pub fn new(sample_rate: impl Into<SampleRate>) -> Self {
        Self {
            sample_rate: sample_rate.into(),
        }
    }

    /// The 16-bit JR tick a sample offset maps to (wrapping at 0x1_0000). A whole
    /// stream is stamped relative to some origin, so pass offsets from that origin.
    #[inline]
    pub fn ticks_at(&self, sample_offset: u64) -> u16 {
        // ticks = samples * (JR_ticks/sec) / (samples/sec)
        let ticks = (sample_offset as f64) * JR_TICKS_PER_SECOND as f64 / self.sample_rate.get();
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
    pub fn new(sample_rate: impl Into<SampleRate>) -> Self {
        Self {
            clock: JrClock::new(sample_rate),
        }
    }

    /// Write each input event into `out`, preceded by a JR Timestamp for its
    /// `frame_offset`. `origin_samples` is the absolute sample position of this
    /// block's frame-offset zero, so stamps stay monotonic across blocks.
    ///
    /// Prefer [`JrStream`] over calling this directly: the origin has to advance
    /// by exactly the right amount between blocks, and that is the part a caller
    /// gets wrong.
    ///
    /// **Appends**; it does not clear. [`JrStream::stamp_span`] writes a due JR
    /// Clock ahead of the events and then calls this, so clearing here would
    /// drop it — the caller that owns the buffer is the one that clears it.
    ///
    /// Takes a buffer rather than returning a `Vec` because stamping is a
    /// per-block operation whose consumers hand the result straight to
    /// `MidiOut::queue` as a slice. Returning one allocated on every block for a
    /// value nobody keeps.
    pub fn stamp_block(&self, events: &[MidiEvent], origin_samples: u64, out: &mut Vec<MidiEvent>) {
        out.reserve(events.len() * 2);
        for ev in events {
            let ticks = self.clock.ticks_at(origin_samples + ev.frame_offset as u64);
            out.push(MidiEvent::jr_timestamp(ticks).with_frame_offset(ev.frame_offset));
            out.push(*ev);
        }
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

/// The spec's hard ceiling on the JR Clock interval: M2-104 §7.2.2.1 — "The
/// Sender **shall** send a JR Clock message at least once every 250
/// milliseconds."
pub const JR_CLOCK_MAX_INTERVAL: Duration = Duration::from_millis(250);

/// The interval [`JrClockEmitter::new`] actually uses.
///
/// Deliberately well inside the 250 ms ceiling. §7.2.2.1 ties the bound to the
/// 16-bit wrap — "to avoid ambiguity of the 2.09712 seconds wrap, and to provide
/// sufficient JR Clock messages for the Receiver" — and §7.2.2.1 also invites a
/// shorter period: "A Sender may send additional JR Clock messages with a
/// shorter period to help the Receiver analyze the jitter." Emitting at the
/// ceiling would leave no margin for a late block to push an interval past it.
pub const JR_CLOCK_INTERVAL: Duration = Duration::from_millis(100);

/// Emits JR Clock messages on a cadence (M2-104 §7.2.2.1).
///
/// This is what makes JR Timestamps mean anything. §7.2.2.3: a receiver that has
/// seen no JR Clock "shall render those messages as soon as possible" — i.e. it
/// discards the sender's timestamps entirely. A stream that stamps but never
/// clocks has done the work and gets none of the benefit.
///
/// Driven by sample position rather than wall time, so it stays in step with the
/// stream it clocks and is deterministic under test. The emitter is pure: ask it
/// [`due`](Self::due) for each block and it tells you whether one is owed.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JrClockEmitter {
    clock: JrClock,
    sample_rate: SampleRate,
    interval_samples: u64,
    /// Absolute sample position of the last emitted clock. `None` until the
    /// first, which is due immediately — a receiver needs a reference before it
    /// can use any stamp.
    last_emit: Option<u64>,
}

impl JrClockEmitter {
    /// An emitter at `sample_rate` Hz using [`JR_CLOCK_INTERVAL`].
    pub fn new(sample_rate: impl Into<SampleRate>) -> Self {
        Self::with_interval(sample_rate, JR_CLOCK_INTERVAL)
    }

    /// An emitter with an explicit interval.
    ///
    /// Panics if `interval` exceeds [`JR_CLOCK_MAX_INTERVAL`]: §7.2.2.1 makes
    /// that bound a `shall`, so a longer interval is not a tuning choice, it is
    /// a non-conformant stream. Catching it here beats shipping one.
    pub fn with_interval(sample_rate: impl Into<SampleRate>, interval: Duration) -> Self {
        assert!(
            interval <= JR_CLOCK_MAX_INTERVAL,
            "JR Clock interval {interval:?} exceeds the §7.2.2.1 maximum of {JR_CLOCK_MAX_INTERVAL:?}"
        );
        let sample_rate = sample_rate.into();
        Self {
            clock: JrClock::new(sample_rate),
            sample_rate,
            interval_samples: (interval.as_secs_f64() * sample_rate.get()).round() as u64,
            last_emit: None,
        }
    }

    /// The JR Clock owed at `origin_samples`, or `None` if one is not yet due.
    ///
    /// Call once per block with the block's starting sample position. The first
    /// call always yields a clock.
    pub fn due(&mut self, origin_samples: u64) -> Option<MidiEvent> {
        let owed = match self.last_emit {
            None => true,
            Some(last) => origin_samples.wrapping_sub(last) >= self.interval_samples,
        };
        if !owed {
            return None;
        }
        self.last_emit = Some(origin_samples);
        Some(MidiEvent::jr_clock(self.clock.ticks_at(origin_samples)))
    }

    /// The configured interval, in samples.
    pub fn interval_samples(&self) -> u64 {
        self.interval_samples
    }

    /// The sample rate this emitter clocks at.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
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
    /// The JR Clock cadence, when this stream clocks as well as stamps.
    emitter: Option<JrClockEmitter>,
}

impl JrStream {
    /// A stream stamping at `sample_rate` Hz, starting at origin 0.
    ///
    /// Stamps only. Use [`with_clock`](Self::with_clock) for a conformant
    /// sender: §7.2.2.3 makes a receiver ignore timestamps it has no clock for.
    pub fn new(sample_rate: impl Into<SampleRate>) -> Self {
        Self::with_stamper(JrStamper::new(sample_rate))
    }

    /// A stream over an existing stamper.
    pub fn with_stamper(stamper: JrStamper) -> Self {
        Self {
            stamper,
            origin_samples: 0,
            emitter: None,
        }
    }

    /// Add the JR Clock cadence (M2-104 §7.2.2.1) at [`JR_CLOCK_INTERVAL`].
    ///
    /// Once set, [`stamp_span`](Self::stamp_span) prefixes a clock to each block
    /// where one is due — including blocks with no events at all, which is why
    /// the cadence cannot ride on [`stamp`](Self::stamp) alone.
    pub fn with_clock(mut self, sample_rate: impl Into<SampleRate>) -> Self {
        self.emitter = Some(JrClockEmitter::new(sample_rate));
        self
    }

    /// Add the JR Clock cadence with an explicit interval. Panics above
    /// [`JR_CLOCK_MAX_INTERVAL`]; see [`JrClockEmitter::with_interval`].
    pub fn with_clock_interval(
        mut self,
        sample_rate: impl Into<SampleRate>,
        interval: Duration,
    ) -> Self {
        self.emitter = Some(JrClockEmitter::with_interval(sample_rate, interval));
        self
    }

    /// Stamp one block and advance the origin past it, so the next call
    /// continues monotonically.
    ///
    /// The origin advances by the *events'* span, which is zero for an empty
    /// block. That is fine for stamping — an empty block stamps nothing — but it
    /// means a clock cadence driven from here would freeze on a silent stream.
    /// Use [`stamp_span`](Self::stamp_span) when clocking.
    pub fn stamp(&mut self, events: &[MidiEvent], out: &mut Vec<MidiEvent>) {
        self.stamp_span(events, JrStamper::block_span(events), out);
    }

    /// Stamp one block of a known `block_samples` length, emitting a JR Clock
    /// first if the cadence owes one, and advance the origin by the *block*
    /// rather than by the events within it.
    ///
    /// This is the entry point a pump wants. §7.2.2.1 makes JR Clocks
    /// "independent … not related to any other message", so the cadence has to
    /// keep running through silence — and it only can if time advances on empty
    /// blocks, which the true block length provides and `block_span` does not.
    /// `out` is cleared first, so it is a destination and not an accumulator.
    /// Reusing one buffer across blocks is the point: this path runs once per
    /// block, and its callers only borrow the result to hand it to
    /// `MidiOut::queue`.
    pub fn stamp_span(
        &mut self,
        events: &[MidiEvent],
        block_samples: u64,
        out: &mut Vec<MidiEvent>,
    ) {
        out.clear();
        if let Some(emitter) = self.emitter.as_mut() {
            if let Some(clock) = emitter.due(self.origin_samples) {
                out.push(clock);
            }
        }
        self.stamper.stamp_block(events, self.origin_samples, out);
        self.origin_samples = self.origin_samples.wrapping_add(block_samples);
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
        // delay — ZERO while a clock is running, else None.
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
    use tutti_midi_types::{MidiChannel, MidiGroup};

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

    #[test]
    fn the_first_clock_is_due_immediately() {
        // A receiver cannot use any stamp until it has a clock reference, so
        // waiting one interval before the first would leave the opening block's
        // timing unusable.
        let mut e = JrClockEmitter::new(48_000.0);
        let first = e.due(0).expect("a clock is owed at once");
        assert_eq!(first.jr_clock_value(), Some(0));
    }

    #[test]
    fn clocks_are_emitted_on_the_configured_cadence() {
        let mut e = JrClockEmitter::with_interval(48_000.0, Duration::from_millis(100));
        let interval = e.interval_samples();
        assert_eq!(interval, 4_800);

        assert!(e.due(0).is_some(), "first");
        assert!(e.due(interval - 1).is_none(), "one sample short");
        assert!(e.due(interval).is_some(), "exactly due");
        assert!(e.due(interval + 1).is_none(), "just emitted");
        assert!(e.due(interval * 2).is_some(), "next period");
    }

    #[test]
    fn the_cadence_keeps_running_through_silence() {
        // The load-bearing case. §7.2.2.1: JR Clocks are "independent … not
        // related to any other message", and §7.2.2.3 makes a receiver discard
        // every stamp until it sees one. A cadence driven by outgoing traffic
        // would satisfy a test that sends notes and still emit nothing on an
        // idle stream — which is exactly when a receiver most needs the clock.
        let sample_rate = 48_000.0;
        let block = 512u64;
        let mut stream =
            JrStream::new(sample_rate).with_clock_interval(sample_rate, Duration::from_millis(100));

        let mut clocks = 0;
        // 1 second of entirely silent blocks.
        for _ in 0..(sample_rate as u64 / block) {
            let mut out = Vec::new();
            stream.stamp_span(&[], block, &mut out);
            clocks += out.iter().filter(|e| e.jr_clock_value().is_some()).count();
            assert!(
                out.iter().all(|e| e.jr_clock_value().is_some()),
                "a silent block carries clocks and nothing else"
            );
        }
        // 100 ms cadence over ~1 s: the first plus one per interval.
        assert!(
            (10..=11).contains(&clocks),
            "expected ~10 clocks across a silent second, got {clocks}"
        );
    }

    #[test]
    fn a_stamped_block_carries_its_clock_before_the_stamps() {
        let sample_rate = 48_000.0;
        let mut stream = JrStream::new(sample_rate).with_clock(sample_rate);
        let events = [
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(0),
        ];
        let mut out = Vec::new();
        stream.stamp_span(&events, 512, &mut out);
        // clock, then stamp, then the note.
        assert_eq!(out.len(), 3);
        assert!(out[0].jr_clock_value().is_some(), "clock leads");
        assert!(out[1].jr_timestamp_value().is_some(), "then the stamp");
        assert_eq!(out[2], events[0]);
    }

    #[test]
    fn a_stream_without_a_clock_emits_none() {
        // `new` alone stamps only — the cadence is opt-in, so existing
        // stamp-only callers keep their exact output.
        let mut stream = JrStream::new(48_000.0);
        let mut out = Vec::new();
        stream.stamp_span(&[], 512, &mut out);
        assert!(out.is_empty(), "no clock, no events, no output");
    }

    #[test]
    fn stamp_advances_by_events_and_stamp_span_by_the_block() {
        // The distinction the cadence depends on: `stamp` cannot move time on an
        // empty block, `stamp_span` can.
        let mut by_events = JrStream::new(48_000.0);
        by_events.stamp(&[], &mut Vec::new());
        assert_eq!(by_events.origin_samples(), 0, "silence stalls the origin");

        let mut by_block = JrStream::new(48_000.0);
        by_block.stamp_span(&[], 512, &mut Vec::new());
        assert_eq!(by_block.origin_samples(), 512, "the block advanced it");
    }

    #[test]
    #[should_panic(expected = "exceeds the §7.2.2.1 maximum")]
    fn an_interval_over_the_spec_maximum_is_rejected() {
        // §7.2.2.1's 250 ms is a `shall`, so a longer interval is not a tuning
        // choice — it is a non-conformant stream.
        JrClockEmitter::with_interval(48_000.0, Duration::from_millis(251));
    }

    #[test]
    fn the_default_interval_is_inside_the_spec_maximum() {
        assert!(JR_CLOCK_INTERVAL < JR_CLOCK_MAX_INTERVAL);
        assert_eq!(JR_CLOCK_MAX_INTERVAL, Duration::from_millis(250));
    }

    /// The span is one *past* the furthest offset, so the next block's origin
    /// does not re-stamp the sample the last event sat on.
    #[test]
    fn a_blocks_span_is_one_past_its_furthest_offset() {
        let events = [
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(0),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0).with_frame_offset(511),
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
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(0),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0)
                .with_frame_offset(24_000),
        ];

        let mut first = Vec::new();
        stream.stamp(&events, &mut first);
        assert_eq!(first[0].jr_timestamp_value(), Some(0));
        assert_eq!(stream.origin_samples(), 24_001);

        // The second block's first event is stamped from the new origin, not zero.
        let mut second = Vec::new();
        stream.stamp(&events, &mut second);
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
        let clock_block = [
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(511),
        ];
        let track_block = [
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 64, 0x8000)
                .with_frame_offset(511),
        ];

        let mut from_clock = Vec::new();
        wire.stamp(&clock_block, &mut from_clock);
        let mut from_track = Vec::new();
        wire.stamp(&track_block, &mut from_track);

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
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(0),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0)
                .with_frame_offset(24_000),
        ];
        let mut out = Vec::new();
        stamper.stamp_block(&events, 0, &mut out);
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
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(0),
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 64, 0x8000)
                .with_frame_offset(quarter_second),
        ];
        let mut stream = Vec::new();
        stamper.stamp_block(&events, 0, &mut stream);

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
        let diff = gap.abs_diff(expected);
        assert!(
            diff < Duration::from_secs_f64(JR_SECONDS_PER_TICK * 2.0),
            "recovered {gap:?} within one tick of {expected:?}"
        );
    }

    #[test]
    fn observe_before_any_timestamp_is_none() {
        let mut rx = JrReceiver::new();
        assert_eq!(
            rx.observe(&MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                0x8000
            )),
            None
        );
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
