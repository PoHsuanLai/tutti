//! [`FilterSizes`]: the bounds of a filter, which allocates nothing from its controls.

use core::marker::PhantomData;

use tutti_types::Db;

use crate::{Ring, Sizes};

/// What a filter design allocates from its controls: nothing. Its width is the
/// design's, so one figure serves every model of `M`; the node declares
/// [`Tail::Unknown`](tutti_types::Tail::Unknown) and never parks, so the ring-out
/// it bounds is [`Ring::Unbounded`] (a resonant filter rings as long as its Q or
/// resonance says).
#[derive(Clone, Copy, Debug)]
pub struct FilterSizes<M>(PhantomData<fn() -> M>);

impl<M> FilterSizes<M> {
    /// The only figure there is.
    pub(super) const NONE: Self = Self(PhantomData);
}

impl<M> Sizes for FilterSizes<M>
where
    M: Copy + core::fmt::Debug,
{
    type Source = M;

    fn measure(_: &M, _: Db) -> Self {
        Self::NONE
    }

    fn tail(&self) -> Ring {
        Ring::Unbounded
    }

    fn dominates(&self, _: &Self) -> bool {
        true
    }
}
