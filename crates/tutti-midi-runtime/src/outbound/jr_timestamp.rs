//! MIDI 2.0 Jitter Reduction (JR) timestamps (M2-104 §2.1.4).
//!
//! JR timestamps let a receiver reconstruct the *original* spacing of a UMP
//! stream that a transport may have bunched up or spread out. The sender stamps
//! the stream with a monotonic 16-bit counter running at the JR reference clock
//! (31.25 kHz — one tick every 32 µs); the receiver reads the deltas between
//! successive stamps to recover inter-event intervals.
//!
//! The `Message::Utility(Utility::JrTimestamp(..))` of `tutti_ump` is the wire message. This
//! module is the *engine* around it:
//! - [`JrClock`] — the shared tick reference (samples ↔ 16-bit ticks).
//! - [`JrStamper`] — outbound: interleave a JR Timestamp before each event,
//!   derived from the event's sample-accurate [`Offset`]. Pure.
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
use tutti_graph::UmpAt;

use tutti_graph::Offset;
use tutti_types::{Frame, SampleRate, Samples};
use tutti_ump::{Message, Ump, Utility};

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

    /// The 16-bit JR tick a frame maps to (wrapping at 0x1_0000). A whole
    /// stream is stamped from one origin, so pass frames counted from it.
    #[inline]
    pub fn ticks_at(&self, frame: Frame) -> u16 {
        // ticks = frames * (JR_ticks/sec) / (frames/sec)
        let ticks = (frame.get() as f64) * JR_TICKS_PER_SECOND as f64 / self.sample_rate.get();
        (ticks as u64 & 0xFFFF) as u16
    }
}

/// Outbound JR stamping: turns a bare UMP stream into a JR-timestamped one by
/// prefixing each event with a JR Timestamp derived from the
/// event's [`Offset`]. Stateless across calls (each `stamp_block` is relative
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

    /// Writes each input event into `out`, preceded by a JR Timestamp for its
    /// [`Offset`]. `origin` is the absolute frame of this
    /// block's offset zero, so stamps stay monotonic across blocks.
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
    pub fn stamp_block(&self, events: &[UmpAt], origin: Frame, out: &mut Vec<UmpAt>) {
        out.reserve(events.len() * 2);
        for ev in events {
            let ticks = self.clock.ticks_at(origin + Samples(ev.0.get() as usize));
            out.push((ev.0, Message::Utility(Utility::JrTimestamp(ticks)).into()));
            out.push(*ev);
        }
    }

    /// How far the origin must advance after stamping `events` — one past the
    /// furthest frame offset in the block, or zero for an empty block.
    ///
    /// Split out from [`JrStream::stamp`] so the arithmetic that keeps stamps
    /// monotonic is stated once and testable on its own.
    #[inline]
    pub fn block_span(events: &[UmpAt]) -> Samples {
        events
            .iter()
            .map(|e| e.0.get() as usize)
            .max()
            .map_or(Samples::ZERO, |m| Samples(m + 1))
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
    interval: Samples,
    /// Absolute frame of the last emitted clock. `None` until the
    /// first, which is due immediately — a receiver needs a reference before it
    /// can use any stamp.
    last_emit: Option<Frame>,
}

impl JrClockEmitter {
    /// An emitter at `sample_rate` Hz using [`JR_CLOCK_INTERVAL`].
    pub fn new(sample_rate: impl Into<SampleRate>) -> Self {
        Self::with_interval(sample_rate, JR_CLOCK_INTERVAL)
    }

    /// An emitter with an explicit interval.
    ///
    /// # Panics
    ///
    /// Panics if `interval` exceeds [`JR_CLOCK_MAX_INTERVAL`]: M2-104 §7.2.2.1 makes
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
            interval: Samples((interval.as_secs_f64() * sample_rate.get()).round() as usize),
            last_emit: None,
        }
    }

    /// The JR Clock owed at `origin`, or `None` if one is not yet due.
    ///
    /// Call once per block with the block's starting frame. The first call
    /// always yields a clock, and so does one whose `origin` is before the last
    /// (the stream restarted).
    pub fn due(&mut self, origin: Frame) -> Option<Ump> {
        let owed = match self.last_emit {
            None => true,
            Some(last) => origin.since(last).is_none_or(|gap| gap >= self.interval),
        };
        if !owed {
            return None;
        }
        self.last_emit = Some(origin);
        Some(Message::Utility(Utility::JrClock(self.clock.ticks_at(origin))).into())
    }

    /// The configured interval, in frames.
    pub fn interval(&self) -> Samples {
        self.interval
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
    /// Absolute frame of the next block's offset zero.
    origin: Frame,
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
    #[must_use]
    pub fn with_stamper(stamper: JrStamper) -> Self {
        Self {
            stamper,
            origin: Frame::ZERO,
            emitter: None,
        }
    }

    /// Adds the JR Clock cadence (M2-104 §7.2.2.1) at [`JR_CLOCK_INTERVAL`].
    ///
    /// Once set, [`stamp_span`](Self::stamp_span) prefixes a clock to each block
    /// where one is due — including blocks with no events at all, which is why
    /// the cadence cannot ride on [`stamp`](Self::stamp) alone.
    #[must_use]
    pub fn with_clock(mut self, sample_rate: impl Into<SampleRate>) -> Self {
        self.emitter = Some(JrClockEmitter::new(sample_rate));
        self
    }

    /// Adds the JR Clock cadence with an explicit interval.
    ///
    /// # Panics
    ///
    /// Panics if `interval` is above [`JR_CLOCK_MAX_INTERVAL`]; see
    /// [`JrClockEmitter::with_interval`].
    #[must_use]
    pub fn with_clock_interval(
        mut self,
        sample_rate: impl Into<SampleRate>,
        interval: Duration,
    ) -> Self {
        self.emitter = Some(JrClockEmitter::with_interval(sample_rate, interval));
        self
    }

    /// Stamps one block and advances the origin past it, so the next call
    /// continues monotonically.
    ///
    /// The origin advances by the *events'* span, which is zero for an empty
    /// block. That is fine for stamping — an empty block stamps nothing — but it
    /// means a clock cadence driven from here would freeze on a silent stream.
    /// Use [`stamp_span`](Self::stamp_span) when clocking.
    pub fn stamp(&mut self, events: &[UmpAt], out: &mut Vec<UmpAt>) {
        self.stamp_span(events, JrStamper::block_span(events), out);
    }

    /// Stamps one block of a known `block` length, emitting a JR Clock
    /// first if the cadence owes one, and advances the origin by the *block*
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
    pub fn stamp_span(&mut self, events: &[UmpAt], block: Samples, out: &mut Vec<UmpAt>) {
        out.clear();
        if let Some(emitter) = self.emitter.as_mut()
            && let Some(clock) = emitter.due(self.origin)
        {
            out.push((Offset::ZERO, clock));
        }
        self.stamper.stamp_block(events, self.origin, out);
        self.origin += block;
    }

    /// The absolute frame the next [`stamp`](Self::stamp) will start from.
    /// Exposed for tests and diagnostics.
    pub fn origin(&self) -> Frame {
        self.origin
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
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one event. On a JR Timestamp, records it and returns `None` (its delay
    /// applies to what follows). On any other event, returns the reconstructed
    /// delay since the previous timestamp — `Some(Duration::ZERO)` when two events
    /// share a timestamp, `None` before any timestamp has been seen.
    pub fn observe(&mut self, event: &Ump) -> Option<Duration> {
        if let Ok(Message::Utility(Utility::JrTimestamp(ticks))) = (*event).decode() {
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
mod tests;
