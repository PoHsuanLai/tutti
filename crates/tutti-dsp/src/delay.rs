//! The fractional-read delay ring and its interpolation: a delay whose time is
//! modulated must interpolate between taps or it steps audibly.

use tutti_types::{SampleRate, Seconds};

/// How a fractional delay position is turned into a sample.
///
/// The cost/quality ladder for a *modulated* delay: a moving delay time lands
/// between samples, and how that gap is filled is what separates a clean chorus
/// from a gritty one. At a fixed, integral delay all three agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum InterpolationMode {
    /// Round to the nearest whole sample. Cheapest, and it quantises the delay
    /// to frame boundaries — a swept delay steps audibly ("zipper"). Fine for a
    /// static echo, wrong for chorus or flanger.
    None,
    /// Linear blend between the two neighbouring samples. The default: no
    /// stepping under modulation, at the cost of slight high-frequency damping
    /// that worsens as the fraction approaches 0.5.
    #[default]
    Linear,
    /// Four-point cubic Hermite. Keeps the highs that [`Linear`](Self::Linear)
    /// damps, for roughly four reads per sample instead of two. Reach for it on
    /// audibly-swept delays; it is wasted on a static one.
    CubicHermite,
}

/// A fractional-delay ring buffer: push a sample per frame, read back at any
/// real-valued delay.
///
/// The read position is fractional and interpolated
/// ([`InterpolationMode`]), which is what makes it usable as the core of a
/// modulated delay. Capacity is fixed at construction — [`read_sample`] clamps
/// rather than growing, so the line never allocates on the audio thread.
///
/// [`read_sample`]: Self::read_sample
#[derive(Clone)]
pub struct DelayLine {
    buffer: Vec<f32>,
    write_pos: usize,
    max_delay_samples: usize,
}

impl std::fmt::Debug for DelayLine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelayLine")
            .field("write_pos", &self.write_pos)
            .field("max_delay_samples", &self.max_delay_samples)
            .finish_non_exhaustive()
    }
}

impl DelayLine {
    /// Allocates a line holding up to `max_delay_samples` frames of history.
    ///
    /// The backing buffer is one frame longer than the maximum delay, so that
    /// an interpolating read at exactly `max_delay_samples` still has a second
    /// tap to blend toward. Allocates — build before going live.
    #[must_use]
    pub fn new(max_delay_samples: usize) -> Self {
        Self {
            buffer: vec![0.0; max_delay_samples + 1],
            write_pos: 0,
            max_delay_samples,
        }
    }

    /// Allocates a line sized for `max_delay_secs` at `sample_rate`.
    ///
    /// The length is rounded **up**, so the line always holds at least the
    /// requested duration. Note that the size is baked in here: a node changing
    /// its sample rate has to rebuild the line, which is what the delay node's
    /// `prepare` does.
    #[must_use]
    pub fn from_seconds(
        max_delay_secs: impl Into<Seconds>,
        sample_rate: impl Into<SampleRate>,
    ) -> Self {
        // `to_samples_ceil`, not a nearest-rounding cast: a line sized for
        // `max_delay` must hold *at least* that long, and rounding to nearest
        // under-allocates for half of all inputs. The multiply stays in f64 —
        // narrowing the sample rate to f32 first loses precision.
        let max_delay_secs: Seconds = max_delay_secs.into();
        let samples = max_delay_secs.to_samples_ceil(sample_rate);
        Self::new(samples.get())
    }

    /// Writes one frame at the cursor and advances, overwriting the oldest
    /// sample.
    ///
    /// Call exactly once per frame: the cursor *is* the line's clock, so a
    /// skipped or doubled push shifts every subsequent read by that much.
    #[inline]
    pub fn push_sample(&mut self, sample: f32) {
        self.buffer[self.write_pos] = sample;
        self.write_pos += 1;
        if self.write_pos >= self.buffer.len() {
            self.write_pos = 0;
        }
    }

    /// Reads the line `delay_samples` frames back from the newest write,
    /// interpolating per `mode`.
    ///
    /// `delay_samples` is a **fractional frame count**, not [`Seconds`] — the
    /// caller converts, because keeping the fraction is the whole point of an
    /// interpolated read. `0.0` is the sample just pushed. Where the unit types
    /// stop: `Samples` is an integer count and cannot carry the fraction, and
    /// `SamplePosition` is an absolute f64 offset into a wave, where this is a
    /// per-sample f32 span back from the write head.
    ///
    /// The delay is clamped to `0.0..=max_delay_samples`, so an over-long
    /// request shortens silently rather than panicking. Does not allocate.
    #[inline]
    pub fn read_sample(&self, delay_samples: f32, mode: InterpolationMode) -> f32 {
        let delay = delay_samples.clamp(0.0, self.max_delay_samples as f32);
        match mode {
            InterpolationMode::None => {
                let idx = delay.round() as usize;
                self.read_at(idx)
            }
            InterpolationMode::Linear => {
                let floor = delay.floor() as usize;
                let frac = delay - floor as f32;
                let s0 = self.read_at(floor);
                let s1 = self.read_at(floor + 1);
                s0 + frac * (s1 - s0)
            }
            InterpolationMode::CubicHermite => {
                let floor = delay.floor() as usize;
                let frac = delay - floor as f32;
                let sm1 = self.read_at(floor.saturating_sub(1));
                let s0 = self.read_at(floor);
                let s1 = self.read_at(floor + 1);
                let s2 = self.read_at(floor + 2);
                crate::cubic_hermite(sm1, s0, s1, s2, frac)
            }
        }
    }

    /// Zeroes the line and returns the cursor to 0, dropping any tail still in
    /// flight.
    ///
    /// Does not reallocate, so it is safe on the audio thread.
    pub fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write_pos = 0;
    }

    #[inline]
    fn read_at(&self, delay_samples: usize) -> f32 {
        let len = self.buffer.len();
        // `write_pos < len` and the clamped delay is below `len`, so the sum
        // is below `2 * len`: one conditional subtract wraps it.
        let idx = self.write_pos + len - 1 - delay_samples.min(self.max_delay_samples);
        self.buffer[if idx >= len { idx - len } else { idx }]
    }
}

/// A delay position in samples, keeping the fraction.
///
/// Deliberately **not** `Seconds::to_samples`: that returns `Samples`, an
/// integer frame count, and the whole point of an interpolated read is the
/// fractional part — rounding here would quantise every delay to a frame
/// boundary and step audibly under modulation.
///
/// The multiply stays in `f64` and narrows once at the end. Narrowing the
/// *rate* first (`secs * sample_rate as f32`) computes the whole position at
/// `f32` precision, which is coarser than the fractional read can resolve.
#[inline]
pub fn fractional_samples(secs: Seconds, sample_rate: SampleRate) -> f32 {
    (secs.get() * sample_rate.get()) as f32
}

/// One sample of one uncoupled channel: read the feedback tap, write the line,
/// read the output tap (the wet sample).
#[inline(always)]
pub fn delay_step(line: &mut DelayLine, x: f32, d: f32, fb: f32, interp: InterpolationMode) -> f32 {
    let tap = line.read_sample((d.max(1.0) - 1.0).max(0.0), interp);
    line.push_sample(x + tap * fb);
    line.read_sample(d, interp)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A line for 0.1 s at 48 kHz holds 4800 frames: `0.1 * 48000` is 4800 in
    /// `f64`, so its `_ceil` is 4800, where the `f32` nearest 0.1 times 48000
    /// is 4800.00007 and rounds up to 4801.
    #[test]
    fn a_line_is_sized_from_the_exact_seconds() {
        let from_secs = DelayLine::from_seconds(Seconds(0.1), SampleRate(48_000.0));
        assert_eq!(from_secs.buffer.len(), DelayLine::new(4800).buffer.len());
    }

    #[test]
    fn test_delay_line_basic() {
        let mut dl = DelayLine::new(10);
        dl.push_sample(1.0);
        dl.push_sample(0.0);
        dl.push_sample(0.0);

        let val = dl.read_sample(2.0, InterpolationMode::None);
        assert!((val - 1.0).abs() < 0.001, "Expected 1.0, got {val}");
    }

    /// An integer read is a delay of exactly that many frames, at every
    /// delay the line holds and every cursor position: an impulse pushed at
    /// frame `k` reads back at `d` only on frame `k + d`.
    #[test]
    fn an_integer_read_delays_by_exactly_that_many_frames() {
        const MAX: usize = 6;
        for at in 0..3 * (MAX + 1) {
            let mut dl = DelayLine::new(MAX);
            for frame in 0..at + MAX + 2 {
                dl.push_sample(if frame == at { 1.0 } else { 0.0 });
                for d in 0..=MAX {
                    let want = if frame == at + d { 1.0 } else { 0.0 };
                    let got = dl.read_sample(d as f32, InterpolationMode::None);
                    assert_eq!(got, want, "impulse at {at}, frame {frame}, delay {d}");
                }
            }
        }
    }

    #[test]
    fn test_delay_line_linear_interpolation() {
        let mut dl = DelayLine::new(10);
        dl.push_sample(0.0);
        dl.push_sample(1.0);
        dl.push_sample(0.0);

        let val = dl.read_sample(1.5, InterpolationMode::Linear);
        assert!((val - 0.5).abs() < 0.001, "Expected 0.5, got {val}");
    }

    #[test]
    fn test_delay_line_cubic_interpolation() {
        let mut dl = DelayLine::new(10);
        for i in 0..5 {
            dl.push_sample(i as f32);
        }
        let val = dl.read_sample(1.5, InterpolationMode::CubicHermite);
        // Between samples 3 and 2 (delay 1 = sample 3, delay 2 = sample 2)
        assert!(
            val > 2.0 && val < 4.0,
            "Cubic value {val} should be between recent samples"
        );
    }

    #[test]
    fn test_delay_line_reset() {
        let mut dl = DelayLine::new(10);
        dl.push_sample(1.0);
        dl.push_sample(1.0);
        dl.reset();
        let val = dl.read_sample(1.0, InterpolationMode::None);
        assert!((val).abs() < 0.001, "After reset, should read 0.0");
    }
}
