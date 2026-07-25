//! Streamable mastering as an [`AudioOut`] decorator.
//!
//! [`DitherOut`] wraps an inner [`AudioOut<f32, CH>`] and applies dither to each
//! block on its way through. Dither carries state across blocks (its RNG), so it
//! is a per-block *decorator*, not a whole-signal pass — a streaming render never
//! materializes the whole signal.
//!
//! Up/downmix is deliberately NOT here: it happens once at the render→frame
//! boundary (`NetSource` folds the graph to the file width `CH`), so every stage
//! from here to the encoder already speaks plain `[f32; CH]` frames at the final
//! width — dither just processes them.

use crate::options::{BitDepth, Dither};
use crate::process::{apply_dither, DitherState};
use tutti_core::io::AudioOut;

/// An [`AudioOut<f32, CH>`] decorator that dithers each block before forwarding
/// it to the wrapped sink. Owns the dither state so the noise sequence is
/// continuous across block boundaries. [`Dither::Off`] forwards untouched.
/// Generic over the frame width `CH`: each channel plane is dithered in turn
/// from the shared RNG.
pub(crate) struct DitherOut<S: AudioOut<f32, CH>, const CH: usize> {
    inner: S,
    state: DitherState,
    bits: u16,
    dither: Dither,
    // Reused planar staging so `apply_dither` keeps its `&mut [f32]` shape and
    // the per-block dither allocates nothing.
    planes: [Vec<f32>; CH],
    out: Vec<[f32; CH]>,
}

impl<S: AudioOut<f32, CH>, const CH: usize> DitherOut<S, CH> {
    pub(crate) fn new(inner: S, dither: Dither, bit_depth: BitDepth) -> Self {
        Self {
            inner,
            state: DitherState::new(dither),
            bits: bit_depth.bits(),
            dither,
            planes: std::array::from_fn(|_| Vec::new()),
            out: Vec::new(),
        }
    }
}

impl<S: AudioOut<f32, CH>, const CH: usize> AudioOut<f32, CH> for DitherOut<S, CH> {
    fn write(&mut self, frames: &[[f32; CH]]) {
        if matches!(self.dither, Dither::Off) {
            self.inner.write(frames);
            return;
        }

        // Deinterleave into per-channel planes, dither each, reinterleave.
        for plane in self.planes.iter_mut() {
            plane.clear();
            plane.reserve(frames.len());
        }
        for frame in frames {
            for (plane, &s) in self.planes.iter_mut().zip(frame.iter()) {
                plane.push(s);
            }
        }

        for plane in self.planes.iter_mut() {
            apply_dither(plane, self.bits, &mut self.state);
        }

        self.out.clear();
        self.out.reserve(frames.len());
        for i in 0..frames.len() {
            self.out.push(std::array::from_fn(|ch| self.planes[ch][i]));
        }
        self.inner.write(&self.out);
    }

    fn finalize(self) -> std::io::Result<()> {
        self.inner.finalize()
    }
}
