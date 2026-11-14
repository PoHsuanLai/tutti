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
    planes: Table<'a, S>,
    layout: ChannelLayout,
    start: usize,
    frames: usize,
}

/// The channel slices a [`Planes`] reads: its own shared ones, or a
/// [`PlanesMut`]'s, read through a shared borrow of them.
#[derive(Clone, Copy, Debug)]
enum Table<'a, S> {
    Shared(&'a [&'a [S]]),
    Unique(&'a [&'a mut [S]]),
}

impl<'a, S> Table<'a, S> {
    /// Channel `c`, whole.
    #[inline]
    fn get(self, c: usize) -> &'a [S] {
        match self {
            Self::Shared(p) => p[c],
            Self::Unique(p) => &*p[c],
        }
    }

    #[inline]
    fn len(self) -> usize {
        match self {
            Self::Shared(p) => p.len(),
            Self::Unique(p) => p.len(),
        }
    }
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
            planes: Table::Shared(planes),
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
        Mono(&self.planes.get(c)[self.start..self.start + self.frames])
    }

    /// Iterates the channels in order.
    #[inline]
    pub fn iter_planes(&self) -> impl Iterator<Item = Mono<'a, S>> + use<'a, S> {
        let (table, start, end) = (self.planes, self.start, self.start + self.frames);
        (0..table.len()).map(move |c| Mono(&table.get(c)[start..end]))
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
/// borrow checker keeps them disjoint. [`window_mut`](Self::window_mut) narrows
/// every plane to a frame range, and [`as_planes`](Self::as_planes) reads the
/// current contents of every channel, for a kernel that writes in place.
///
/// Two lifetimes: `'s` borrows the table of channel slices and `'a` is the
/// slices' own, so a window can borrow the table for less time than the audio.
///
/// ```
/// use tutti_buffers::PlanesMut;
/// use tutti_types::Samples;
///
/// let (mut l, mut r) = ([0.0f32; 4], [0.0f32; 4]);
/// let mut planes = [&mut l[..], &mut r[..]];
/// let mut out = PlanesMut::new(&mut planes)?;
/// for (c, mut plane) in out.window_mut(Samples(2)..Samples(4)).planes_mut().enumerate() {
///     plane.samples_mut().fill(c as f32 + 1.0);
/// }
/// assert_eq!(out.as_planes().plane(1).samples(), &[0.0, 0.0, 2.0, 2.0]);
/// assert_eq!((l, r), ([0.0, 0.0, 1.0, 1.0], [0.0, 0.0, 2.0, 2.0]));
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
#[derive(Debug)]
pub struct PlanesMut<'s, 'a, S: Sample = f32> {
    planes: &'s mut [&'a mut [S]],
    layout: ChannelLayout,
    start: usize,
    frames: usize,
}

impl<'s, 'a, S: Sample> PlanesMut<'s, 'a, S> {
    /// Wraps `planes`, one writable slice per channel.
    ///
    /// # Errors
    ///
    /// The same as [`Planes::new`].
    pub fn new(planes: &'s mut [&'a mut [S]]) -> Result<Self, BufferError> {
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

    /// Reads every channel as it is now.
    #[inline]
    #[must_use]
    pub fn as_planes(&self) -> Planes<'_, S> {
        Planes {
            planes: Table::Unique(&*self.planes),
            layout: self.layout,
            start: self.start,
            frames: self.frames,
        }
    }

    /// Reborrows for a shorter lifetime, so the view can be passed on and used
    /// again.
    #[inline]
    #[must_use]
    pub fn reborrow(&mut self) -> PlanesMut<'_, 'a, S> {
        PlanesMut {
            planes: &mut *self.planes,
            ..*self
        }
    }

    /// Returns every plane narrowed to a frame range.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds, like slice indexing.
    #[inline]
    #[must_use]
    pub fn window_mut(&mut self, frames: Range<Samples>) -> PlanesMut<'_, 'a, S> {
        assert!(
            frames.start <= frames.end && frames.end.get() <= self.frames,
            "window {}..{} is out of bounds for {} frames",
            frames.start,
            frames.end,
            self.frames
        );
        PlanesMut {
            planes: &mut *self.planes,
            layout: self.layout,
            start: self.start + frames.start.get(),
            frames: frames.end.get() - frames.start.get(),
        }
    }

    /// Returns channel `c` read-only.
    ///
    /// # Panics
    ///
    /// If `c` is not below the width.
    #[inline]
    #[must_use]
    pub fn plane(&self, c: usize) -> Mono<'_, S> {
        Mono(&self.planes[c][self.start..self.start + self.frames])
    }

    /// Returns channel `c` writable.
    ///
    /// # Panics
    ///
    /// If `c` is not below the width.
    #[inline]
    pub fn plane_mut(&mut self, c: usize) -> MonoMut<'_, S> {
        MonoMut(&mut self.planes[c][self.start..self.start + self.frames])
    }

    /// Iterates the channels in order, each a writable view disjoint from the
    /// others.
    #[inline]
    pub fn planes_mut(&mut self) -> impl Iterator<Item = MonoMut<'_, S>> + use<'_, 'a, S> {
        let (start, end) = (self.start, self.start + self.frames);
        self.planes
            .iter_mut()
            .map(move |p| MonoMut(&mut p[start..end]))
    }
}
