//! [`Mono`] and [`MonoMut`]: one channel's run of samples.

use crate::Sample;
use core::ops::Range;
use tutti_types::Samples;

/// A borrowed run of one channel's samples.
///
/// Any slice is a valid mono run, so [`new`](Self::new) cannot fail. It reads
/// as `&[S]` through [`samples`](Self::samples) and [`AsRef`], and counts in
/// frames ([`Samples`]).
///
/// ```
/// use tutti_buffers::Mono;
/// use tutti_types::Samples;
///
/// let data = [0.1f32, 0.2, 0.3, 0.4];
/// let run = Mono::new(&data);
/// assert_eq!(run.len(), Samples(4));
/// assert_eq!(run.window(Samples(1)..Samples(3)).samples(), &[0.2, 0.3]);
/// ```
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct Mono<'a, S: Sample = f32>(pub(crate) &'a [S]);

impl<'a, S: Sample> Mono<'a, S> {
    /// Wraps `data` as one channel.
    #[inline]
    #[must_use]
    pub const fn new(data: &'a [S]) -> Self {
        Self(data)
    }

    /// Returns the frame count.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> Samples {
        Samples(self.0.len())
    }

    /// Returns whether there are no frames.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the samples as a slice.
    #[inline]
    #[must_use]
    pub const fn samples(&self) -> &'a [S] {
        self.0
    }

    /// Returns the sub-run covering a frame range.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds, like slice indexing.
    #[inline]
    #[must_use]
    pub fn window(&self, frames: Range<Samples>) -> Mono<'a, S> {
        Mono(&self.0[frames.start.get()..frames.end.get()])
    }

    /// Splits into the first `at` frames and the rest.
    ///
    /// # Panics
    ///
    /// If `at` is past [`len`](Self::len).
    #[inline]
    #[must_use]
    pub fn split_at(&self, at: Samples) -> (Mono<'a, S>, Mono<'a, S>) {
        let (a, b) = self.0.split_at(at.get());
        (Mono(a), Mono(b))
    }
}

impl<S: Sample> AsRef<[S]> for Mono<'_, S> {
    #[inline]
    fn as_ref(&self) -> &[S] {
        self.0
    }
}

/// The write side of [`Mono`].
///
/// Separate because a mutable borrow cannot be `Copy`.
#[derive(Debug)]
#[repr(transparent)]
pub struct MonoMut<'a, S: Sample = f32>(pub(crate) &'a mut [S]);

impl<'a, S: Sample> MonoMut<'a, S> {
    /// Wraps `data` as one writable channel.
    #[inline]
    #[must_use]
    pub const fn new(data: &'a mut [S]) -> Self {
        Self(data)
    }

    /// Returns the frame count.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> Samples {
        Samples(self.0.len())
    }

    /// Returns whether there are no frames.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the samples as a mutable slice.
    #[inline]
    pub fn samples_mut(&mut self) -> &mut [S] {
        self.0
    }

    /// Reborrows as a read-only view.
    #[inline]
    #[must_use]
    pub fn as_ref(&self) -> Mono<'_, S> {
        Mono(self.0)
    }

    /// Reborrows for a shorter lifetime, so the view can be passed on and used
    /// again.
    #[inline]
    #[must_use]
    pub fn reborrow(&mut self) -> MonoMut<'_, S> {
        MonoMut(self.0)
    }

    /// Returns the writable sub-run covering a frame range.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds, like slice indexing.
    #[inline]
    #[must_use]
    pub fn window_mut(&mut self, frames: Range<Samples>) -> MonoMut<'_, S> {
        MonoMut(&mut self.0[frames.start.get()..frames.end.get()])
    }

    /// Splits into the first `at` frames and the rest, two disjoint views.
    ///
    /// # Panics
    ///
    /// If `at` is past [`len`](Self::len).
    #[inline]
    #[must_use]
    pub fn split_at_mut(&mut self, at: Samples) -> (MonoMut<'_, S>, MonoMut<'_, S>) {
        let (a, b) = self.0.split_at_mut(at.get());
        (MonoMut(a), MonoMut(b))
    }
}

impl<S: Sample> AsRef<[S]> for MonoMut<'_, S> {
    #[inline]
    fn as_ref(&self) -> &[S] {
        self.0
    }
}

impl<S: Sample> AsMut<[S]> for MonoMut<'_, S> {
    #[inline]
    fn as_mut(&mut self) -> &mut [S] {
        self.0
    }
}
