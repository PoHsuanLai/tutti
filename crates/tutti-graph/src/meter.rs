//! Level meters: declared readers on ports of the compiled plan.
//!
//! A meter is declared on the [`Topology`](tutti_types::Topology) as "measure
//! these sources" ([`MeterSpec`](tutti_types::MeterSpec)): the global outputs
//! for a master meter, any node's output ports for a track. The compiler emits
//! one [`Op::Meter`](crate::Op::Meter) per meter, which reads every lane's
//! source once per block and folds it into that lane's cells. A meter that is
//! not in the topology compiles to nothing.
//!
//! # What a lane holds
//!
//! Two atomic words, written by the op and taken by the reader:
//!
//! - the **peak** since the reader last read, as the bits of a non-negative
//!   `f32` (which order like the numbers), raised with `fetch_max`;
//! - the **energy** since the reader last read: the frames measured and their
//!   sum of squares, packed in one word so a read takes both together, added
//!   with a compare-exchange loop.
//!
//! A read swaps both to zero. Every update and the swap are read-modify-writes
//! of one location, so each block lands either in this read or the next and
//! none is lost or counted twice. The words are independent of each other and
//! of the switch, so no store-load pair needs a fence and every access is
//! `Relaxed`. Nothing grows while nobody reads: a peak is a maximum, and the
//! frame count saturates (a block that would overflow it is not added).
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
//! The op writes only atomics, and a lane has exactly one writer by
//! construction (lane `i` is source `i` of the meter's one op), so the cells
//! need no claim: the serial and parallel executors run the same
//! [`meter_op`] and leave the same bits.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use tutti_rt::RtScope;
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

/// One channel's cells.
#[derive(Debug, Default)]
struct Lane {
    peak: AtomicU32,
    /// Frames in the high half, the sum of squares (an `f32`) in the low.
    energy: AtomicU64,
}

const FRAMES_SHIFT: u32 = 32;

fn pack(frames: u32, sum_sq: f32) -> u64 {
    (u64::from(frames) << FRAMES_SHIFT) | u64::from(sum_sq.to_bits())
}

fn unpack(word: u64) -> (u32, f32) {
    // The low half is the bits of an `f32`, the high the count: both halves
    // fit by construction.
    ((word >> FRAMES_SHIFT) as u32, f32::from_bits(word as u32))
}

impl Lane {
    /// Folds one block's figures in.
    #[inline]
    fn add(&self, block: BlockStats, frames: u32) {
        self.peak.fetch_max(block.peak.to_bits(), Ordering::Relaxed);
        let mut cur = self.energy.load(Ordering::Relaxed);
        loop {
            let (n, sum) = unpack(cur);
            let Some(n) = n.checked_add(frames) else {
                return;
            };
            match self.energy.compare_exchange_weak(
                cur,
                pack(n, sum + block.sum_sq),
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(seen) => cur = seen,
            }
        }
    }

    fn take(&self) -> Level {
        let peak = f32::from_bits(self.peak.swap(0, Ordering::Relaxed));
        let (frames, sum) = unpack(self.energy.swap(0, Ordering::Relaxed));
        Level {
            peak: Amplitude(peak),
            rms: Amplitude(if frames == 0 {
                0.0
            } else {
                (sum / frames as f32).sqrt()
            }),
            frames: Samples(frames as usize),
        }
    }
}

/// The cells the two ends share.
#[derive(Debug)]
struct Cells {
    lanes: Box<[Lane]>,
    /// The lanes the current plan writes (at most `lanes.len()`).
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

/// The executor's end of a meter: shared cells it writes from the op. Not
/// `Clone`; one per meter.
#[derive(Debug)]
pub struct MeterWriter {
    cells: Arc<Cells>,
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
    let cells = &*meter.cells;
    if !cells.enabled.load(Ordering::Relaxed) {
        return;
    }
    // A block is at most `MaxBlock` frames, far below `u32::MAX`; a longer one
    // (unreachable) is not counted.
    let Ok(n) = u32::try_from(frames) else {
        return;
    };
    for (lane, &src) in cells.lanes.iter().zip(srcs) {
        let stats = if flags.get(src) & SILENT != 0 {
            BlockStats {
                peak: 0.0,
                sum_sq: 0.0,
            }
        } else {
            block_stats(arena.slot(src, frames))
        };
        lane.add(stats, n);
    }
}

impl MeterWriter {
    /// Records how many lanes the plan being applied writes. Control or
    /// audio thread: one store.
    pub(crate) fn set_width(&self, lanes: usize) {
        let lanes = lanes.min(self.cells.lanes.len());
        self.cells
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
    cells: Arc<Cells>,
    /// The editor holds a `Weak` of this: when it dies, the reader is gone.
    alive: Arc<()>,
}

impl MeterReader {
    /// A reader and the writer the executor will fill it with, for `lanes`
    /// channels.
    pub(crate) fn pair(id: MeterId, lanes: usize, width: usize) -> (Self, MeterWriter) {
        let cells = Arc::new(Cells {
            lanes: (0..lanes).map(|_| Lane::default()).collect(),
            width: AtomicU32::new(u32::try_from(width).unwrap_or(u32::MAX)),
            enabled: AtomicBool::new(false),
        });
        (
            Self {
                id,
                cells: Arc::clone(&cells),
                alive: Arc::new(()),
            },
            MeterWriter { cells },
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
        self.cells.width.load(Ordering::Relaxed) as usize
    }

    /// Starts measuring: the op begins on the next block.
    pub fn enable(&self) {
        self.cells.enabled.store(true, Ordering::Relaxed);
    }

    /// Stops measuring, skipping the pass entirely. What was held stays
    /// readable.
    pub fn disable(&self) {
        self.cells.enabled.store(false, Ordering::Relaxed);
    }

    /// Whether the op is filling this meter.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.cells.enabled.load(Ordering::Relaxed)
    }

    /// Whether the meter's writer is gone: the meter was closed and the
    /// plan that ran it retired, so nothing will be measured again.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        Arc::strong_count(&self.cells) == 1
    }

    /// Takes what each channel measured since the last read into `out`, one
    /// [`Level`] per channel, and returns how many it wrote (the fewer of
    /// [`channels`](Self::channels) and `out.len()`). Takes only the lanes it
    /// writes. Allocates nothing; any thread that owns the reader.
    pub fn read_into(&mut self, out: &mut [Level]) -> usize {
        let lanes = &self.cells.lanes[..self.channels().min(self.cells.lanes.len())];
        let n = lanes.len().min(out.len());
        for (slot, lane) in out.iter_mut().zip(lanes) {
            *slot = lane.take();
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
mod tests {
    use super::*;

    /// Constant `v` over `frames`, for `lane`.
    fn feed(lane: &Lane, v: f32, frames: usize) {
        lane.add(block_stats(&vec![v; frames]), frames as u32);
    }

    /// A constant block reads its analytic peak and RMS, exactly.
    ///
    /// Mutation (run, recorded here): `block_stats` summing `s` instead of
    /// `s * s` -> the RMS reads 0.5 where 0.5 * 0.5 = 0.25 is summed, and this
    /// fails.
    #[test]
    fn a_constant_reads_exactly() {
        let lane = Lane::default();
        feed(&lane, -0.5, 100);
        let l = lane.take();
        assert_eq!(l.peak.get(), 0.5);
        assert_eq!(l.rms.get(), 0.5);
        assert_eq!(l.frames, Samples(100));
    }

    /// The statistics of a block of any length agree with a plain loop: the
    /// vector shape handles every tail.
    ///
    /// Mutation (run, recorded here): fold `tail` into nothing (drop the
    /// final loop) -> lengths off a multiple of 8 read short and this fails.
    #[test]
    fn every_length_is_measured_whole() {
        for len in 0..40usize {
            let block: Vec<f32> = (0..len).map(|i| (i as f32 - 30.0) * 0.25).collect();
            let got = block_stats(&block);
            let peak = block.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            let sum: f32 = block.iter().map(|s| s * s).sum();
            assert_eq!(got.peak, peak, "len {len}");
            assert_eq!(got.sum_sq, sum, "len {len}");
        }
    }

    /// A read takes what was held and leaves nothing: the next read, with no
    /// block between, is empty.
    ///
    /// Mutation (run, recorded here): `take` loading instead of swapping the
    /// peak -> the second read still shows the spike and this fails.
    #[test]
    fn a_read_takes_what_was_held() {
        let lane = Lane::default();
        feed(&lane, 0.25, 64);
        assert_eq!(lane.take().peak.get(), 0.25);
        assert_eq!(lane.take(), Level::default());
    }

    /// Blocks between reads fold: the peak is the largest, the RMS covers all
    /// the frames, whatever the block size.
    ///
    /// Mutation (run, recorded here): `add` storing the block's figures
    /// instead of folding them -> only the last block shows and this fails.
    #[test]
    fn blocks_between_reads_fold() {
        let lane = Lane::default();
        feed(&lane, 1.0, 64);
        feed(&lane, 0.5, 192);
        let l = lane.take();
        assert_eq!(l.peak.get(), 1.0);
        assert_eq!(l.frames, Samples(256));
        // sqrt((64 * 1 + 192 * 0.25) / 256)
        assert_eq!(l.rms.get(), (112.0f32 / 256.0).sqrt());
    }

    /// The frame count does not wrap: a block that fits is added, one that
    /// would overflow is left out, so a reader that never reads grows
    /// nothing.
    ///
    /// Mutation (run, recorded here): `checked_add` -> `wrapping_add` -> the
    /// second read wraps to 1 and this fails.
    #[test]
    fn the_count_does_not_wrap() {
        let lane = Lane::default();
        lane.energy
            .store(pack(u32::MAX - 2, 4.0), Ordering::Relaxed);
        lane.add(
            BlockStats {
                peak: 1.0,
                sum_sq: 9.0,
            },
            2,
        );
        assert_eq!(lane.take().frames, Samples(u32::MAX as usize));
        lane.energy
            .store(pack(u32::MAX - 1, 4.0), Ordering::Relaxed);
        lane.add(
            BlockStats {
                peak: 1.0,
                sum_sq: 9.0,
            },
            3,
        );
        assert_eq!(lane.take().frames, Samples(u32::MAX as usize - 1));
    }

    /// A NaN sample does not set the peak, whether it falls in a vector pass
    /// or the tail.
    ///
    /// Mutation (run, recorded here): the tail loop's `max` as
    /// `if x >= peak || x.is_nan() { x } else { peak }` -> the 3-sample block
    /// fails. The same in the chunk loop and the final fold together -> the
    /// 16-sample block fails (one of the two alone is masked by the other
    /// `f32::max`, and a tail would mask it too, which is why the block has
    /// none).
    #[test]
    fn nan_does_not_set_the_peak() {
        assert_eq!(block_stats(&[0.5, f32::NAN, 0.25]).peak, 0.5);
        let mut long = vec![0.25f32; 16];
        long[3] = f32::NAN;
        long[9] = 0.75;
        assert_eq!(block_stats(&long).peak, 0.75);
    }

    /// A block measured in pieces reads as the block measured whole, whatever
    /// the cut: the peak exactly, the frames exactly, the RMS to rounding (the
    /// cut changes which partial sums are added first).
    ///
    /// Mutation (run, recorded here): `Lane::add` storing the piece's frames
    /// instead of adding them -> the frame count and RMS are the last
    /// piece's and this fails.
    #[test]
    fn a_block_in_pieces_reads_as_the_whole_block() {
        let block = sine(0.8, 300);
        let whole = read(&block);
        let lane = Lane::default();
        for _ in 0..2 {
            for piece in block.chunks(77) {
                lane.add(block_stats(piece), piece.len() as u32);
            }
            let l = lane.take();
            assert_eq!(l.peak, whole.peak);
            assert_eq!(l.frames, whole.frames);
            assert!(
                (l.rms.get() - whole.rms.get()).abs() < 1e-6,
                "{l:?} against {whole:?}"
            );
        }
    }

    fn read(samples: &[f32]) -> Level {
        let lane = Lane::default();
        lane.add(block_stats(samples), samples.len() as u32);
        lane.take()
    }

    /// An amplitude-`a` sine, 64 frames per period.
    fn sine(a: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| a * (std::f32::consts::TAU * i as f32 / 64.0).sin())
            .collect()
    }

    /// Whole periods of a sine: peak `a`, RMS `a / sqrt(2)`.
    ///
    /// Mutation (run, recorded here): the RMS divisor `frames - 1` in
    /// `Lane::take` -> this fails.
    #[test]
    fn a_known_sine_reads_its_peak_and_rms() {
        let l = read(&sine(0.5, 256));
        assert!((l.peak.get() - 0.5).abs() < 1e-5, "peak {}", l.peak.get());
        assert!(
            (l.rms.get() - 0.5 / 2f32.sqrt()).abs() < 1e-5,
            "rms {}",
            l.rms.get()
        );
    }

    /// The loud part sits past frame 8192, which the old stereo scratch
    /// dropped in a release build, so a meter of the head read silence.
    ///
    /// Mutation (run, recorded here): `block_stats` over `block[..8192]` ->
    /// the peak reads 0 and this fails.
    #[test]
    fn a_block_past_8192_frames_is_metered_whole() {
        let frames = 8192 + 1024;
        let mut buf = vec![0.0f32; frames];
        buf[8192..].fill(0.5);
        let l = read(&buf);
        assert_eq!(l.peak.get(), 0.5);
        // sqrt(1024 * 0.25 / 9216) = 0.5 / 3
        assert!((l.rms.get() - 0.5 / 3.0).abs() < 1e-6);
    }

    /// Silence reads zero (over the frames measured).
    ///
    /// Mutation (run, recorded here): a floor of 0.1 on the peak
    /// (`BlockStats` starting at 0.1) -> this fails.
    #[test]
    fn silence_reads_zero() {
        let l = read(&[0.0; 512]);
        assert_eq!((l.peak.get(), l.rms.get()), (0.0, 0.0));
        assert_eq!(l.frames, Samples(512));
    }

    /// Lanes are channels: a signal on one reads on its own lane and nothing
    /// leaks into its neighbour.
    ///
    /// Mutation (run, recorded here): the peak without `abs()` in
    /// `block_stats`' vector pass -> the negative signal reads 0 and this
    /// fails; the same in its tail loop fails `every_length_is_measured_whole`.
    #[test]
    fn a_one_sided_signal_reads_on_its_own_lane() {
        let (quiet, loud) = (Lane::default(), Lane::default());
        quiet.add(block_stats(&[0.0; 16]), 16);
        loud.add(block_stats(&[-0.75; 16]), 16);
        let (q, l) = (quiet.take(), loud.take());
        assert_eq!((q.peak.get(), q.rms.get()), (0.0, 0.0));
        assert_eq!((l.peak.get(), l.rms.get()), (0.75, 0.75));
    }

    /// An empty block adds nothing: what was held reads as it was.
    ///
    /// Mutation (run, recorded here): `BlockStats` for an empty block with a
    /// peak of 1.0 -> the peak changes and this fails.
    #[test]
    fn an_empty_block_changes_nothing() {
        let lane = Lane::default();
        lane.add(block_stats(&[0.5, 0.5]), 2);
        lane.add(block_stats(&[]), 0);
        assert_eq!(lane.take(), read(&[0.5, 0.5]));
    }

    /// The switch starts off and both ends see it.
    ///
    /// Mutation (run, recorded here): `disable` storing `true` -> this
    /// fails.
    #[test]
    fn the_switch_starts_off_and_both_ends_see_it() {
        let (reader, writer) = MeterReader::pair(MeterId(0), 1, 1);
        let on = || writer.cells.enabled.load(Ordering::Relaxed);
        assert!(!reader.is_enabled() && !on());
        reader.enable();
        assert!(on());
        reader.disable();
        assert!(!on());
    }

    /// A writer thread and a reading thread conserve the total: every frame
    /// and every unit of energy is taken exactly once.
    ///
    /// Mutation (run, recorded here): `Lane::add` as a load then a store
    /// (no compare-exchange) -> a read between them loses a block and the
    /// totals fall short.
    #[test]
    fn concurrent_reads_lose_nothing() {
        const BLOCKS: u32 = 20_000;
        let lane = Arc::new(Lane::default());
        let writer = {
            let lane = Arc::clone(&lane);
            std::thread::spawn(move || {
                for _ in 0..BLOCKS {
                    lane.add(
                        BlockStats {
                            peak: 0.5,
                            sum_sq: 4.0,
                        },
                        16,
                    );
                }
            })
        };
        let (mut frames, mut sum, mut peak) = (0u64, 0.0f64, 0.0f32);
        while !writer.is_finished() {
            let (n, s) = unpack(lane.energy.swap(0, Ordering::Relaxed));
            frames += u64::from(n);
            sum += f64::from(s);
            peak = peak.max(f32::from_bits(lane.peak.swap(0, Ordering::Relaxed)));
        }
        writer.join().expect("the writer finished");
        let (n, s) = unpack(lane.energy.swap(0, Ordering::Relaxed));
        frames += u64::from(n);
        sum += f64::from(s);
        assert_eq!(frames, u64::from(BLOCKS) * 16);
        assert_eq!(sum, f64::from(BLOCKS) * 4.0);
        assert_eq!(peak, 0.5);
    }
}
