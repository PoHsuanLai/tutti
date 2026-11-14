//! The conversions between formats and between layouts, as functions over two
//! views.

use crate::{BufferError, Interleaved, InterleavedMut, Planes, PlanesMut, Sample};

/// Converts every sample of `src` into `dst`'s format by the rule in
/// [`Sample`].
///
/// ```
/// use tutti_buffers::{convert, Interleaved, InterleavedMut};
/// use tutti_types::ChannelLayout;
///
/// let src = [0.5f32, -1.0, 2.0, f32::NAN];
/// let mut dst = [0i16; 4];
/// convert(
///     Interleaved::new(&src, ChannelLayout::STEREO)?,
///     InterleavedMut::new(&mut dst, ChannelLayout::STEREO)?,
/// )?;
/// assert_eq!(dst, [16384, -32768, 32767, 0]);
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
///
/// # Errors
///
/// [`BufferError::WidthMismatch`] if the two widths differ and
/// [`BufferError::LengthMismatch`] if the frame counts differ. Nothing is
/// written then.
pub fn convert<A: Sample, B: Sample>(
    src: Interleaved<'_, A>,
    mut dst: InterleavedMut<'_, B>,
) -> Result<(), BufferError> {
    same_shape(src.layout(), dst.layout(), src.len(), dst.len())?;
    for (d, &s) in dst.samples_mut().iter_mut().zip(src.samples()) {
        *d = s.convert_to();
    }
    Ok(())
}

/// Interleaves `src`'s planes into `dst`: frame `i`, channel `c` of `dst` is
/// `src.plane(c)[i]`.
///
/// # Errors
///
/// The same as [`convert`].
pub fn interleave<S: Sample>(
    src: Planes<'_, S>,
    mut dst: InterleavedMut<'_, S>,
) -> Result<(), BufferError> {
    same_shape(src.layout(), dst.layout(), src.len(), dst.len())?;
    let stride = dst.stride();
    for (c, plane) in src.iter_planes().enumerate() {
        let out = dst.samples_mut().iter_mut().skip(c).step_by(stride);
        for (d, &s) in out.zip(plane.samples()) {
            *d = s;
        }
    }
    Ok(())
}

/// Deinterleaves `src` into `dst`'s planes: `dst.plane(c)[i]` is frame `i`,
/// channel `c` of `src`.
///
/// # Errors
///
/// The same as [`convert`].
pub fn deinterleave<S: Sample>(
    src: Interleaved<'_, S>,
    mut dst: PlanesMut<'_, S>,
) -> Result<(), BufferError> {
    same_shape(src.layout(), dst.layout(), src.len(), dst.len())?;
    let stride = src.stride();
    for (c, mut plane) in dst.planes_mut().enumerate() {
        let from = src.samples().iter().skip(c).step_by(stride);
        for (d, &s) in plane.samples_mut().iter_mut().zip(from) {
            *d = s;
        }
    }
    Ok(())
}

/// Refuses a width or frame-count mismatch, width first.
fn same_shape(
    src_layout: tutti_types::ChannelLayout,
    dst_layout: tutti_types::ChannelLayout,
    src_len: tutti_types::Samples,
    dst_len: tutti_types::Samples,
) -> Result<(), BufferError> {
    if src_layout != dst_layout {
        return Err(BufferError::WidthMismatch {
            src: src_layout,
            dst: dst_layout,
        });
    }
    if src_len != dst_len {
        return Err(BufferError::LengthMismatch {
            src: src_len,
            dst: dst_len,
        });
    }
    Ok(())
}
