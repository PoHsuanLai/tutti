//! Level meters: declared readers on ports of the compiled plan.
//!
//! A meter is declared on the [`Topology`](tutti_types::Topology) as "measure
//! these sources" ([`MeterSpec`](tutti_types::MeterSpec)): the global outputs
//! for a master meter (`Outputs`, aligned: each lane is what its output
//! channel carries, at the output's time), any node's output ports for a track
//! (`Ports { sources, align }`, as they leave their nodes or aligned to the
//! output time). The compiler emits one [`Op::Meter`](crate::Op::Meter) per
//! meter, which reads every lane's source once per block and folds it into
//! that lane's cell. A lane whose port does not exist reads silence. A meter
//! that is not in the topology compiles to nothing, and a meter never changes
//! an output sample.
//!
//! # What a lane holds
//!
//! A lane is a [`tutti_rt::level_cell`]: the peak, the frame count and the
//! energy (a sum of squares, an `f64`, so an hour unread stays exact) since
//! the reader last read. The op folds each block into it and a read takes it
//! whole, so **a block lands in one read, peak and energy together**: a read
//! with no frames has no peak and no energy. The cell is `tutti-rt`'s, with a
//! loom model; the two ends are wait-free for the op and wait for a block in
//! flight on the reader's side.
//!
//! The audio side keeps no state of its own and no clock: **ballistics**
//! (decay, peak-hold timers, dB scaling) belong to the reader, which knows
//! its own refresh rate. An offline render runs faster than any clock, so a
//! decay on the audio side would mean nothing there.
//!
//! # One consumer
//!
//! [`MeterReader::read`] takes what was held, so two consumers of one reader
//! would steal each other's peaks. The reader is therefore not `Clone` and
//! reads through `&mut self`; a second consumer declares a second meter.
//!
//! # Parallel safety
//!
//! The op writes only the cells' atomics, and a lane has exactly one writer by
//! construction (lane `i` is source `i` of the meter's one op), so the cells
//! need no claim: the serial and parallel executors run the same
//! [`meter_op`] and leave the same bits.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Weak};

use tutti_rt::{LevelReader, LevelSpan, LevelWriter, RtScope, level_cell};
use tutti_types::{Amplitude, MeterId, Samples};

use crate::exec::SILENT;
use crate::slots::{AudioSlots, SlotFlags};

/// What one lane measured since it was last read.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Level {
    /// The largest absolute sample.
    pub peak: Amplitude,
    /// The root-mean-square over [`frames`](Self::frames). A non-finite sample
    /// makes it non-finite for that read (a peak ignores NaN).
    pub rms: Amplitude,
    /// How many frames this covers. Zero: no block was measured since the
    /// last read (the meter is off, or the reader is faster than the blocks),
    /// and the figures above are silence, not a measurement.
    pub frames: Samples,
}

impl From<LevelSpan> for Level {
    fn from(span: LevelSpan) -> Self {
        let rms = if span.frames == 0 {
            0.0
        } else {
            (span.sum_sq / span.frames as f64).sqrt() as f32
        };
        Self {
            peak: Amplitude(span.peak),
            rms: Amplitude(rms),
            frames: Samples(usize::try_from(span.frames).unwrap_or(usize::MAX)),
        }
    }
}

/// What the two ends share besides the lanes.
#[derive(Debug)]
struct Shared {
    /// The lanes the current plan writes (at most the lanes there are).
    width: AtomicU32,
    enabled: AtomicBool,
}

/// One block's figures for one lane.
#[derive(Clone, Copy)]
struct BlockStats {
    peak: f32,
    sum_sq: f32,
}

/// Samples a pass folds at once. The partial sums are a fixed shape, so the
/// result is the same on every executor and the loop vectorizes (a plain
/// left-to-right `f32` sum cannot).
const WIDTH: usize = 8;

/// Peak and sum of squares of `block`: `WIDTH` running figures per pass,
/// combined in lane order at the end, then the tail in order. NaN is ignored
/// by the peak (`f32::max`) and poisons the sum.
#[inline]
fn block_stats(block: &[f32]) -> BlockStats {
    let (chunks, tail) = block.as_chunks::<WIDTH>();
    let mut peak = [0.0f32; WIDTH];
    let mut sum = [0.0f32; WIDTH];
    for chunk in chunks {
        for i in 0..WIDTH {
            peak[i] = peak[i].max(chunk[i].abs());
            sum[i] += chunk[i] * chunk[i];
        }
    }
    let mut stats = BlockStats {
        peak: peak.iter().fold(0.0, |a, &p| a.max(p)),
        sum_sq: sum.iter().sum(),
    };
    for &s in tail {
        stats.peak = stats.peak.max(s.abs());
        stats.sum_sq += s * s;
    }
    stats
}

/// The executor's end of a meter: a writer per lane and the switch. Not
/// `Clone`; one per meter.
#[derive(Debug)]
pub struct MeterWriter {
    lanes: Box<[LevelWriter]>,
    shared: Arc<Shared>,
}

impl Drop for MeterWriter {
    fn drop(&mut self) {
        RtScope::check_outside("a meter writer");
    }
}

/// [`Op::Meter`](crate::Op::Meter): the first `frames` samples of each of
/// `srcs` into the meter's lanes, if the reader switched it on. A slot the
/// executor knows is silent is not scanned: it adds its frames and no energy.
#[inline]
pub(crate) fn meter_op<A: AudioSlots + ?Sized, F: SlotFlags + ?Sized>(
    meter: &MeterWriter,
    arena: &A,
    flags: &F,
    srcs: &[u32],
    frames: usize,
) {
    if !meter.shared.enabled.load(Ordering::Relaxed) {
        return;
    }
    // A block is at most `MaxBlock` frames, far below `u32::MAX`; a longer one
    // (unreachable) is not counted.
    let Ok(n) = u32::try_from(frames) else {
        return;
    };
    for (lane, &src) in meter.lanes.iter().zip(srcs) {
        let stats = if flags.get(src) & SILENT != 0 {
            BlockStats {
                peak: 0.0,
                sum_sq: 0.0,
            }
        } else {
            block_stats(arena.slot(src, frames))
        };
        lane.fold(stats.peak, f64::from(stats.sum_sq), n);
    }
}

impl MeterWriter {
    /// Records how many lanes the plan being applied writes. Control or
    /// audio thread: one store.
    pub(crate) fn set_width(&self, lanes: usize) {
        let lanes = lanes.min(self.lanes.len());
        self.shared
            .width
            .store(u32::try_from(lanes).unwrap_or(u32::MAX), Ordering::Relaxed);
    }
}

/// The consumer's end of a meter: the level of each lane since the last read.
///
/// Not `Clone`, and reads take `&mut self`: one consumer per meter (see the
/// module docs). Switched **off** until [`enable`](Self::enable): the
/// op then skips its pass entirely. Dropping it leaves the meter in the plan,
/// measuring nothing anyone reads, until the editor closes or reaps it
/// ([`Editor::close_meter`](crate::Editor::close_meter),
/// [`Editor::reap_meters`](crate::Editor::reap_meters)). It must not be
/// dropped on the audio thread in a debug build.
#[derive(Debug)]
pub struct MeterReader {
    id: MeterId,
    lanes: Box<[LevelReader]>,
    shared: Arc<Shared>,
    /// The editor holds a `Weak` of this: when it dies, the reader is gone.
    alive: Arc<()>,
}

impl MeterReader {
    /// A reader and the writer the executor will fill it with, for `lanes`
    /// channels.
    pub(crate) fn pair(id: MeterId, lanes: usize, width: usize) -> (Self, MeterWriter) {
        let shared = Arc::new(Shared {
            width: AtomicU32::new(u32::try_from(width).unwrap_or(u32::MAX)),
            enabled: AtomicBool::new(false),
        });
        let (writers, readers): (Vec<_>, Vec<_>) = (0..lanes).map(|_| level_cell()).unzip();
        (
            Self {
                id,
                lanes: readers.into(),
                shared: Arc::clone(&shared),
                alive: Arc::new(()),
            },
            MeterWriter {
                lanes: writers.into(),
                shared,
            },
        )
    }

    /// A handle to see whether this reader is still alive: its strong count
    /// is zero once the reader is dropped.
    #[must_use]
    pub fn watch(&self) -> Weak<()> {
        Arc::downgrade(&self.alive)
    }

    /// The meter's id, the one [`Editor::close_meter`](crate::Editor::close_meter) takes.
    #[must_use]
    pub fn id(&self) -> MeterId {
        self.id
    }

    /// How many channels the meter measures now: its sources' count, as of
    /// the plan the executor runs (zero before the first commit that names
    /// it is applied).
    #[must_use]
    pub fn channels(&self) -> usize {
        self.shared.width.load(Ordering::Relaxed) as usize
    }

    /// Starts measuring: the op begins on the next block.
    pub fn enable(&self) {
        self.shared.enabled.store(true, Ordering::Relaxed);
    }

    /// Stops measuring, skipping the pass entirely. What was held stays
    /// readable.
    pub fn disable(&self) {
        self.shared.enabled.store(false, Ordering::Relaxed);
    }

    /// Whether the op is filling this meter.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.shared.enabled.load(Ordering::Relaxed)
    }

    /// Whether the meter's writer is gone: the meter was closed and the
    /// plan that ran it retired, so nothing will be measured again.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        Arc::strong_count(&self.shared) == 1
    }

    /// Takes what each channel measured since the last read into `out`, one
    /// [`Level`] per channel, and returns how many it wrote (the fewer of
    /// [`channels`](Self::channels) and `out.len()`). Takes only the lanes it
    /// writes. Allocates nothing; any thread that owns the reader.
    pub fn read_into(&mut self, out: &mut [Level]) -> usize {
        let width = self.channels().min(self.lanes.len());
        let mut n = 0;
        for (slot, lane) in out.iter_mut().zip(&mut self.lanes[..width]) {
            *slot = lane.take().into();
            n += 1;
        }
        n
    }

    /// As [`read_into`](Self::read_into), into a new `Vec`.
    #[must_use]
    pub fn read(&mut self) -> Vec<Level> {
        let mut out = vec![Level::default(); self.channels()];
        let n = self.read_into(&mut out);
        out.truncate(n);
        out
    }
}

impl Drop for MeterReader {
    fn drop(&mut self) {
        RtScope::check_outside("a meter reader");
    }
}

#[cfg(test)]
mod tests;
