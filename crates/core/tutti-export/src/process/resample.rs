//! Sample-rate conversion, as a streaming stage.
//!
//! The converter is a *block* resampler (an FFT overlap-add one,
//! [`FftResampler`], on `tutti_dsp::RealFft`, so its bits do not depend on the
//! CPU) — `input_frames_next()` / `process_into_buffer()` in a loop — so this
//! never needs the whole signal, which is why nothing in this crate buffers for
//! it.
//!
//! # The delay
//!
//! An FFT resampler has latency: `output_delay()` FRAMES of its output are the
//! filter warming up, not signal. Ignoring that figure shifts every resampled
//! export late by exactly it (320 frames at 44.1→48 k, ~6.7 ms of leading
//! silence) and loses the same amount off the tail, because a blind `truncate()`
//! to the expected length cuts where the delayed content would have been.
//! [`Resampler`] drops those frames at the head and flushes with silence at the
//! end to push the real tail out.

use crate::error::{Error, Result};
use tutti_dsp::{FftResampler, ResampleError};
use tutti_types::{SampleRate, Samples};

/// Input frames per FFT chunk, and how finely each chunk is subdivided.
///
/// Named for the quantity, not for a ranking. A `Fast | Medium | High | Best`
/// enum names which end of a scale a caller is on and nothing about what changes
/// between two of them, so the trade cannot be reasoned about — longer chunks
/// are a steeper anti-alias filter and more latency; more sub-chunks is finer
/// time resolution and more work — and a point the enum does not list cannot be
/// expressed at all.
///
/// Shaped like `FftSize` in `tutti-sampler`: constants named for their numbers,
/// `MIN`/`MAX` bounds, and a fallible constructor for anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkSize {
    chunk: Samples,
    sub_chunks: usize,
}

impl ChunkSize {
    /// 512 frames, undivided.
    pub const N512: Self = Self {
        chunk: Samples(512),
        sub_chunks: 1,
    };
    /// 1024 frames in 2 — the default.
    pub const N1024: Self = Self {
        chunk: Samples(1024),
        sub_chunks: 2,
    };
    /// 2048 frames in 4.
    pub const N2048: Self = Self {
        chunk: Samples(2048),
        sub_chunks: 4,
    };
    /// 4096 frames in 8.
    pub const N4096: Self = Self {
        chunk: Samples(4096),
        sub_chunks: 8,
    };

    /// Shortest chunk that still admits a subdivision.
    pub const MIN: Self = Self {
        chunk: Samples(64),
        sub_chunks: 1,
    };
    /// Longest chunk before the filter's latency dominates a short render.
    pub const MAX: Self = Self {
        chunk: Samples(32768),
        sub_chunks: 32,
    };

    /// Every preset, for tests and for enumerating a UI.
    pub const PRESETS: [Self; 4] = [Self::N512, Self::N1024, Self::N2048, Self::N4096];

    /// A chunk length and subdivision, or `None` unless `chunk` is a power of
    /// two in [`MIN`](Self::MIN)..=[`MAX`](Self::MAX) and `sub_chunks` divides
    /// it, within `1..=MAX.sub_chunks()`.
    ///
    /// Fallible rather than clamping: rounding a chunk length hands back a
    /// resampler whose latency is not the one that was asked for. The chunk
    /// sets the input frames per call and, with the sub-chunk count, how many
    /// of the rate pair's repeating units one FFT block holds; the block
    /// sizes are those units, not powers of two.
    #[must_use]
    pub const fn new(chunk: Samples, sub_chunks: usize) -> Option<Self> {
        if !chunk.0.is_power_of_two()
            || chunk.0 < Self::MIN.chunk.0
            || chunk.0 > Self::MAX.chunk.0
            || sub_chunks == 0
            || sub_chunks > Self::MAX.sub_chunks
            || !chunk.0.is_multiple_of(sub_chunks)
        {
            return None;
        }
        Some(Self { chunk, sub_chunks })
    }

    /// Input frames per FFT chunk.
    #[must_use]
    pub const fn chunk(self) -> Samples {
        self.chunk
    }

    /// Sub-chunks each chunk is split into.
    #[must_use]
    pub const fn sub_chunks(self) -> usize {
        self.sub_chunks
    }
}

impl Default for ChunkSize {
    fn default() -> Self {
        Self::N1024
    }
}

/// A rate as the converter's integer Hz, or an error if it is not a finite
/// rate of at least 0.5 Hz (which rounds to 1) and under 2^32 Hz. A bare
/// `as usize` would saturate infinity and a huge rate, and turn NaN or a
/// negative rate into 0.
fn whole_hz(hz: f64) -> Result<usize> {
    if hz.is_finite() && (0.5..4_294_967_296.0).contains(&hz) {
        Ok(hz.round() as usize)
    } else {
        Err(ResampleError::InvalidRate { hz }.into())
    }
}

pub(crate) use streaming::Resampler;

mod streaming {
    use super::*;

    /// A rate converter fed one block at a time.
    ///
    /// Owns its own input carry, so callers hand it whatever block size they
    /// have and it emits output whenever the converter has a chunk ready.
    pub(crate) struct Resampler {
        inner: FftResampler,
        channels: usize,
        ratio: f64,
        /// Input frames per channel, consumed up to `head`.
        carry: Vec<Vec<f32>>,
        /// Frames at the front of every `carry` plane already handed to the
        /// converter. A chunk advances it instead of shifting the plane down,
        /// so a push of the whole signal (the two-pass path) costs O(N) rather
        /// than one memmove of the rest per chunk; `drain` drops the consumed
        /// prefix once, on its way out.
        head: usize,
        /// Output frames still to be discarded as filter warm-up.
        skip: usize,
        /// Real input frames fed so far (excludes the flush padding), which is
        /// what sets how much output is signal rather than filter tail.
        fed: usize,
        /// Output frames emitted so far, so the budget is a running total.
        emitted: usize,
        /// Reusable scratch so the per-block path does not allocate.
        in_scratch: Vec<Vec<f32>>,
        /// The converter's output, `output_frames_max()` per channel, written
        /// by `process_into_buffer`.
        out_scratch: Vec<Vec<f32>>,
    }

    impl Resampler {
        /// Both rates are [`SampleRate`], which every caller already holds;
        /// the narrowing to the converter's integer rates happens once, here,
        /// at its boundary.
        ///
        /// This does **not** make a transposition a compile error: the two
        /// arguments are the same type, so swapping them still builds
        /// (verified). Only a `SourceRate`/`TargetRate` split would catch it,
        /// and two names for one behaviour is not a type. What is gained is one
        /// derivation of the ratio, from values that never lost precision.
        pub(crate) fn new(
            channels: usize,
            source_rate: SampleRate,
            target_rate: SampleRate,
            chunk: ChunkSize,
        ) -> Result<Self> {
            let (source_hz, target_hz) = (source_rate.get(), target_rate.get());
            let (source_int, target_int) = (whole_hz(source_hz)?, whole_hz(target_hz)?);
            let inner = FftResampler::new(
                source_int,
                target_int,
                chunk.chunk().get(),
                chunk.sub_chunks(),
                channels,
            )?;
            let skip = inner.output_delay();
            let out_scratch = vec![vec![0.0; inner.output_frames_max()]; channels];
            Ok(Self {
                inner,
                channels,
                ratio: target_hz / source_hz,
                carry: vec![Vec::new(); channels],
                head: 0,
                skip,
                fed: 0,
                emitted: 0,
                in_scratch: vec![Vec::new(); channels],
                out_scratch,
            })
        }

        /// Feed one block of planar input; append whatever output it completes.
        pub(crate) fn push(&mut self, planes: &[Vec<f32>], out: &mut [Vec<f32>]) -> Result<()> {
            let n = planes.first().map_or(0, |p| p.len());
            for (c, p) in planes.iter().enumerate().take(self.channels) {
                self.carry[c].extend_from_slice(p);
            }
            self.fed += n;
            self.drain(out, false)
        }

        /// Flush the filter's delayed tail out.
        ///
        /// Feeding silence pushes the last `output_delay()` frames of real
        /// signal — still inside the converter — into `out`. The budget in
        /// [`drain`](Self::drain) then stops the padding itself from being
        /// emitted, so the total lands at `input * ratio`.
        ///
        /// Both halves are needed. Capping without flushing is what the previous
        /// version did, and it cut exactly where the real tail would have been.
        pub(crate) fn finish(&mut self, out: &mut [Vec<f32>]) -> Result<()> {
            // Enough silence to clear the filter's delay plus one whole chunk,
            // so the final partial chunk is emitted too.
            let pad = self.inner.output_delay() + self.inner.input_frames_next();
            let silence: Vec<Vec<f32>> = vec![vec![0.0; pad]; self.channels];
            for (c, p) in silence.iter().enumerate().take(self.channels) {
                self.carry[c].extend_from_slice(p);
            }
            self.drain(out, true)
        }

        /// Frames of real output still owed, given everything fed so far.
        ///
        /// The budget is a running total rather than a truncate at the end,
        /// because output is emitted block by block: a caller has already
        /// written earlier frames by the time `finish` runs, so there is nothing
        /// left to trim.
        fn budget(&self) -> usize {
            let want = (self.fed as f64 * self.ratio).round() as usize;
            want.saturating_sub(self.emitted)
        }

        /// `(unconsumed carry frames, head, input frames the next chunk
        /// needs)`, for the test that pins the carry's bound.
        #[cfg(test)]
        pub(crate) fn backlog(&self) -> (usize, usize, usize) {
            let len = self.carry.first().map_or(0, |c| c.len());
            (len - self.head, self.head, self.inner.input_frames_next())
        }

        fn drain(&mut self, out: &mut [Vec<f32>], flushing: bool) -> Result<()> {
            let result = self.drain_chunks(out, flushing);
            // Drop the consumed prefix once per call, not once per chunk.
            for c in self.carry.iter_mut() {
                c.drain(..self.head);
            }
            self.head = 0;
            result
        }

        fn drain_chunks(&mut self, out: &mut [Vec<f32>], flushing: bool) -> Result<()> {
            // Every pass consumes one chunk, so the carry this call starts
            // with runs out within `ceil(have / need)` passes (the last one
            // padded when flushing). The `+ 1` is slack; past it the carry is
            // not shrinking, and a bug that stops it shrinking fails here
            // instead of spinning forever. Mutations (run): drop
            // `self.head += need`, or pad to `self.head + need + 1` → both
            // hung the flush before this bound; now every resample test
            // fails at it.
            let max_passes = {
                let have = self.carry.first().map_or(0, |c| c.len()) - self.head;
                have.div_ceil(self.inner.input_frames_next().max(1)) + 1
            };
            let mut passes = 0usize;
            loop {
                let need = self.inner.input_frames_next();
                let have = self.carry.first().map_or(0, |c| c.len()) - self.head;
                if have < need {
                    if !flushing {
                        return Ok(());
                    }
                    // Pad the last partial chunk so the converter can emit it.
                    if have == 0 {
                        return Ok(());
                    }
                    for c in self.carry.iter_mut() {
                        c.resize(self.head + need, 0.0);
                    }
                }

                passes += 1;
                if passes > max_passes {
                    debug_assert!(false, "the resample carry stopped shrinking");
                    return Err(Error::ResampleStalled { chunks: max_passes });
                }

                let chunk = self.head..self.head + need;
                for (c, scratch) in self.in_scratch.iter_mut().enumerate() {
                    scratch.clear();
                    scratch.extend_from_slice(&self.carry[c][chunk.clone()]);
                }
                let produced_len = self
                    .inner
                    .process_into_buffer(&self.in_scratch, &mut self.out_scratch)?;
                let produced = &self.out_scratch;
                self.head += need;

                // Drop warm-up frames at the head before anything is kept.
                let drop = self.skip.min(produced_len);
                self.skip -= drop;
                // Emit at most what is still owed, so the flush padding that
                // pushed the tail out does not itself become output.
                let take = (produced_len - drop).min(self.budget());
                for (c, p) in produced.iter().enumerate().take(out.len()) {
                    out[c].extend_from_slice(&p[drop..drop + take]);
                }
                self.emitted += take;

                if flushing && self.carry.first().is_none_or(|c| c.len() == self.head) {
                    return Ok(());
                }
            }
        }
    }
}

/// Convert a whole [`Rendered`](crate::Rendered) to `opts.target_rate`.
///
/// The streaming [`Resampler`] is the right shape for an encode, which pulls the
/// graph a block at a time. This is for the two-pass path, which already holds
/// the signal whole and must convert it *before* measuring — sample-rate
/// conversion moves the true peak, so a gain measured at the render rate would
/// miss its target once the file is written at another.
// Not codec-gated: this converts buffers through the FFT resampler, which is
// unconditional, and its caller `render_normalized_to_file` is ungated too. Gating
// it would make a no-codec build fail to compile rather than merely fail to
// encode.
pub(crate) fn resample_rendered(
    rendered: &crate::Rendered,
    opts: crate::config::Resample,
) -> crate::Result<crate::Rendered> {
    let channels = rendered.channels();
    let mut rs = Resampler::new(channels, rendered.sample_rate, opts.target_rate, opts.chunk)?;
    let mut out: Vec<Vec<f32>> = vec![Vec::new(); channels];
    rs.push(&rendered.planes, &mut out)?;
    rs.finish(&mut out)?;

    Ok(crate::Rendered {
        planes: out,
        sample_rate: opts.target_rate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Push `planes` through a [`Resampler`] in blocks of `block` frames
    /// (`None` is the whole signal at once, the two-pass path), then finish.
    fn resample_blocked(
        planes: &[Vec<f32>],
        from: u32,
        to: u32,
        chunk: ChunkSize,
        block: Option<usize>,
    ) -> Vec<Vec<f32>> {
        let n = planes[0].len();
        let mut r = Resampler::new(planes.len(), from.into(), to.into(), chunk).unwrap();
        let mut out = vec![Vec::new(); planes.len()];
        match block {
            None => r.push(planes, &mut out).unwrap(),
            Some(len) => {
                for start in (0..n).step_by(len) {
                    let end = (start + len).min(n);
                    let part: Vec<Vec<f32>> =
                        planes.iter().map(|p| p[start..end].to_vec()).collect();
                    r.push(&part, &mut out).unwrap();
                }
            }
        }
        r.finish(&mut out).unwrap();
        out
    }

    const RATE_PAIRS: [(u32, u32); 3] = [(44_100, 48_000), (48_000, 44_100), (48_000, 96_000)];
    const BLOCKINGS: [Option<usize>; 3] = [None, Some(64), Some(333)];

    /// The output length is `round(input * ratio)` however the input is
    /// blocked, at every preset chunk size, in both directions, for lengths
    /// that end mid-chunk or before the first one.
    ///
    /// Mutation (run): round the budget down (`.floor()` in `budget`) →
    /// 44.1->48 k, n = 4410 yields 4799 frames, not 4800. (Reading chunks from
    /// the wrong place keeps the length; `the_signal_survives_every_blocking`
    /// catches that.)
    #[test]
    fn the_length_is_the_rounded_ratio_however_the_input_is_blocked() {
        for (from, to) in RATE_PAIRS {
            for chunk in ChunkSize::PRESETS {
                for n in [0usize, 1, 1000, 4096, 4410, 10_007] {
                    let planes: Vec<Vec<f32>> = (0..2).map(|_| vec![0.1f32; n]).collect();
                    let want = (n as f64 * f64::from(to) / f64::from(from)).round() as usize;
                    for block in BLOCKINGS {
                        let out = resample_blocked(&planes, from, to, chunk, block);
                        for (c, plane) in out.iter().enumerate() {
                            assert_eq!(
                                plane.len(),
                                want,
                                "{from}->{to} {chunk:?} n={n} block={block:?} ch{c}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// The signal itself survives every blocking: a 1 kHz sine of amplitude
    /// 0.5 on one channel keeps its level (within 0.001 dB, the bound
    /// `a_sine_keeps_its_frequency_and_amplitude` holds the default chunk to)
    /// and matches the analytic waveform `0.5 sin(2 pi f j / to)` to 1e-4 (the
    /// latency is compensated, so frame `j` is time `j / to`), and a 0.25
    /// constant on the other stays within 1e-4 of it, measured in the middle
    /// half, so a chunk read from the wrong place, a phase error or a seam
    /// shows. The bounds sit above the worst error measured at every preset
    /// (under 1e-5).
    ///
    /// Mutation (run): read each chunk from the start of the carry instead of
    /// the head (`let chunk = 0..need;`) → the second chunk repeats the first
    /// and the waveform is off at the seam; delaying the head by one frame
    /// (`output_delay() + 1` for `skip` in `Resampler::new`) → the level holds but the waveform
    /// is off by about 0.07.
    #[test]
    fn the_signal_survives_every_blocking() {
        for (from, to) in RATE_PAIRS {
            let n = from as usize;
            let planes = vec![tone(1000.0, f64::from(from), n, 0.5), vec![0.25f32; n]];
            for chunk in ChunkSize::PRESETS {
                for block in BLOCKINGS {
                    let out = resample_blocked(&planes, from, to, chunk, block);
                    let len = out[0].len();
                    let (a, b) = (len / 4, 3 * len / 4);
                    let db = 20.0 * (level_at(&out[0], f64::from(to), 1000.0, a, b) / 0.5).log10();
                    assert!(
                        db.abs() < 0.001,
                        "{from}->{to} {chunk:?} block={block:?}: sine {db:+.4} dB"
                    );
                    // Latency is compensated, so output frame j is the input
                    // tone at time j / to: compare the waveform, not only its
                    // level, so a phase or chunk-offset error shows.
                    let waveform = (a..b)
                        .map(|j| {
                            let want = 0.5
                                * (2.0 * std::f64::consts::PI * 1000.0 * j as f64 / f64::from(to))
                                    .sin();
                            (f64::from(out[0][j]) - want).abs()
                        })
                        .fold(0.0f64, f64::max);
                    assert!(
                        waveform < 1e-4,
                        "{from}->{to} {chunk:?} block={block:?}: waveform off by {waveform}"
                    );
                    let worst = out[1][a..b]
                        .iter()
                        .map(|s| (s - 0.25).abs())
                        .fold(0.0f32, f32::max);
                    assert!(
                        worst < 1e-4,
                        "{from}->{to} {chunk:?} block={block:?}: DC off by {worst}"
                    );
                }
            }
        }
    }

    /// The carry holds less than one chunk between pushes, so a streaming
    /// encode keeps O(chunk) of input, not the whole render: every `push`
    /// leaves the consumed prefix dropped (`head == 0`) and fewer frames
    /// than the next chunk needs.
    ///
    /// Mutation (run, M4): never drop the consumed prefix and never reset
    /// `head` in `drain` → `head` is non-zero after the first chunk and the
    /// assertion fails (the bit-identity test still passes, since the head
    /// offset alone reads the right frames).
    #[test]
    fn the_carry_stays_under_one_chunk_between_pushes() {
        for chunk in ChunkSize::PRESETS {
            let mut r = Resampler::new(2, 44_100.into(), 48_000.into(), chunk).unwrap();
            let mut out = vec![Vec::new(); 2];
            let block = vec![vec![0.25f32; 37]; 2];
            for i in 0..2_000 {
                r.push(&block, &mut out).unwrap();
                let (backlog, head, need) = r.backlog();
                assert_eq!(
                    head, 0,
                    "{chunk:?}: push {i} left the consumed prefix in place"
                );
                assert!(
                    backlog < need,
                    "{chunk:?}: push {i} left {backlog} frames, a whole chunk is {need}"
                );
            }
            r.finish(&mut out).unwrap();
        }
    }

    fn resample(planes: &[Vec<f32>], from: u32, to: u32, chunk: ChunkSize) -> Vec<Vec<f32>> {
        // The `u32`s keep the call sites below reading as plain rates; the
        // conversion is here, once.
        let mut r = Resampler::new(planes.len(), from.into(), to.into(), chunk).unwrap();
        let mut out = vec![Vec::new(); planes.len()];
        r.push(planes, &mut out).unwrap();
        r.finish(&mut out).unwrap();
        out
    }

    /// `new` refuses anything it cannot honour, rather than rounding to
    /// something adjacent and reporting success.
    #[test]
    fn a_chunk_size_must_be_a_power_of_two_that_its_subdivision_divides() {
        assert!(
            ChunkSize::new(Samples(1000), 2).is_none(),
            "not a power of 2"
        );
        assert!(
            ChunkSize::new(Samples(1024), 0).is_none(),
            "zero sub-chunks"
        );
        assert!(ChunkSize::new(Samples(32), 1).is_none(), "below MIN");
        assert!(ChunkSize::new(Samples(65536), 1).is_none(), "above MAX");
        assert!(
            ChunkSize::new(Samples(1024), 3).is_none(),
            "3 does not divide 1024"
        );
        assert_eq!(ChunkSize::new(Samples(1024), 2), Some(ChunkSize::N1024));
    }

    /// Every preset must be constructible through `new` — if one is not, the
    /// constants and the validation disagree about what is legal.
    #[test]
    fn every_preset_satisfies_its_own_constructor() {
        for p in ChunkSize::PRESETS {
            assert_eq!(
                ChunkSize::new(p.chunk(), p.sub_chunks()),
                Some(p),
                "{p:?} is a preset its own constructor rejects"
            );
        }
    }

    /// An impulse must come out where the rate change puts it, not
    /// `output_delay()` frames later (320 frames late at 44.1→48 k if the
    /// delay is not compensated).
    #[test]
    fn an_impulse_keeps_its_position() {
        let n = 8192;
        let at = 1000usize;
        let mut plane = vec![0.0f32; n];
        plane[at] = 1.0;

        for q in [
            ChunkSize::N512,
            ChunkSize::N1024,
            ChunkSize::N2048,
            ChunkSize::N4096,
        ] {
            let out = resample(&[plane.clone()], 44100, 48000, q);
            let peak = out[0]
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
                .map(|(i, _)| i)
                .unwrap();
            let expected = (at as f64 * 48000.0 / 44100.0).round() as usize;
            let err = peak.abs_diff(expected);
            assert!(
                err <= 2,
                "{q:?}: impulse landed at {peak}, expected ~{expected} (off by {err})"
            );
        }
    }

    /// …and the tail must survive. A marker on the final input frame vanishes
    /// entirely if the output is truncated where the delayed tail would be.
    #[test]
    fn the_tail_is_flushed_not_truncated() {
        let n = 4096;
        let mut plane = vec![0.0f32; n];
        plane[n - 1] = 1.0;
        let out = resample(&[plane], 44100, 48000, ChunkSize::N1024);
        let tail_peak = out[0]
            .iter()
            .rev()
            .take(256)
            .fold(0.0f32, |a, &b| a.max(b.abs()));
        assert!(
            tail_peak > 0.1,
            "the last input frame must appear in the output; tail peak {tail_peak}"
        );
    }

    #[test]
    fn output_length_tracks_the_rate_ratio() {
        let n = 44100;
        let out = resample(&[vec![0.5f32; n]], 44100, 48000, ChunkSize::N1024);
        let expected = 48000usize;
        let err = out[0].len().abs_diff(expected);
        assert!(
            err < 256,
            "expected ~{expected} frames, got {} (off by {err})",
            out[0].len()
        );
    }

    /// Amplitude and phase-free level of `f_hz` in `x[from..to]` (sampled at
    /// `rate`): the magnitude of the single-bin DFT, `2/N * |sum x e^-iwt|`,
    /// so a sine of amplitude `a` at `f_hz` reads `a`.
    fn level_at(x: &[f32], rate: f64, f_hz: f64, from: usize, to: usize) -> f64 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, &v) in x.iter().enumerate().take(to).skip(from) {
            let w = 2.0 * std::f64::consts::PI * f_hz * n as f64 / rate;
            re += v as f64 * w.cos();
            im -= v as f64 * w.sin();
        }
        2.0 * re.hypot(im) / (to - from) as f64
    }

    fn tone(hz: f64, rate: f64, n: usize, amp: f64) -> Vec<f32> {
        (0..n)
            .map(|i| (amp * (2.0 * std::f64::consts::PI * hz * i as f64 / rate).sin()) as f32)
            .collect()
    }

    /// A sine keeps its frequency and amplitude: a 1 kHz, 0.5-amplitude sine
    /// at 44.1 kHz read at 48 kHz has level 0.5 within 0.001 dB at 1 kHz
    /// (so the output's frequency is 1 kHz) and the strongest of a +-10 Hz scan
    /// is the 1 kHz bin. Measured in the middle half, clear of the edges.
    ///
    /// Mutations (run): drop the overlap-add in `resample_unit` (`*out =
    /// fresh + 0.0 * carried`) → the level collapses at every block seam and
    /// this fails; scale the filter by 1.01 in `Unit::new` → the level reads
    /// 0.5 x 1.01 = 0.505, +0.086 dB, over the bound.
    #[test]
    fn a_sine_keeps_its_frequency_and_amplitude() {
        for (from, to) in [
            (44_100.0, 48_000.0),
            (48_000.0, 44_100.0),
            (48_000.0, 96_000.0),
        ] {
            let n = from as usize; // one second
            let out = resample(
                &[tone(1000.0, from, n, 0.5)],
                from as u32,
                to as u32,
                ChunkSize::N1024,
            );
            let x = &out[0];
            let (a, b) = (x.len() / 4, 3 * x.len() / 4);
            let level = level_at(x, to, 1000.0, a, b);
            let db = 20.0 * (level / 0.5).log10();
            assert!(
                db.abs() < 0.001,
                "{from}->{to}: level {level} ({db:+.4} dB)"
            );
            for off in [-10.0, 10.0] {
                let other = level_at(x, to, 1000.0 + off, a, b);
                assert!(
                    other < level,
                    "{from}->{to}: {off:+} Hz reads {other} >= {level}"
                );
            }
        }
    }

    /// DC gain is 1: a constant comes out as that constant (within 1e-5 once
    /// the filter has settled), and the output never sits at the start-up value.
    ///
    /// Mutation (run): drop the `/= sum` in `sinc_filter` → the DC error is
    /// 0.0119 on the 0.5 input (a gain of about 1.024), far over the bound.
    #[test]
    fn dc_passes_with_unit_gain() {
        for (from, to) in [(44_100u32, 48_000u32), (48_000, 44_100)] {
            let out = resample(
                &[vec![0.5f32; from as usize / 2]],
                from,
                to,
                ChunkSize::N1024,
            );
            let x = &out[0];
            let mid = &x[x.len() / 4..3 * x.len() / 4];
            let worst = mid.iter().map(|s| (s - 0.5).abs()).fold(0.0f32, f32::max);
            assert!(worst < 1e-5, "{from}->{to}: DC off by {worst}");
        }
    }

    /// Content above the new Nyquist is attenuated, not folded down. Tones of
    /// 0.5 amplitude at 48 kHz, converted to 44.1 kHz (Nyquist 22.05 kHz),
    /// would alias to `44.1 kHz - f`; the output holds under -120 dB re the
    /// input (`AMOUNT` below) at that frequency for every `f` from 22.3 kHz to
    /// 23.9 kHz, at every preset chunk size. A 15 kHz tone, well inside the
    /// band, passes within 0.01 dB.
    ///
    /// Mutation (run): drop the anti-alias cut when downsampling, so the
    /// filter is cut at the output's Nyquist without the fit
    /// (`cutoff(fft_size_out)` -> `1.0` in `Unit::new`) → the 22.3 kHz tone
    /// leaves an alias at 21.8 kHz of -57.5 dB and this fails.
    #[test]
    fn content_above_the_new_nyquist_is_rejected_not_folded() {
        const AMOUNT: f64 = -120.0;
        let (from, to) = (48_000.0, 44_100.0);
        let n = 48_000;
        for chunk in ChunkSize::PRESETS {
            for f in [22_300.0, 22_600.0, 23_000.0, 23_500.0, 23_900.0] {
                let out = resample(&[tone(f, from, n, 0.5)], 48_000, 44_100, chunk);
                let x = &out[0];
                let (a, b) = (x.len() / 4, 3 * x.len() / 4);
                let alias = level_at(x, to, to - f, a, b);
                let db = 20.0 * (alias / 0.5).log10();
                assert!(
                    db < AMOUNT,
                    "{chunk:?} {f} Hz: alias at {} Hz is {db:.1} dB",
                    to - f
                );
            }

            let inband = resample(&[tone(15_000.0, from, n, 0.5)], 48_000, 44_100, chunk);
            let x = &inband[0];
            let (a, b) = (x.len() / 4, 3 * x.len() / 4);
            let kept = 20.0 * (level_at(x, to, 15_000.0, a, b) / 0.5).log10();
            assert!(
                kept.abs() < 0.01,
                "{chunk:?}: 15 kHz moved by {kept:+.4} dB"
            );
        }
    }

    /// The length is exactly `round(input * ratio)` for every input length, and
    /// the latency is compensated to the frame: an impulse at input frame 1000
    /// lands on output frame 1088 (1000 x 48/44.1 = 1088.4) at every chunk
    /// size, and the filter's `output_delay` (320 frames at the default chunk,
    /// 44.1 -> 48 kHz) is what is dropped from the head.
    ///
    /// Mutation (run): do not drop the warm-up (`skip: 0`) → the impulse lands
    /// at 1408 and the first assertion that reads it fails; round the budget
    /// down (`.floor()`) → n = 4410 gives 4799 frames, not 4800.
    #[test]
    fn length_and_latency_match_the_documented_values() {
        for n in [1usize, 441, 1000, 4410, 10_007, 44_100] {
            let out = resample(&[vec![0.1f32; n]], 44_100, 48_000, ChunkSize::N1024);
            assert_eq!(
                out[0].len(),
                (n as f64 * 48_000.0 / 44_100.0).round() as usize,
                "n = {n}"
            );
        }
        let r = FftResampler::new(44_100, 48_000, 1024, 2, 1).unwrap();
        assert_eq!(r.output_delay(), 320);
        for q in ChunkSize::PRESETS {
            let mut plane = vec![0.0f32; 8192];
            plane[1000] = 1.0;
            let out = resample(&[plane], 44_100, 48_000, q);
            let peak = out[0]
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
                .unwrap()
                .0;
            assert_eq!(peak, 1088, "{q:?}");
        }
    }

    /// Upsampling filters at the input's band edge, not the output's. The
    /// low-pass is designed to be 6 dB down (gain 0.5) at its relative cutoff
    /// `cutoff(fft_size_in)`: 0.97679 of the input Nyquist for the 588-frame
    /// blocks of 44.1 -> 48 kHz (21 538 Hz), 0.97337 for the 512-frame blocks
    /// of 48 -> 96 kHz (23 361 Hz). A tone at that edge reads 0.5 x its
    /// amplitude within 0.5 dB, and one at 0.995 Nyquist, in the stopband above
    /// it, is at least 60 dB down, so nothing near the input's Nyquist reaches
    /// the output to image against it (the output spectrum above the input
    /// Nyquist is zero by construction).
    ///
    /// Mutations (run): `cutoff(fft_size_in)` -> `cutoff(fft_size_out)` in
    /// `Unit::new` moves the 44.1 -> 48 kHz edge to 0.97867 of Nyquist, so its
    /// edge tone reads -3.87 dB instead of -6.02 and this fails (the 48 -> 96
    /// kHz edge moves further, to 0.98664, but the loop stops at the first
    /// failure); multiplying each bin by `1 / (2 * fft_size_in)` alone, with
    /// no `filter_f`, reads -0.03 dB at that edge and fails too.
    #[test]
    fn upsampling_cuts_at_the_input_band_edge() {
        for (from, to, edge) in [
            (44_100.0, 48_000.0, 21_538.0),
            (48_000.0, 96_000.0, 23_361.0),
        ] {
            let at = |f: f64| {
                let out = resample(
                    &[tone(f, from, from as usize, 0.5)],
                    from as u32,
                    to as u32,
                    ChunkSize::N1024,
                );
                let x = &out[0];
                let (a, b) = (x.len() / 4, 3 * x.len() / 4);
                20.0 * (level_at(x, to, f, a, b) / 0.5).log10()
            };
            let edge_db = at(edge);
            assert!(
                (edge_db + 6.02).abs() < 0.5,
                "{from}->{to}: {edge} Hz reads {edge_db:+.2} dB, expected -6.02"
            );
            let stop_db = at(0.995 * from / 2.0);
            assert!(
                stop_db < -60.0,
                "{from}->{to}: 0.995 Nyquist reads {stop_db:.1} dB"
            );
        }
    }

    /// A rate the converter cannot take is an error that names the rate, not a
    /// saturated or wrapped integer: NaN, infinities, zero, negatives and
    /// anything under 0.5 Hz (which rounds to 0) or at 2^32 Hz and over. A
    /// pair whose blocks would be enormous (1 Hz -> 192 kHz plans a ~98 M-point
    /// FFT) is refused too.
    ///
    /// Mutations (run): `whole_hz` returning `Ok(hz.round() as usize)` makes
    /// NaN, negatives and sub-0.5 Hz rates reach the converter as 0 and
    /// infinity as `usize::MAX`; the converter still refuses them, but with its
    /// own message, so the message assertions fail. Removing the block cap
    /// (see `fft_resample`) makes the last case allocate gigabytes.
    #[test]
    fn an_unusable_rate_is_an_error() {
        let new = |from: f64, to: f64| {
            Resampler::new(1, SampleRate(from), SampleRate(to), ChunkSize::N1024)
        };
        for bad in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -48_000.0,
            0.4,
            1e300,
            4_294_967_296.0,
        ] {
            for (from, to) in [(bad, 48_000.0), (44_100.0, bad)] {
                let err = new(from, to).err().unwrap_or_else(|| {
                    panic!("{from} -> {to} Hz was accepted");
                });
                assert!(
                    err.to_string().contains("invalid sample rate"),
                    "{from} -> {to}: {err}"
                );
            }
        }
        assert!(new(44_100.0, 48_000.0).is_ok());
        assert!(new(0.5, 1.0).is_ok());
        let err = new(1.0, 192_000.0).err().unwrap();
        assert!(err.to_string().contains("block"), "{err}");
    }

    #[test]
    fn channels_stay_aligned() {
        let n = 4410;
        let planes: Vec<Vec<f32>> = (0..4).map(|c| vec![c as f32 * 0.1; n]).collect();
        let out = resample(&planes, 44100, 48000, ChunkSize::N1024);
        assert_eq!(out.len(), 4);
        let len = out[0].len();
        assert!(out.iter().all(|p| p.len() == len));
    }
}
