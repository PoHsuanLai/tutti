//! The block clock: where the OS clock and the graph's frame clock stood at
//! one render call's start, published by the engine to threads that hold an OS
//! time and need a frame (doc 017 section 5, "Timing at the edge").
//!
//! The engine writes one reading per render call and any number of producer
//! threads read the latest ([`BlockClockReader::read`]). The cell is a
//! [`SeqWriter`]: no allocation, no lock, and a read is the value of one
//! write, never the bytes of two.
//!
//! The OS time is [`tutti_rt::monotonic_ns`], the OS's own monotonic clock in
//! nanoseconds, not an `Instant`: it is on the OS's epoch, so a plugin server
//! or another process reads the same timebase. The reader hands back plain
//! values; whoever holds arrival stamps in another representation converts
//! them (the session maps the nanoseconds onto the `Instant`s the MIDI
//! backends stamp with).

use bytemuck::{Pod, Zeroable};
use tutti_rt::{SeqReader, SeqWriter};
use tutti_types::{Frame, SampleRate};

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
    pub(crate) fn write(&mut self, os_ns: i64, frame: Frame, rate: SampleRate) {
        self.cell.write(Wire {
            nanos: os_ns,
            frame: frame.get(),
            rate: rate.get(),
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
            frame: Frame(wire.frame),
            rate: SampleRate(wire.rate),
        })
    }
}

#[cfg(test)]
mod tests;
