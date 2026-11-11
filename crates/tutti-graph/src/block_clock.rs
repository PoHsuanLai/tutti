//! The block clock: where the OS clock and the graph's frame clock stood at
//! one block's start, published by the engine to threads that hold an OS time
//! and need a frame (doc 017 section 5, "Timing at the edge").
//!
//! The engine writes one reading per render call ([`BlockClock::write`]) and
//! any number of producer threads read the latest ([`BlockClockReader::read`]):
//! a hardware MIDI thread feeds each reading to a `ClockMap`, which places an
//! arrival on a frame. The cell is a [`SeqWriter`], the audio thread's way to
//! publish a value it overwrites each block: no allocation, no lock, and a
//! read is the value of one write, never the bytes of two.
//!
//! An [`Instant`] is opaque and process-local, so the cell holds the OS time
//! as nanoseconds after an epoch the pair shares ([`BlockClock::new`]).

use std::time::{Duration, Instant};

use bytemuck::{Pod, Zeroable};
use tutti_rt::{SeqReader, SeqWriter};
use tutti_types::{Frame, Moment, SampleRate};

/// How many times a read retries a copy a write overlapped before it answers
/// "nothing": a write is a few stores, so a reader that loses this many was
/// descheduled.
const RETRIES: usize = 1_000;

#[derive(Clone, Copy, Debug, Pod, Zeroable)]
#[repr(C)]
struct Wire {
    /// The block's start, in nanoseconds after the pair's epoch.
    nanos: u64,
    frame: u64,
    rate: f64,
}

/// The writing end of the block clock: the engine's. One writer; see the
/// module docs.
#[derive(Debug)]
pub struct BlockClock {
    epoch: Instant,
    cell: SeqWriter<Wire>,
}

impl BlockClock {
    /// A clock with nothing written, its epoch now. Control side.
    #[must_use]
    pub fn new() -> Self {
        Self {
            epoch: Instant::now(),
            cell: SeqWriter::new(),
        }
    }

    /// A reader of this clock, for a producer thread. Clone it for more.
    #[must_use]
    pub fn reader(&self) -> BlockClockReader {
        BlockClockReader {
            epoch: self.epoch,
            cell: self.cell.reader(),
        }
    }

    /// Publishes that the block beginning at graph frame `frame` (counted at
    /// `rate`) began at `os`. Audio thread: a few stores. An `os` before the
    /// epoch reads as the epoch.
    pub fn write(&mut self, os: Instant, frame: Frame, rate: SampleRate) {
        let nanos = os.saturating_duration_since(self.epoch).as_nanos();
        self.cell.write(Wire {
            nanos: u64::try_from(nanos).unwrap_or(u64::MAX),
            frame: frame.get(),
            rate: rate.get(),
        });
    }
}

impl Default for BlockClock {
    fn default() -> Self {
        Self::new()
    }
}

/// A producer thread's end of the block clock. `Clone`, and `read` takes
/// `&self`: it keeps nothing between reads.
#[derive(Clone, Debug)]
pub struct BlockClockReader {
    epoch: Instant,
    cell: SeqReader<Wire>,
}

impl BlockClockReader {
    /// The latest reading: the OS time a block began, and the graph frame it
    /// began at with the rate that frame is counted at. `None` before the
    /// first block, or when every copy overlapped a write. Lock-free and
    /// allocation-free.
    #[must_use]
    pub fn read(&self) -> Option<(Instant, Moment)> {
        let wire = self.cell.read(RETRIES).ok().flatten()?;
        let os = self.epoch.checked_add(Duration::from_nanos(wire.nanos))?;
        Some((os, Moment::new(Frame(wire.frame), SampleRate(wire.rate))))
    }
}

#[cfg(test)]
mod tests;
