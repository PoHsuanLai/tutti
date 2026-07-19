//! Block consumer abstraction for the render stage.
//!
//! [`RenderSink`] is a one-method trait; two impls cover the current needs:
//! [`BufferedSink`] collects blocks into two `Vec<f32>`s, and
//! [`StreamSink`] forwards each gated block to a user-supplied closure (which
//! typically calls into an encoder).
//!
//! [`BlockCursor`] packs the per-block positioning and gating logic so both
//! sinks share one implementation.

use crate::Result;
use std::ops::Range;

/// Per-block positioning handed from the driver to a sink. Encapsulates the
/// latency-gate + output-length-cap arithmetic that both sinks need.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BlockCursor {
    /// Absolute sample index of the block's first sample (index 0 in the
    /// slices the sink receives).
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

/// A consumer of rendered stereo blocks. The driver calls `accept_block`
/// once per processed chunk, passing a fresh [`BlockCursor`] that describes
/// where this block sits in the overall render.
pub(crate) trait RenderSink {
    fn accept_block(&mut self, left: &[f32], right: &[f32], cursor: BlockCursor) -> Result<usize>;
}

/// Collects blocks into two `Vec<f32>`s, dropping samples that fall outside
/// the cursor window.
pub(crate) struct BufferedSink {
    left: Vec<f32>,
    right: Vec<f32>,
}

impl BufferedSink {
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

impl RenderSink for BufferedSink {
    fn accept_block(&mut self, left: &[f32], right: &[f32], cursor: BlockCursor) -> Result<usize> {
        let window = cursor.window(left.len());
        let kept = window.end - window.start;
        if kept > 0 {
            self.left.extend_from_slice(&left[window.clone()]);
            self.right.extend_from_slice(&right[window]);
        }
        Ok(kept)
    }
}

/// Forwards each gated block to an `FnMut` closure. The closure typically
/// pushes the chunk into a streaming encoder.
pub(crate) struct StreamSink<F: FnMut(&[f32], &[f32]) -> Result<()>> {
    f: F,
}

impl<F: FnMut(&[f32], &[f32]) -> Result<()>> StreamSink<F> {
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

impl<F: FnMut(&[f32], &[f32]) -> Result<()>> RenderSink for StreamSink<F> {
    fn accept_block(&mut self, left: &[f32], right: &[f32], cursor: BlockCursor) -> Result<usize> {
        let window = cursor.window(left.len());
        let kept = window.end - window.start;
        if kept > 0 {
            (self.f)(&left[window.clone()], &right[window])?;
        }
        Ok(kept)
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
    fn buffered_sink_collects_windowed_samples() {
        let mut sink = BufferedSink::with_capacity(16);
        let block = vec![1.0_f32; 8];
        let kept = sink
            .accept_block(&block, &block, cursor(0, 0, 0, 16))
            .unwrap();
        assert_eq!(kept, 8);
        let kept = sink
            .accept_block(&block, &block, cursor(8, 0, 8, 16))
            .unwrap();
        assert_eq!(kept, 8);
        let (l, r) = sink.into_stereo();
        assert_eq!(l.len(), 16);
        assert_eq!(r.len(), 16);
    }

    #[test]
    fn stream_sink_forwards_to_closure() {
        let mut captured = Vec::new();
        {
            let mut sink = StreamSink::new(|l: &[f32], _r: &[f32]| {
                captured.extend_from_slice(l);
                Ok(())
            });
            let block = vec![2.0_f32; 8];
            sink.accept_block(&block, &block, cursor(0, 0, 0, 16))
                .unwrap();
        }
        assert_eq!(captured.len(), 8);
    }
}
