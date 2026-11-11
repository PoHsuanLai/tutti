//! Sample-rate conversion for the two-pass path.
//!
//! The converter and its bookkeeping (the delay, the flush, the output budget)
//! are `tutti_dsp::StreamingResampler`; this file is the two-pass call site.

use crate::control::{Control, Outcome, Stage};
use tutti_dsp::StreamingResampler;
use tutti_types::Samples;

/// Convert a whole [`Rendered`](crate::Rendered) to `opts.target_rate`.
///
/// The streaming `StreamingResampler` is the right shape for an encode, which pulls the
/// graph a block at a time. This is for the two-pass path, which already holds
/// the signal whole and must convert it *before* measuring — sample-rate
/// conversion moves the true peak, so a gain measured at the render rate would
/// miss its target once the file is written at another.
///
/// The signal goes in chunks of at most one second, so `control` is checked
/// between them and before the converter is flushed, and each chunk is a
/// reading of [`Stage::Measuring`] (`total` frames in all, this conversion
/// the first `rendered.frames()` of them). A cancelled conversion is
/// [`Outcome::Cancelled`].
// Not codec-gated: this converts buffers through the FFT resampler, which is
// unconditional, and its caller `render_normalized_to_file` is ungated too. Gating
// it would make a no-codec build fail to compile rather than merely fail to
// encode.
pub(crate) fn resample_rendered(
    rendered: &crate::Rendered,
    opts: crate::config::Resample,
    total: Samples,
    control: &mut Control<'_>,
) -> crate::Result<Outcome<crate::Rendered>> {
    let channels = rendered.channels();
    let mut rs = StreamingResampler::new(
        channels,
        rendered.sample_rate(),
        opts.target_rate,
        opts.chunk,
    )?;
    let mut out: Vec<Vec<f32>> = vec![Vec::new(); channels];
    let frames = rendered.frames().get();
    let chunk = crate::normalize::chunk_frames(rendered.sample_rate());
    let mut planes: Vec<Vec<f32>> = vec![Vec::new(); channels];
    let mut at = 0;
    while at < frames {
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let end = (at + chunk).min(frames);
        for (dst, src) in planes.iter_mut().zip(rendered.planes()) {
            dst.clear();
            dst.extend_from_slice(&src[at..end]);
        }
        rs.push(&planes, &mut out)?;
        at = end;
        control.advance(Stage::Measuring, Samples(at), total);
    }
    if control.is_cancelled() {
        return Ok(Outcome::Cancelled);
    }
    rs.finish(&mut out)?;

    Ok(Outcome::Finished(crate::Rendered::from_render(
        out,
        opts.target_rate,
        rendered.dropped_events(),
    )))
}
