//! [`Planes`] and [`PlanesMut`]: one equal-length run per channel.

use crate::{BufferError, Mono, MonoMut, Sample};
use core::ops::Range;
use tutti_types::{ChannelLayout, Samples};

/// Checks the planes' width and that every plane is as long as the first,
/// returning the width and the common length.
fn check<S: Sample, P: AsRef<[S]>>(planes: &[P]) -> Result<(ChannelLayout, Samples), BufferError> {
    let Some(first) = planes.first() else {
        return Err(BufferError::ZeroWidth);
    };
    let layout = ChannelLayout::try_from(planes.len()).map_err(|_| BufferError::TooWide {
        channels: planes.len(),
    })?;
    let expected = Samples(first.as_ref().len());
    for (plane, p) in planes.iter().enumerate() {
        let found = Samples(p.as_ref().len());
        if found != expected {
            return Err(BufferError::RaggedPlane {
                plane,
                expected,
                found,
            });
        }
    }
    Ok((layout, expected))
}

/// Borrowed planar audio: one equal-length run per channel.
///
/// The width is the number of planes. Construction checks once that there is
/// at least one plane and that no plane is longer or shorter than the first;
/// nothing after it re-checks.
///
/// ```
/// use tutti_buffers::Planes;
/// use tutti_types::Samples;
///
/// let (l, r) = ([1.0f32, 2.0, 3.0], [4.0, 5.0, 6.0]);
/// let slices = [&l[..], &r[..]];
/// let planes = Planes::new(&slices)?;
/// assert_eq!(planes.len(), Samples(3));
/// assert_eq!(planes.plane(1).samples(), &[4.0, 5.0, 6.0]);
/// assert!(Planes::new(&[&l[..], &r[..2]]).is_err());
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Planes<'a, S: Sample = f32> {
    planes: &'a [&'a [S]],
    layout: ChannelLayout,
    start: usize,
    frames: usize,
}

impl<'a, S: Sample> Planes<'a, S> {
    /// Wraps `planes`, one slice per channel.
    ///
    /// # Errors
    ///
    /// [`BufferError::ZeroWidth`] for no planes, [`BufferError::TooWide`] for
    /// more than `u16::MAX`, and [`BufferError::RaggedPlane`] for a plane whose
    /// length differs from the first's. Nothing is truncated.
    pub fn new(planes: &'a [&'a [S]]) -> Result<Self, BufferError> {
        let (layout, frames) = check(planes)?;
        Ok(Self {
            planes,
            layout,
            start: 0,
            frames: frames.get(),
        })
    }

    /// Returns the width: the number of planes.
    #[inline]
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Returns the frame count of every plane.
    #[inline]
    #[must_use]
    pub fn len(&self) -> Samples {
        Samples(self.frames)
    }

    /// Returns whether there are no frames.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }

    /// Returns channel `c`.
    ///
    /// # Panics
    ///
    /// If `c` is not below the width.
    #[inline]
    #[must_use]
    pub fn plane(&self, c: usize) -> Mono<'a, S> {
        Mono(&self.planes[c][self.start..self.start + self.frames])
    }

    /// Iterates the channels in order.
    #[inline]
    pub fn iter_planes(&self) -> impl Iterator<Item = Mono<'a, S>> + use<'a, S> {
        let (start, end) = (self.start, self.start + self.frames);
        self.planes.iter().map(move |p| Mono(&p[start..end]))
    }

    /// Returns the sub-run of every plane covering a frame range.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds, like slice indexing.
    #[inline]
    #[must_use]
    pub fn window(&self, frames: Range<Samples>) -> Planes<'a, S> {
        assert!(
            frames.start <= frames.end && frames.end.get() <= self.frames,
            "window {}..{} is out of bounds for {} frames",
            frames.start,
            frames.end,
            self.frames
        );
        Planes {
            start: self.start + frames.start.get(),
            frames: frames.end.get() - frames.start.get(),
            ..*self
        }
    }
}

/// The write side of [`Planes`].
///
/// [`planes_mut`](Self::planes_mut) yields one [`MonoMut`] per channel, and the
/// borrow checker keeps them disjoint.
///
/// ```
/// use tutti_buffers::PlanesMut;
///
/// let (mut l, mut r) = ([0.0f32; 2], [0.0f32; 2]);
/// let mut planes = [&mut l[..], &mut r[..]];
/// let mut out = PlanesMut::new(&mut planes)?;
/// for (c, mut plane) in out.planes_mut().enumerate() {
///     plane.samples_mut().fill(c as f32 + 1.0);
/// }
/// assert_eq!((l, r), ([1.0; 2], [2.0; 2]));
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
#[derive(Debug)]
pub struct PlanesMut<'a, S: Sample = f32> {
    planes: &'a mut [&'a mut [S]],
    layout: ChannelLayout,
    frames: usize,
}

impl<'a, S: Sample> PlanesMut<'a, S> {
    /// Wraps `planes`, one writable slice per channel.
    ///
    /// # Errors
    ///
    /// The same as [`Planes::new`].
    pub fn new(planes: &'a mut [&'a mut [S]]) -> Result<Self, BufferError> {
        let (layout, frames) = check(planes)?;
        Ok(Self {
            planes,
            layout,
            frames: frames.get(),
        })
    }

    /// Returns the width: the number of planes.
    #[inline]
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Returns the frame count of every plane.
    #[inline]
    #[must_use]
    pub fn len(&self) -> Samples {
        Samples(self.frames)
    }

    /// Returns whether there are no frames.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames == 0
    }

    /// Returns channel `c` read-only.
    ///
    /// # Panics
    ///
    /// If `c` is not below the width.
    #[inline]
    #[must_use]
    pub fn plane(&self, c: usize) -> Mono<'_, S> {
        Mono(&self.planes[c][..])
    }

    /// Returns channel `c` writable.
    ///
    /// # Panics
    ///
    /// If `c` is not below the width.
    #[inline]
    pub fn plane_mut(&mut self, c: usize) -> MonoMut<'_, S> {
        MonoMut(&mut self.planes[c][..])
    }

    /// Iterates the channels in order, each a writable view disjoint from the
    /// others.
    #[inline]
    pub fn planes_mut(&mut self) -> impl Iterator<Item = MonoMut<'_, S>> + use<'_, 'a, S> {
        self.planes.iter_mut().map(|p| MonoMut(&mut p[..]))
    }
}
