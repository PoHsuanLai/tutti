//! Sample-rate conversion (rubato), as a streaming stage.
//!
//! rubato is a *block* resampler — `input_frames_next()` / `process()` in a
//! loop — so this never needs the whole signal, which is why nothing in this
//! crate buffers for it.
//!
//! # The delay
//!
//! An FFT resampler has latency: `output_delay()` frames of its output are the
//! filter warming up, not signal. The previous version never called it, so every
//! resampled export was shifted late by that much (measured: 320 frames at
//! 44.1→48 k, ~6.7 ms of leading silence) and lost the same amount off the tail,
//! where a blind `truncate()` to the expected length cut exactly where the
//! delayed content would have been. [`Resampler`] drops those frames at the head
//! and flushes with silence at the end to push the real tail out.

use crate::error::Result;
use tutti_types::{SampleRate, Samples};

/// Input frames per FFT chunk, and how finely each chunk is subdivided.
///
/// Named for the quantity, not for a ranking. A `Fast | Medium | High | Best`
/// enum said which end of a scale a caller was on and nothing about what
/// changed between two of them — so the trade could not be reasoned about
/// (longer chunks are a steeper anti-alias filter and more latency; more
/// sub-chunks is finer time resolution and more work) and a point the enum did
/// not list could not be expressed at all.
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
    /// resampler whose latency is not the one that was asked for, and rubato's
    /// FFT requires a power of two.
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
    pub const fn chunk(self) -> Samples {
        self.chunk
    }

    /// Sub-chunks each chunk is split into.
    pub const fn sub_chunks(self) -> usize {
        self.sub_chunks
    }
}

impl Default for ChunkSize {
    fn default() -> Self {
        Self::N1024
    }
}

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use streaming::Resampler;

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
mod streaming {
    use super::*;
    use rubato::{FftFixedIn, Resampler as _};

    /// A rate converter fed one block at a time.
    ///
    /// Owns its own input carry, so callers hand it whatever block size they
    /// have and it emits output whenever rubato has a chunk ready.
    pub(crate) struct Resampler {
        inner: FftFixedIn<f32>,
        channels: usize,
        ratio: f64,
        /// Input frames not yet consumed, per channel.
        carry: Vec<Vec<f32>>,
        /// Output frames still to be discarded as filter warm-up.
        skip: usize,
        /// Real input frames fed so far (excludes the flush padding), which is
        /// what sets how much output is signal rather than filter tail.
        fed: usize,
        /// Output frames emitted so far, so the budget is a running total.
        emitted: usize,
        /// Reusable scratch so the per-block path does not allocate.
        in_scratch: Vec<Vec<f32>>,
    }

    impl Resampler {
        /// Both rates are [`SampleRate`], which every caller already holds —
        /// they used to narrow to `u32` at the call and this widened straight
        /// back to divide. The narrowing now happens once, here, at rubato's
        /// boundary, which is the only place it is owed.
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
            let inner = FftFixedIn::<f32>::new(
                source_hz.round() as usize,
                target_hz.round() as usize,
                chunk.chunk().get(),
                chunk.sub_chunks(),
                channels,
            )?;
            let skip = inner.output_delay();
            Ok(Self {
                inner,
                channels,
                ratio: target_hz / source_hz,
                carry: vec![Vec::new(); channels],
                skip,
                fed: 0,
                emitted: 0,
                in_scratch: vec![Vec::new(); channels],
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
        /// signal — still inside rubato — into `out`. The budget in
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

        fn drain(&mut self, out: &mut [Vec<f32>], flushing: bool) -> Result<()> {
            loop {
                let need = self.inner.input_frames_next();
                let have = self.carry.first().map_or(0, |c| c.len());
                if have < need {
                    if !flushing {
                        return Ok(());
                    }
                    // Pad the last partial chunk so rubato can emit it.
                    if have == 0 {
                        return Ok(());
                    }
                    for c in self.carry.iter_mut() {
                        c.resize(need, 0.0);
                    }
                }

                for (c, scratch) in self.in_scratch.iter_mut().enumerate() {
                    scratch.clear();
                    scratch.extend_from_slice(&self.carry[c][..need]);
                }
                let produced = self.inner.process(&self.in_scratch, None)?;
                for c in self.carry.iter_mut() {
                    c.drain(..need);
                }

                // Drop warm-up frames at the head before anything is kept.
                let produced_len = produced.first().map_or(0, |p| p.len());
                let drop = self.skip.min(produced_len);
                self.skip -= drop;
                // Emit at most what is still owed, so the flush padding that
                // pushed the tail out does not itself become output.
                let take = (produced_len - drop).min(self.budget());
                for (c, p) in produced.iter().enumerate().take(out.len()) {
                    out[c].extend_from_slice(&p[drop..drop + take]);
                }
                self.emitted += take;

                if flushing && self.carry.first().is_none_or(|c| c.is_empty()) {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
mod tests {
    use super::*;

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

    /// The regression: an impulse must come out where the rate change puts it,
    /// not `output_delay()` frames later. The old code never called
    /// `output_delay`, so this landed 320 frames late at 44.1→48 k.
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

    /// …and the tail must survive. A marker on the final input frame used to
    /// vanish entirely, because the blind truncate cut where the delayed tail
    /// would have been.
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

/// Convert a whole [`Rendered`](crate::Rendered) to `opts.target_rate`.
///
/// The streaming [`Resampler`] is the right shape for an encode, which pulls the
/// graph a block at a time. This is for the two-pass path, which already holds
/// the signal whole and must convert it *before* measuring — sample-rate
/// conversion moves the true peak, so a gain measured at the render rate would
/// miss its target once the file is written at another.
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
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
