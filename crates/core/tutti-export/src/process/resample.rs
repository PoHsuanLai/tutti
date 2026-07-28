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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ResampleQuality {
    Fast,
    #[default]
    Medium,
    High,
    Best,
}

impl ResampleQuality {
    fn chunk_size(&self) -> usize {
        match self {
            Self::Fast => 512,
            Self::Medium => 1024,
            Self::High => 2048,
            Self::Best => 4096,
        }
    }

    fn sub_chunks(&self) -> usize {
        match self {
            Self::Fast => 1,
            Self::Medium => 2,
            Self::High => 4,
            Self::Best => 8,
        }
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
        pub(crate) fn new(
            channels: usize,
            source_rate: u32,
            target_rate: u32,
            quality: ResampleQuality,
        ) -> Result<Self> {
            let inner = FftFixedIn::<f32>::new(
                source_rate as usize,
                target_rate as usize,
                quality.chunk_size(),
                quality.sub_chunks(),
                channels,
            )?;
            let skip = inner.output_delay();
            Ok(Self {
                inner,
                channels,
                ratio: f64::from(target_rate) / f64::from(source_rate),
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

                if flushing && self.carry.first().map_or(true, |c| c.is_empty()) {
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

    fn resample(planes: &[Vec<f32>], from: u32, to: u32, q: ResampleQuality) -> Vec<Vec<f32>> {
        let mut r = Resampler::new(planes.len(), from, to, q).unwrap();
        let mut out = vec![Vec::new(); planes.len()];
        r.push(planes, &mut out).unwrap();
        r.finish(&mut out).unwrap();
        out
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
            ResampleQuality::Fast,
            ResampleQuality::Medium,
            ResampleQuality::High,
            ResampleQuality::Best,
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
        let out = resample(&[plane], 44100, 48000, ResampleQuality::Medium);
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
        let out = resample(&[vec![0.5f32; n]], 44100, 48000, ResampleQuality::Medium);
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
        let out = resample(&planes, 44100, 48000, ResampleQuality::Medium);
        assert_eq!(out.len(), 4);
        let len = out[0].len();
        assert!(out.iter().all(|p| p.len() == len));
    }
}
