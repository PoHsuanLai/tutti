//! Block consumers for the render stage, speaking the cold-path [`AudioOut`]
//! vocabulary.
//!
//! The driver gates each block through a [`BlockCursor`] (latency-trim +
//! output-length cap) *before* handing it to a sink, so the gate stays off the
//! sink trait: a sink just accepts the frames it is given. Three sinks cover the
//! current needs — [`RenderOut`] collects frames into two `Vec<f32>` planes
//! (read back with `into_stereo`, since [`AudioOut::finalize`] returns `()` not
//! data), [`EncoderOut`] forwards each block into a streaming encoder, and
//! [`BufferingOut`] collects the whole signal to master it at finalize.

use std::io;
use std::ops::Range;
use tutti_core::io::AudioOut;

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
use crate::encode::sink::StreamingEncoder;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
use crate::process::{master_collected, Mastering};

/// Per-block positioning handed from the driver to its gate. Encapsulates the
/// latency-gate + output-length-cap arithmetic. It is applied *before* the
/// sink sees a block, so it never rides on the [`AudioOut`] trait.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BlockCursor {
    /// Absolute sample index of the block's first sample (index 0 in the
    /// slices the gate receives).
    pub block_start_sample: usize,
    /// Leading samples that should be dropped (look-ahead latency).
    pub latency_samples: usize,
    /// How many samples have *already* been kept by earlier blocks.
    pub samples_kept_so_far: usize,
    /// Total samples the sink is allowed to keep.
    pub output_length: usize,
}

impl BlockCursor {
    /// Window into the block the sink should consume. Returns `0..0` when the
    /// block lies entirely before the latency gate or entirely past the
    /// output-length cap.
    pub fn window(&self, block_len: usize) -> Range<usize> {
        let start = self.latency_samples.saturating_sub(self.block_start_sample);
        if start >= block_len {
            return 0..0;
        }
        let remaining = self.output_length.saturating_sub(self.samples_kept_so_far);
        if remaining == 0 {
            return 0..0;
        }
        let end = block_len.min(start + remaining);
        start..end
    }
}

/// Collects gated stereo frames into two `Vec<f32>` planes.
pub(crate) struct RenderOut {
    left: Vec<f32>,
    right: Vec<f32>,
}

impl RenderOut {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            left: Vec::with_capacity(capacity),
            right: Vec::with_capacity(capacity),
        }
    }

    pub fn into_stereo(self) -> (Vec<f32>, Vec<f32>) {
        (self.left, self.right)
    }
}

impl AudioOut for RenderOut {
    fn write(&mut self, frames: &[[f32; 2]]) {
        self.left.reserve(frames.len());
        self.right.reserve(frames.len());
        for &[l, r] in frames {
            self.left.push(l);
            self.right.push(r);
        }
    }

    fn finalize(self) -> io::Result<()> {
        // Buffered output is read back via `into_stereo`, not through finalize.
        Ok(())
    }
}

/// An [`AudioOut`] that pushes each block of stereo frames into a streaming
/// encoder. Any encoder error is stashed and surfaced at
/// [`finalize`](AudioOut::finalize), per the trait's deferred-error contract.
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) struct EncoderOut {
    encoder: Box<dyn StreamingEncoder>,
    deferred: crate::error::Result<()>,
}

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
impl EncoderOut {
    pub(crate) fn new(encoder: Box<dyn StreamingEncoder>) -> Self {
        Self {
            encoder,
            deferred: Ok(()),
        }
    }
}

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
impl AudioOut for EncoderOut {
    fn write(&mut self, frames: &[[f32; 2]]) {
        if self.deferred.is_err() || frames.is_empty() {
            return;
        }
        if let Err(e) = self.encoder.write_frames(frames) {
            self.deferred = Err(e);
        }
    }

    fn finalize(self) -> io::Result<()> {
        self.deferred.map_err(|e| io::Error::other(e.to_string()))?;
        self.encoder
            .finalize()
            .map_err(|e| io::Error::other(e.to_string()))
    }
}

/// An [`AudioOut`] adapter that realizes whole-signal mastering: it collects
/// every raw block, then at [`finalize`](AudioOut::finalize) masters the whole
/// signal in one shot (resample → normalize → dither, via
/// [`master_collected`]) and writes the finished frames into the wrapped sink
/// before finalizing it.
///
/// This is the whole "buffered export" concept — not a mode, just a sink that
/// holds its input until it has enough to finish. The exporter wraps a sink in
/// this only when [`Mastering::needs_whole_signal`]; a streaming export never
/// allocates it and dithers per block via a
/// [`DitherOut`](crate::process::DitherOut) instead.
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) struct BufferingOut<S: AudioOut> {
    inner: S,
    left: Vec<f32>,
    right: Vec<f32>,
    mastering: Mastering,
}

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
impl<S: AudioOut> BufferingOut<S> {
    pub(crate) fn new(inner: S, mastering: Mastering) -> Self {
        Self {
            inner,
            left: Vec::new(),
            right: Vec::new(),
            mastering,
        }
    }

    /// Master the collected signal and push the finished frames into the inner
    /// sink. Split out so `finalize` can `?` on it.
    fn master_into_inner(&mut self) -> crate::error::Result<()> {
        let (frames, _rate) = master_collected(
            std::mem::take(&mut self.left),
            std::mem::take(&mut self.right),
            &self.mastering,
        )?;
        self.inner.write(&frames);
        Ok(())
    }
}

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
impl<S: AudioOut> AudioOut for BufferingOut<S> {
    fn write(&mut self, frames: &[[f32; 2]]) {
        // Collect raw; no mastering yet (resample/normalize need the whole
        // signal, which we don't have until finalize).
        self.left.reserve(frames.len());
        self.right.reserve(frames.len());
        for &[l, r] in frames {
            self.left.push(l);
            self.right.push(r);
        }
    }

    fn finalize(mut self) -> io::Result<()> {
        if let Err(e) = self.master_into_inner() {
            return Err(io::Error::other(e.to_string()));
        }
        self.inner.finalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(start: usize, latency: usize, kept: usize, output_length: usize) -> BlockCursor {
        BlockCursor {
            block_start_sample: start,
            latency_samples: latency,
            samples_kept_so_far: kept,
            output_length,
        }
    }

    /// Interleave two planar slices into stereo frames, mirroring the driver's
    /// pre-sink packing so these tests exercise the same input shape.
    fn frames(left: &[f32], right: &[f32]) -> Vec<[f32; 2]> {
        left.iter().zip(right).map(|(&l, &r)| [l, r]).collect()
    }

    #[test]
    fn window_drops_block_before_latency_gate() {
        let c = cursor(0, 100, 0, 1000);
        assert_eq!(c.window(64), 0..0);
    }

    #[test]
    fn window_partial_overlap_with_latency_gate() {
        let c = cursor(80, 100, 0, 1000);
        // block 80..144, latency gate at 100 → keep indices 20..64
        assert_eq!(c.window(64), 20..64);
    }

    #[test]
    fn window_past_latency_full_block() {
        let c = cursor(200, 100, 100, 1000);
        assert_eq!(c.window(64), 0..64);
    }

    #[test]
    fn window_caps_at_output_length() {
        // already kept 990, output_length 1000 → at most 10 more
        let c = cursor(200, 100, 990, 1000);
        assert_eq!(c.window(64), 0..10);
    }

    #[test]
    fn window_after_output_length_full() {
        let c = cursor(200, 100, 1000, 1000);
        assert_eq!(c.window(64), 0..0);
    }

    #[test]
    fn render_out_collects_frames() {
        let mut sink = RenderOut::with_capacity(16);
        let block = frames(&[1.0; 8], &[2.0; 8]);
        sink.write(&block);
        sink.write(&block);
        let (l, r) = sink.into_stereo();
        assert_eq!(l.len(), 16);
        assert_eq!(r.len(), 16);
        assert!(l.iter().all(|&x| x == 1.0));
        assert!(r.iter().all(|&x| x == 2.0));
    }
}
