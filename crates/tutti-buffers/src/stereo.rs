//! [`StereoPlanes`]: exactly two equal-length planes.

use crate::{BufferError, Sample};
use tutti_types::{ChannelLayout, Samples};

/// Two equal-length channel planes: the deinterleaved stereo pair that
/// correlation and amplitude metering consume.
///
/// Mid/side and L/R correlation are defined at exactly two channels, so the
/// arity is in the type rather than a [`Planes`](crate::Planes) of any width.
/// Construction checks that the lengths agree, so a measurement never divides
/// by the wrong count.
///
/// ```
/// use tutti_buffers::StereoPlanes;
/// use tutti_types::Samples;
///
/// let (l, r) = ([1.0f32, 2.0], [3.0, 4.0]);
/// let pair = StereoPlanes::new(&l, &r)?;
/// assert_eq!(pair.frames(), Samples(2));
/// assert!(StereoPlanes::new(&l, &r[..1]).is_err());
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
#[derive(Clone, Copy, Debug)]
pub struct StereoPlanes<'a, S: Sample = f32> {
    left: &'a [S],
    right: &'a [S],
}

impl<'a, S: Sample> StereoPlanes<'a, S> {
    /// Pairs two planes.
    ///
    /// # Errors
    ///
    /// [`BufferError::RaggedPlane`] if the right plane's length differs from
    /// the left's.
    #[inline]
    pub fn new(left: &'a [S], right: &'a [S]) -> Result<Self, BufferError> {
        if left.len() == right.len() {
            Ok(Self { left, right })
        } else {
            Err(BufferError::RaggedPlane {
                plane: 1,
                expected: Samples(left.len()),
                found: Samples(right.len()),
            })
        }
    }

    /// Returns the frames in each plane.
    #[inline]
    #[must_use]
    pub fn frames(&self) -> Samples {
        Samples(self.left.len())
    }

    /// Returns the width, always [`ChannelLayout::STEREO`].
    #[inline]
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        ChannelLayout::STEREO
    }

    /// Returns whether the planes are empty.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.left.is_empty()
    }

    /// Returns the left plane.
    #[inline]
    #[must_use]
    pub fn left(&self) -> &'a [S] {
        self.left
    }

    /// Returns the right plane.
    #[inline]
    #[must_use]
    pub fn right(&self) -> &'a [S] {
        self.right
    }
}
