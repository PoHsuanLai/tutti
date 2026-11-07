//! Lock-free amplitude metering.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tutti_types::Amplitude;
use tutti_types::AtomicF32;

/// One meter reading: peak and RMS for a stereo pair, as linear
/// [`Amplitude`]s.
// A struct rather than a `(f32, f32, f32, f32)`: four same-typed positional
// values are one careless edit away from swapping a peak with an RMS.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MeterReading {
    /// Largest absolute sample on the left channel over the measured block.
    pub peak_left: Amplitude,
    /// Largest absolute sample on the right channel over the measured block.
    pub peak_right: Amplitude,
    /// Root-mean-square level of the left channel over the measured block.
    pub rms_left: Amplitude,
    /// Root-mean-square level of the right channel over the measured block.
    pub rms_right: Amplitude,
}

/// The cells both halves share: the four levels, each its own atomic, and the
/// switch beside them. The reads are independent, so a reading may straddle
/// two blocks: acceptable for a meter, where the display refreshes far slower
/// than the audio thread writes.
#[derive(Debug, Default)]
struct Cells {
    peak_left: AtomicF32,
    peak_right: AtomicF32,
    rms_left: AtomicF32,
    rms_right: AtomicF32,
    enabled: AtomicBool,
}

/// The master output's meter, the reading half: peak and RMS of what the
/// audio callback last measured, and the switch that says whether it should.
///
/// The switch matters because measuring costs a pass over the callback
/// buffer; when nothing is watching a meter, that work is skipped entirely.
/// Cheap to clone (both halves share the cells). Only its [`MeterWriter`]
/// writes a level: [`pair`](Self::pair) makes the two, and a reader cannot
/// reach the writer. `Default` is a meter whose writer is gone, silent and
/// disabled.
#[derive(Clone, Default, Debug)]
pub struct MasterMeter {
    cells: Arc<Cells>,
}

impl MasterMeter {
    /// A silent, **disabled** meter and its writer. Nothing is measured until
    /// [`enable`](Self::enable) is called.
    #[must_use]
    pub fn pair() -> (MeterWriter, Self) {
        let cells = Arc::new(Cells::default());
        (
            MeterWriter {
                cells: Arc::clone(&cells),
            },
            Self { cells },
        )
    }

    /// Starts measuring. The audio callback begins on the next block.
    pub fn enable(&self) {
        self.cells.enabled.store(true, Ordering::Release);
    }

    /// Stops measuring, skipping the pass over the buffer entirely. The last
    /// reading stays readable and goes stale.
    pub fn disable(&self) {
        self.cells.enabled.store(false, Ordering::Release);
    }

    /// Whether the audio callback is filling this meter.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.cells.enabled.load(Ordering::Acquire)
    }

    /// The last measured buffer's reading.
    ///
    /// The four loads are independent, so a reading may straddle two blocks.
    pub fn get(&self) -> MeterReading {
        let c = &*self.cells;
        MeterReading {
            peak_left: Amplitude(c.peak_left.load(Ordering::Acquire)),
            peak_right: Amplitude(c.peak_right.load(Ordering::Acquire)),
            rms_left: Amplitude(c.rms_left.load(Ordering::Acquire)),
            rms_right: Amplitude(c.rms_right.load(Ordering::Acquire)),
        }
    }
}

/// The audio callback's half of the master meter: the only value that writes
/// a level. Not `Clone`, and [`measure`](Self::measure) takes `&mut self`, so
/// one callback owns it.
#[derive(Debug)]
pub struct MeterWriter {
    cells: Arc<Cells>,
}

/// Running peak and sum of squares for one channel.
#[derive(Clone, Copy, Default)]
struct Channel {
    peak: f32,
    sum_sq: f32,
}

impl Channel {
    #[inline]
    fn push(self, s: f32) -> Self {
        Self {
            peak: self.peak.max(s.abs()),
            sum_sq: self.sum_sq + s * s,
        }
    }

    #[inline]
    fn rms(self, frames: usize) -> Amplitude {
        Amplitude((self.sum_sq / frames as f32).sqrt())
    }
}

impl MeterWriter {
    /// Whether the reading half asked for measurements.
    #[inline]
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.cells.enabled.load(Ordering::Acquire)
    }

    /// Measures peak and RMS over one interleaved stereo buffer (L, R, L, R,
    /// ...) and publishes them. A trailing odd sample is ignored, and an empty
    /// buffer publishes nothing.
    ///
    /// RT-safe: one pass, four stores, no allocation. The sum keeps its
    /// left-to-right order (reassociating an `f32` sum changes the result), so
    /// it does not vectorize; nothing here relies on whether the peak does.
    #[inline]
    pub fn measure(&mut self, interleaved: &[f32]) {
        let pairs = interleaved.as_chunks::<2>().0;
        if pairs.is_empty() {
            return;
        }
        let (left, right) = pairs.iter().fold(
            (Channel::default(), Channel::default()),
            |(l, r), &[a, b]| (l.push(a), r.push(b)),
        );
        let frames = pairs.len();
        let c = &*self.cells;
        c.peak_left.store(left.peak, Ordering::Release);
        c.peak_right.store(right.peak, Ordering::Release);
        c.rms_left.store(left.rms(frames).get(), Ordering::Release);
        c.rms_right
            .store(right.rms(frames).get(), Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(samples: &[f32]) -> MeterReading {
        let (mut writer, meter) = MasterMeter::pair();
        writer.measure(samples);
        meter.get()
    }

    /// An amplitude-`a` sine, 64 frames per period, the same on both channels.
    fn sine(a: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|i| {
                let s = a * (std::f32::consts::TAU * i as f32 / 64.0).sin();
                [s, s]
            })
            .collect()
    }

    // Mutations, each run and each failed the tests named:
    // - the peak without `abs()`: `a_one_sided_signal_reads_on_its_own_channel`.
    // - the RMS divisor `frames - 1`: `a_known_sine_reads_its_peak_and_rms`,
    //   `a_block_past_8192_frames_is_metered_whole` and the one-sided test.
    // - metering only the first 8192 frames: `a_block_past_8192_frames_is_metered_whole`.
    // - `(l.push(b), r.push(a))`: the one-sided test and the past-8192 test.
    // - a floor of 0.1 on the peak: `silence_reads_zero`.
    // - dropping the empty-buffer return: `an_empty_buffer_publishes_nothing`.
    // - `disable` storing `true`: `the_switch_starts_off_and_both_halves_see_it`.
    #[test]
    fn a_known_sine_reads_its_peak_and_rms() {
        // Whole periods: peak `a`, RMS `a / sqrt(2)`.
        let r = read(&sine(0.5, 256));
        for (peak, rms) in [(r.peak_left, r.rms_left), (r.peak_right, r.rms_right)] {
            assert!((peak.get() - 0.5).abs() < 1e-5, "peak {}", peak.get());
            assert!(
                (rms.get() - 0.5 / 2f32.sqrt()).abs() < 1e-5,
                "rms {}",
                rms.get()
            );
        }
    }

    /// The loud part sits past frame 8192, which the old scratch dropped in a
    /// release build, so a meter of the head read silence.
    #[test]
    fn a_block_past_8192_frames_is_metered_whole() {
        let frames = 8192 + 1024;
        let mut buf = vec![0.0f32; frames * 2];
        for f in 8192..frames {
            buf[f * 2] = 0.5;
        }
        let r = read(&buf);
        assert_eq!(r.peak_left.get(), 0.5);
        // sqrt(1024 * 0.25 / 9216) = 0.5 / 3
        assert!((r.rms_left.get() - 0.5 / 3.0).abs() < 1e-6);
        assert_eq!(r.peak_right.get(), 0.0);
    }

    #[test]
    fn silence_reads_zero() {
        assert_eq!(read(&[0.0; 512]), MeterReading::default());
    }

    #[test]
    fn a_one_sided_signal_reads_on_its_own_channel() {
        let r = read(&[0.0, -0.75, 0.0, -0.75]);
        assert_eq!(r.peak_left.get(), 0.0);
        assert_eq!(r.rms_left.get(), 0.0);
        assert_eq!(r.peak_right.get(), 0.75);
        assert_eq!(r.rms_right.get(), 0.75);
    }

    #[test]
    fn an_empty_buffer_publishes_nothing() {
        let (mut writer, meter) = MasterMeter::pair();
        writer.measure(&[0.5, 0.5]);
        let before = meter.get();
        writer.measure(&[]);
        assert_eq!(meter.get(), before);
    }

    #[test]
    fn the_switch_starts_off_and_both_halves_see_it() {
        let (writer, meter) = MasterMeter::pair();
        assert!(!meter.is_enabled() && !writer.is_enabled());
        meter.enable();
        assert!(writer.is_enabled());
        meter.disable();
        assert!(!writer.is_enabled());
    }
}
