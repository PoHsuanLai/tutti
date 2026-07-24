//! Streamable mastering as an [`AudioOut`] decorator.
//!
//! [`DitherOut`] wraps an inner [`AudioOut`] and applies dither to each block on
//! its way through. Dither carries state across blocks (its RNG), so it is a
//! per-block *decorator*, not a whole-signal pass — a streaming render never
//! materializes the whole signal.
//!
//! Mono downmix is deliberately NOT here: channel count is the encoder's
//! concern (it needs it for the file header anyway), so each encoder folds
//! `[f32; 2]` frames to mono from its `ChannelLayout`. Every stage upstream of the
//! encoder speaks plain stereo `[f32; 2]`.

use crate::options::{BitDepth, Dither};
use crate::process::{apply_dither, DitherState};
use tutti_core::io::AudioOut;

/// An [`AudioOut`] decorator that dithers each block before forwarding it to the
/// wrapped sink. Owns the dither state so the noise sequence is continuous
/// across block boundaries. [`Dither::Off`] forwards untouched.
pub(crate) struct DitherOut<S: AudioOut> {
    inner: S,
    state: DitherState,
    bits: u16,
    dither: Dither,
    // Reused planar staging so `apply_dither` keeps its `&mut [f32]` shape and
    // the per-block dither allocates nothing.
    left: Vec<f32>,
    right: Vec<f32>,
    out: Vec<[f32; 2]>,
}

impl<S: AudioOut> DitherOut<S> {
    pub(crate) fn new(inner: S, dither: Dither, bit_depth: BitDepth) -> Self {
        Self {
            inner,
            state: DitherState::new(dither),
            bits: bit_depth.bits(),
            dither,
            left: Vec::new(),
            right: Vec::new(),
            out: Vec::new(),
        }
    }
}

impl<S: AudioOut> AudioOut for DitherOut<S> {
    fn write(&mut self, frames: &[[f32; 2]]) {
        if matches!(self.dither, Dither::Off) {
            self.inner.write(frames);
            return;
        }

        self.left.clear();
        self.right.clear();
        for &[l, r] in frames {
            self.left.push(l);
            self.right.push(r);
        }

        apply_dither(&mut self.left, &mut self.right, self.bits, &mut self.state);

        self.out.clear();
        self.out
            .extend(self.left.iter().zip(&self.right).map(|(&l, &r)| [l, r]));
        self.inner.write(&self.out);
    }

    fn finalize(self) -> std::io::Result<()> {
        self.inner.finalize()
    }
}
