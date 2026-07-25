//! Block consumers for the render stage, speaking the cold-path [`AudioOut`]
//! vocabulary.
//!
//! The driver gates each block through a [`BlockCursor`] (latency-trim +
//! output-length cap) *before* handing it to a sink, so the gate stays off the
//! sink trait: a sink just accepts the frames it is given. Three sinks cover the
//! current needs — [`RenderOut`] collects frames into `CH` `Vec<f32>` planes
//! (read back with `into_planes`, since [`AudioOut::finalize`] returns `()` not
//! data), [`EncoderOut`] forwards each block into a streaming encoder, and
//! [`BufferingOut`] collects the whole signal to master it at finalize.
//!
//! All three are generic over the frame width `CH`, matching the driver: a
//! stereo export instantiates them at `CH = 2`, a surround render at `4`/`6`/`8`.
//! [`BlockCursor`] is width-agnostic — it windows by sample index, never looking
//! inside a frame — so it stays a plain struct.

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

/// Collects gated frames into `CH` deinterleaved `Vec<f32>` planes.
pub(crate) struct RenderOut<const CH: usize> {
    planes: [Vec<f32>; CH],
}

impl<const CH: usize> RenderOut<CH> {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            planes: std::array::from_fn(|_| Vec::with_capacity(capacity)),
        }
    }

    /// The collected per-channel planes, in channel order. The buffer terminal
    /// reads these back (a mono/stereo export takes 1/2 of them).
    pub fn into_planes(self) -> [Vec<f32>; CH] {
        self.planes
    }
}

impl<const CH: usize> AudioOut<f32, CH> for RenderOut<CH> {
    fn write(&mut self, frames: &[[f32; CH]]) {
        for plane in self.planes.iter_mut() {
            plane.reserve(frames.len());
        }
        for frame in frames {
            for (plane, &s) in self.planes.iter_mut().zip(frame.iter()) {
                plane.push(s);
            }
        }
    }

    fn finalize(self) -> io::Result<()> {
        // Buffered output is read back via `into_planes`, not through finalize.
        Ok(())
    }
}

/// An [`AudioOut`] that pushes each block of `CH`-wide frames into a streaming
/// encoder. It flattens each `[f32; CH]` block to an interleaved `&[f32]` at the
/// encoder seam — the encoder speaks a runtime channel count
/// ([`ChannelLayout`](tutti_types::ChannelLayout)), matching the hound/vorbis/
/// flac APIs, so codecs are not monomorphized per width. Any encoder error is
/// stashed and surfaced at [`finalize`](AudioOut::finalize), per the trait's
/// deferred-error contract.
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) struct EncoderOut<const CH: usize> {
    encoder: Box<dyn StreamingEncoder>,
    /// Reused interleave scratch so each block's flatten allocates at most once.
    interleaved: Vec<f32>,
    deferred: crate::error::Result<()>,
}

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
impl<const CH: usize> EncoderOut<CH> {
    pub(crate) fn new(encoder: Box<dyn StreamingEncoder>) -> Self {
        Self {
            encoder,
            interleaved: Vec::new(),
            deferred: Ok(()),
        }
    }
}

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
impl<const CH: usize> AudioOut<f32, CH> for EncoderOut<CH> {
    fn write(&mut self, frames: &[[f32; CH]]) {
        if self.deferred.is_err() || frames.is_empty() {
            return;
        }
        // Flatten `[f32; CH]` frames into the interleaved buffer the encoder
        // wants. `[[f32; CH]]` is already interleaved in memory, but we copy via
        // a reused Vec to hand the encoder an owned `&[f32]` and keep the frame
        // type off its trait.
        self.interleaved.clear();
        self.interleaved.reserve(frames.len() * CH);
        for frame in frames {
            self.interleaved.extend_from_slice(frame);
        }
        if let Err(e) = self.encoder.write_interleaved(&self.interleaved, CH as u16) {
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
pub(crate) struct BufferingOut<S: AudioOut<f32, CH>, const CH: usize> {
    inner: S,
    planes: [Vec<f32>; CH],
    mastering: Mastering,
}

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
impl<S: AudioOut<f32, CH>, const CH: usize> BufferingOut<S, CH> {
    pub(crate) fn new(inner: S, mastering: Mastering) -> Self {
        Self {
            inner,
            planes: std::array::from_fn(|_| Vec::new()),
            mastering,
        }
    }

    /// Master the collected signal and push the finished frames into the inner
    /// sink. Split out so `finalize` can `?` on it.
    fn master_into_inner(&mut self) -> crate::error::Result<()> {
        let planes = std::array::from_fn(|i| std::mem::take(&mut self.planes[i]));
        let (frames, _rate) = master_collected::<CH>(planes, &self.mastering)?;
        self.inner.write(&frames);
        Ok(())
    }
}

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
impl<S: AudioOut<f32, CH>, const CH: usize> AudioOut<f32, CH> for BufferingOut<S, CH> {
    fn write(&mut self, frames: &[[f32; CH]]) {
        // Collect raw; no mastering yet (resample/normalize need the whole
        // signal, which we don't have until finalize).
        for plane in self.planes.iter_mut() {
            plane.reserve(frames.len());
        }
        for frame in frames {
            for (plane, &s) in self.planes.iter_mut().zip(frame.iter()) {
                plane.push(s);
            }
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
        let mut sink = RenderOut::<2>::with_capacity(16);
        let block = frames(&[1.0; 8], &[2.0; 8]);
        sink.write(&block);
        sink.write(&block);
        let [l, r] = sink.into_planes();
        assert_eq!(l.len(), 16);
        assert_eq!(r.len(), 16);
        assert!(l.iter().all(|&x| x == 1.0));
        assert!(r.iter().all(|&x| x == 2.0));
    }

    #[test]
    fn render_out_collects_quad_frames() {
        let mut sink = RenderOut::<4>::with_capacity(4);
        sink.write(&[[1.0, 2.0, 3.0, 4.0], [5.0, 6.0, 7.0, 8.0]]);
        let [a, b, c, d] = sink.into_planes();
        assert_eq!(a, vec![1.0, 5.0]);
        assert_eq!(b, vec![2.0, 6.0]);
        assert_eq!(c, vec![3.0, 7.0]);
        assert_eq!(d, vec![4.0, 8.0]);
    }
}
