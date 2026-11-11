//! Sample-rate conversion for the two-pass path.
//!
//! The converter and its bookkeeping (the delay, the flush, the output budget)
//! are `tutti_dsp::StreamingResampler`; this file is the two-pass call site.

use tutti_dsp::StreamingResampler;

/// Convert a whole [`Rendered`](crate::Rendered) to `opts.target_rate`.
///
/// The streaming `StreamingResampler` is the right shape for an encode, which pulls the
/// graph a block at a time. This is for the two-pass path, which already holds
/// the signal whole and must convert it *before* measuring — sample-rate
/// conversion moves the true peak, so a gain measured at the render rate would
/// miss its target once the file is written at another.
// Not codec-gated: this converts buffers through the FFT resampler, which is
// unconditional, and its caller `render_normalized_to_file` is ungated too. Gating
// it would make a no-codec build fail to compile rather than merely fail to
// encode.
pub(crate) fn resample_rendered(
    rendered: &crate::Rendered,
    opts: crate::config::Resample,
) -> crate::Result<crate::Rendered> {
    let channels = rendered.channels();
    let mut rs = StreamingResampler::new(
        channels,
        rendered.sample_rate(),
        opts.target_rate,
        opts.chunk,
    )?;
    let mut out: Vec<Vec<f32>> = vec![Vec::new(); channels];
    rs.push(rendered.planes(), &mut out)?;
    rs.finish(&mut out)?;

    Ok(crate::Rendered::from_render(
        out,
        opts.target_rate,
        rendered.dropped_events(),
    ))
}
