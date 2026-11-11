//! [`DitherSink`]: the dither noise as a [`Sink`] adapter, the one path an
//! export and a live recording share.

use tutti_types::{Amplitude, ChannelLayout, Samples, Sink, SinkFailed};

use crate::dither::{DitherNoise, DitherShape};

/// Frames dithered and handed on at a time: the adapter's whole scratch.
///
/// A longer write goes through in pieces of this size. The noise advances once
/// per sample in order, so the split changes nothing in what is written.
const CHUNK: Samples = Samples(1024);

/// What to add: the noise's shape and the size of one least significant bit of
/// the depth the wrapped sink will quantize to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DitherSpec {
    /// The probability density of the noise.
    pub shape: DitherShape,
    /// One LSB of the target depth in the `[-1, 1]` float domain: `2^-(bits-1)`
    /// (`IntDepth::lsb` in `tutti-codec`). The depth is the caller's to name:
    /// a sink of floats has no LSB and takes no dither.
    pub lsb: Amplitude,
}

/// A [`Sink`] that adds dither noise to every block before it reaches the sink
/// it wraps.
///
/// It computes no noise of its own: [`DitherNoise`] does, seeded as an
/// export's is, one draw per sample in the order the samples arrive, so the
/// sequence is continuous across writes and across channels. Wrap the file
/// sink of a recording (`FileOut`) and a live 16-bit take carries the dither
/// an export of the same signal does; leave it unwrapped and the take is
/// quantized as it always was.
///
/// With `None` for the [`DitherSpec`] the adapter is the identity: the block
/// goes to the inner sink as it came, uncopied. Frames in are frames out, the
/// layout is the inner sink's, a write the inner sink refuses is refused
/// here ([`SinkFailed`]), and [`finalize`](Sink::finalize) is the inner
/// sink's. A trailing partial frame is ignored, as every sink does.
///
/// Nothing allocates after [`new`](Self::new): the scratch is one chunk of
/// 1024 frames, sized there.
///
/// # Examples
///
/// ```
/// use tutti_dsp::{DitherShape, DitherSink, DitherSpec};
/// use tutti_types::{Amplitude, ChannelLayout, Sink, SinkFailed};
///
/// struct Count(usize);
/// impl Sink for Count {
///     fn layout(&self) -> ChannelLayout { ChannelLayout::MONO }
///     fn write(&mut self, block: &[f32]) -> Result<(), SinkFailed> {
///         self.0 += block.len();
///         Ok(())
///     }
///     fn finalize(self) -> std::io::Result<()> { Ok(()) }
/// }
///
/// let lsb = Amplitude(1.0 / 32768.0); // 16 bit
/// let spec = DitherSpec { shape: DitherShape::Triangular, lsb };
/// let mut sink = DitherSink::new(Count(0), Some(spec));
/// sink.write(&[0.0; 64]).unwrap();
/// assert_eq!(sink.into_inner().0, 64);
/// ```
#[derive(Debug)]
pub struct DitherSink<S> {
    inner: S,
    noise: DitherNoise,
    spec: Option<DitherSpec>,
    scratch: Vec<f32>,
}

impl<S: Sink> DitherSink<S> {
    /// Wraps `inner`, dithering with `spec`, or passing through with `None`.
    pub fn new(inner: S, spec: Option<DitherSpec>) -> Self {
        let scratch = match spec {
            Some(_) => Vec::with_capacity(CHUNK.interleaved_len(inner.layout())),
            None => Vec::new(),
        };
        Self {
            inner,
            noise: DitherNoise::new(),
            spec,
            scratch,
        }
    }

    /// The wrapped sink, for a caller that finishes it itself.
    pub fn into_inner(self) -> S {
        self.inner
    }

    /// The wrapped sink, to read what it holds between writes.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.inner
    }

    /// Whether a write reaches the inner sink as it came.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        self.spec.is_none()
    }
}

impl<S: Sink> Sink for DitherSink<S> {
    fn layout(&self) -> ChannelLayout {
        self.inner.layout()
    }

    fn write(&mut self, interleaved: &[f32]) -> Result<(), SinkFailed> {
        let Some(spec) = self.spec else {
            return self.inner.write(interleaved);
        };
        let layout = self.inner.layout();
        let whole =
            Samples::from_interleaved_len(interleaved.len(), layout).interleaved_len(layout);
        let step = CHUNK.interleaved_len(layout);
        if step == 0 {
            return Ok(());
        }
        for chunk in interleaved[..whole].chunks(step) {
            self.scratch.clear();
            self.scratch.extend_from_slice(chunk);
            self.noise.apply(spec.shape, spec.lsb, &mut self.scratch);
            self.inner.write(&self.scratch)?;
        }
        Ok(())
    }

    fn finalize(self) -> std::io::Result<()> {
        self.inner.finalize()
    }
}

#[cfg(test)]
mod tests;
