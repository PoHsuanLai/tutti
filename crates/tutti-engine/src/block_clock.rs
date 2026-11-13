//! The block clock: where the OS clock and the graph's frame clock stood at
//! one render call's start, published by the engine to threads that hold an OS
//! time and need a frame (doc 017 section 5, "Timing at the edge").
//!
//! The engine writes one reading per render call and any number of producer
//! threads read the latest ([`BlockClockReader::read`]). The cell is a
//! [`SeqWriter`]: no allocation, no lock, and a read is the value of one
//! write, never the bytes of two.
//!
//! It carries the transport of the call's first frame too ([`Playhead`]).
//!
//! The OS time is [`tutti_rt::monotonic_ns`], the OS's own monotonic clock in
//! nanoseconds, not an `Instant`: it is on the OS's epoch, so a plugin server
//! or another process reads the same timebase. The reader hands back plain
//! values; whoever holds arrival stamps in another representation converts
//! them (the session maps the nanoseconds onto the `Instant`s the MIDI
//! backends stamp with).

use bytemuck::{Pod, Zeroable};
use tutti_rt::{SeqReader, SeqWriter};
use tutti_types::{Beat, BeatDuration, Bpm, Frame, SampleRate, Samples, Seconds};

/// How many times a read retries a copy a write overlapped before it answers
/// "nothing": a write is a few stores, so a reader that loses this many was
/// descheduled.
const RETRIES: usize = 1_000;

#[derive(Clone, Copy, Debug, Pod, Zeroable)]
#[repr(C)]
struct Wire {
    nanos: i64,
    frame: u64,
    rate: f64,
    beat: f64,
    seconds: f64,
    tempo: f64,
    span: u64,
    playing: u64,
}

/// One render call's start: the OS monotonic time, the graph frame it began at
/// and the rate that frame is counted at.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BlockReading {
    /// [`tutti_rt::monotonic_ns`] at the call's start.
    pub os_ns: i64,
    /// The graph's frame at the call's start.
    pub frame: Frame,
    /// The rate `frame` is counted at.
    pub rate: SampleRate,
}

/// The transport as one render call's first frame found it: what the engine
/// hands [`BlockClock::write`] beside the call's own clock reading.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pose {
    pub(crate) beat: Beat,
    pub(crate) seconds: Seconds,
    pub(crate) tempo: Bpm,
    pub(crate) playing: bool,
    /// The call's length in frames.
    pub(crate) span: Samples,
}

/// The transport at the start of one render call, read in one piece: where
/// the playhead stood, whether it was rolling, and the clock that says when.
///
/// The audio thread writes it once per render call through a sequence lock
/// (the block clock's cell), so a read is the value of one write: `beat`,
/// `frame`, `seconds` and `playing` are the same instant, which two atomics
/// read one after the other cannot promise. It describes the call's first
/// frame, after the transport commands due on that frame: a command that
/// lands later in the call shows in the next snapshot.
///
/// The fields are private because `rate`, `tempo` and `span` carry an
/// invariant ([`beat_now`](Self::beat_now) divides and clamps by them), and
/// an engine is the only writer of a real one; the getters are the plain
/// values.
///
/// ```
/// use tutti_engine::Playhead;
/// use tutti_types::Beat;
/// let resting = Playhead::default();
/// assert!(!resting.playing());
/// assert_eq!(resting.at(), None, "no call has rendered");
/// assert_eq!(resting.beat_now(123), Beat(0.0));
/// ```
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Playhead {
    beat: Beat,
    frame: Frame,
    seconds: Seconds,
    playing: bool,
    tempo: Bpm,
    rate: SampleRate,
    span: Samples,
    at: Option<i64>,
}

impl Default for Playhead {
    /// What a transport reads before any render call: stopped at the origin,
    /// at the default tempo, with no clock reading ([`at`](Self::at) is
    /// `None`).
    fn default() -> Self {
        Self {
            beat: Beat(0.0),
            frame: Frame::ZERO,
            seconds: Seconds(0.0),
            playing: false,
            tempo: Bpm::default(),
            rate: SampleRate::SR_48K,
            span: Samples(0),
            at: None,
        }
    }
}

impl Playhead {
    /// The playhead's beat at the call's first frame. May be negative during
    /// a count-in.
    #[must_use]
    pub fn beat(&self) -> Beat {
        self.beat
    }

    /// The graph's absolute frame the call began at (the executor's clock,
    /// which a locate does not move).
    #[must_use]
    pub fn frame(&self) -> Frame {
        self.frame
    }

    /// The rate [`frame`](Self::frame) is counted at.
    #[must_use]
    pub fn rate(&self) -> SampleRate {
        self.rate
    }

    /// Transport time: the seconds of the project's tempo map that
    /// [`beat`](Self::beat) stands at, not the time since the first frame.
    #[must_use]
    pub fn seconds(&self) -> Seconds {
        self.seconds
    }

    /// Whether the transport was rolling.
    #[must_use]
    pub fn playing(&self) -> bool {
        self.playing
    }

    /// The call's length in frames: how far the transport advances from
    /// [`seconds`](Self::seconds) to the next call's first frame while it
    /// [`playing`](Self::playing). What a control-side edit that lands at the
    /// next block places itself by.
    #[must_use]
    pub fn span(&self) -> Samples {
        self.span
    }

    /// The tempo in force at the call's first frame: the rate
    /// [`beat_now`](Self::beat_now) extrapolates at.
    #[must_use]
    pub fn tempo(&self) -> Bpm {
        self.tempo
    }

    /// [`tutti_rt::monotonic_ns`] at the call's start: the OS clock reading
    /// [`beat_now`](Self::beat_now) measures from, on the epoch the block
    /// clock and a plugin server share. `None` before any render call.
    #[must_use]
    pub fn at(&self) -> Option<i64> {
        self.at
    }

    /// The playhead's beat at `now`, a [`tutti_rt::monotonic_ns`] reading,
    /// extrapolated from the snapshot: the call's beat plus the tempo's beats
    /// over the time since [`at`](Self::at), so a UI that reads it at 60 Hz
    /// sees the playhead move between blocks instead of in block steps.
    ///
    /// It holds the snapshot's beat when the transport is stopped, when no
    /// call has rendered, and for a `now` before the snapshot. The elapsed
    /// time is **clamped to the call's own length** (one block at the device's
    /// size): the next call's snapshot replaces this one by then, and an
    /// audio thread that stalls leaves the playhead one block past where it
    /// was last heard, never running away.
    ///
    /// The extrapolation is linear at the tempo in force at the call's first
    /// frame. Through a tempo ramp the playhead's true path curves, and the
    /// line departs from it by at most the ramp's change over one block; a
    /// loop wrap or a locate inside the call shows with the next snapshot.
    #[must_use]
    pub fn beat_now(&self, now: i64) -> Beat {
        let Some(at) = self.at else {
            return self.beat;
        };
        if !self.playing {
            return self.beat;
        }
        let bound_ns = self.span.to_seconds(self.rate).get() * 1e9;
        let elapsed_ns = now.saturating_sub(at).max(0) as f64;
        let elapsed = Seconds(elapsed_ns.min(bound_ns) * 1e-9);
        // Beats per second is the tempo over sixty: the one place a time
        // becomes a beat span here, in `f64` as the beat is.
        self.beat + BeatDuration(elapsed.get() * self.tempo.get() / 60.0)
    }
}

/// The writing end: the engine's. One writer.
#[derive(Debug)]
pub(crate) struct BlockClock {
    cell: SeqWriter<Wire>,
}

impl BlockClock {
    pub(crate) fn new() -> Self {
        Self {
            cell: SeqWriter::new(),
        }
    }

    pub(crate) fn reader(&self) -> BlockClockReader {
        BlockClockReader {
            cell: self.cell.reader(),
        }
    }

    /// Publishes that the call beginning at graph frame `frame` (counted at
    /// `rate`) began at `os_ns`. Audio thread: a few stores.
    pub(crate) fn write(&mut self, os_ns: i64, frame: Frame, rate: SampleRate, pose: Pose) {
        self.cell.write(Wire {
            nanos: os_ns,
            frame: frame.get(),
            rate: rate.get(),
            beat: pose.beat.get(),
            seconds: pose.seconds.get(),
            tempo: pose.tempo.get(),
            span: pose.span.get() as u64,
            playing: u64::from(pose.playing),
        });
    }
}

/// A producer thread's end. `Clone`, and `read` takes `&self`.
#[derive(Clone, Debug)]
pub struct BlockClockReader {
    cell: SeqReader<Wire>,
}

impl BlockClockReader {
    /// The latest reading; `None` before the first render call, or when every
    /// copy overlapped a write. Lock-free and allocation-free.
    #[must_use]
    pub fn read(&self) -> Option<BlockReading> {
        let wire = self.cell.read(RETRIES).ok().flatten()?;
        Some(BlockReading {
            os_ns: wire.nanos,
            frame: Frame::new(wire.frame),
            rate: SampleRate::new(wire.rate)?,
        })
    }

    /// The transport of the latest call, in one read: the same write
    /// [`read`](Self::read) answers from. Before the first render call, or
    /// when every copy overlapped a write, it is the resting
    /// [`Playhead::default`], with no clock reading.
    #[must_use]
    pub fn playhead(&self) -> Playhead {
        let Some(wire) = self.cell.read(RETRIES).ok().flatten() else {
            return Playhead::default();
        };
        let Some(rate) = SampleRate::new(wire.rate) else {
            return Playhead::default();
        };
        Playhead {
            beat: Beat(wire.beat),
            frame: Frame::new(wire.frame),
            seconds: Seconds(wire.seconds),
            playing: wire.playing != 0,
            tempo: Bpm::new(wire.tempo).unwrap_or_default(),
            rate,
            span: Samples(wire.span as usize),
            at: Some(wire.nanos),
        }
    }
}

#[cfg(test)]
mod tests;
