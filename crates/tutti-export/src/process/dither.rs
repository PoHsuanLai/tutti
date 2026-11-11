//! The dither a file target calls for; the noise and the sink that adds it are
//! `tutti_dsp`'s ([`DitherSink`](tutti_dsp::DitherSink)).

use crate::output::FileTarget;
use tutti_dsp::DitherSpec;

/// The dither `target` calls for, or `None` when nothing quantizes.
///
/// `Float32` never dithers, and the guard is load-bearing rather than tidy.
/// Dither exists to decorrelate *quantization* error, and a 32-bit float
/// output does not quantize: it has no integer width to take an LSB from.
///
/// Ogg Vorbis never dithers either: its encoder takes the float stream and
/// quantizes nothing, so the noise would only be encoded as signal. Both are
/// a format with no integer width.
pub(crate) fn dither_spec(target: &FileTarget) -> Option<DitherSpec> {
    let shape = target.dither()?;
    let depth = target.format().int_depth()?;
    Some(DitherSpec {
        shape,
        lsb: depth.lsb(),
    })
}

#[cfg(test)]
mod tests;
