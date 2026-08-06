//! IParamValueQueue COM implementation.
//!
//! # Real-time safety
//!
//! Inner storage uses [`AudioThreadCell`] rather than a `Mutex`. VST3
//! queues are only touched during `IAudioProcessor::process`, which is
//! single-threaded on the audio thread — the lock is unnecessary
//! overhead.
//!
//! [`refill_from_queue`](ParamValueQueueImpl::refill_from_queue) lets
//! callers recycle a single `ComWrapper<ParamValueQueueImpl>` across
//! buffers; it keeps the inline `SmallVec` storage (16 points) and only
//! spills to the heap for unusually dense automation.

use smallvec::SmallVec;
use vst3::Steinberg::{
    kInvalidArgument, kResultOk, tresult,
    Vst::{IParamValueQueue, IParamValueQueueTrait},
};
use vst3::{Class, ComWrapper};

use crate::types::{ParameterPoint, ParameterQueue};
use tutti_plugin_types::Normalized;
#[cfg(test)]
use tutti_plugin_types::ParamAddress;
use tutti_types::AudioThreadCell;

/// Typical automation carries a handful of points per buffer. Values past
/// the inline capacity spill to the heap — rare and not on the hot path
/// once the caller has warmed up the queue off-RT.
const INLINE_POINTS: usize = 16;

pub struct ParamValueQueueImpl {
    param_id: AudioThreadCell<u32>,
    points: AudioThreadCell<SmallVec<[ParameterPoint; INLINE_POINTS]>>,
}

impl Class for ParamValueQueueImpl {
    type Interfaces = (IParamValueQueue,);
}

impl ParamValueQueueImpl {
    /// Build a queue from an existing [`ParameterQueue`]. Test-harness helper;
    /// the RT path uses [`new_empty`] + [`refill_from_queue`] to reuse the
    /// ComWrapper across buffers.
    #[cfg(test)]
    pub fn from_queue(queue: &ParameterQueue) -> ComWrapper<Self> {
        let mut points = SmallVec::with_capacity(queue.points.len().max(INLINE_POINTS));
        points.extend_from_slice(&queue.points);
        ComWrapper::new(Self {
            // The COM cell holds the bare `ParamID` the VST3 ABI passes; the
            // address model is resolved here, at the boundary. VST3 ids are
            // opaque, so a positional index addresses nothing and is dropped
            // to id 0 rather than reinterpreted as a handle.
            param_id: AudioThreadCell::new(queue.param_id.opaque().map(|id| id.get()).unwrap_or(0)),
            points: AudioThreadCell::new(points),
        })
    }

    pub fn new_empty(param_id: u32) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            param_id: AudioThreadCell::new(param_id),
            points: AudioThreadCell::new(SmallVec::new()),
        })
    }

    /// Replace this queue's contents with `queue`'s points in place.
    /// Allocation-free when `queue.points.len() <= capacity` (inline up to
    /// 16, or whatever the current heap capacity is after prior reuse).
    ///
    /// VST3 `IParamValueQueue` values are **normalized `0..1`** (the plugin
    /// reads them via `getPoint` and un-normalizes internally), which matches
    /// the host's authoring convention — so each point's value is clamped to
    /// `[0, 1]` here, guarding against an over-range authored/modulated value.
    pub fn refill_from_queue(&self, queue: &ParameterQueue) {
        // Same boundary as `from_queue`: unwrap the opaque handle for the ABI.
        *self.param_id.borrow_mut() = queue.param_id.opaque().map(|id| id.get()).unwrap_or(0);
        let mut points = self.points.borrow_mut();
        points.clear();
        points.reserve(queue.points.len());
        for p in &queue.points {
            points.push(ParameterPoint {
                sample_offset: p.sample_offset,
                // Copied verbatim. The `clamp(0.0, 1.0)` that stood here was
                // one of four copies of the unit-interval guard, and it was the
                // unsafe spelling — `f64::clamp` returns NaN for a NaN input,
                // so it passed through exactly the value it looked like it was
                // stopping. `Normalized` now carries the guard at construction,
                // making this copy redundant rather than wrong.
                value: p.value,
            });
        }
        // VST3 requires the points a plugin reads via `getPoint(0..n)` to be in
        // ascending sample-offset order: the documented interpolation is
        // `slope = (y2 - y1) / (x2 - x1)` over *consecutive* points, so an
        // out-of-order pair yields a negative `x2 - x1` and inverts the ramp.
        // Callers assemble automation from several sources (host lanes, routed
        // MIDI CCs) and have no obligation to interleave them in order, so the
        // sort belongs here, at the boundary where the plugin's view is built.
        //
        // `sort_by` on a nearly-sorted slice is the common case and cheap;
        // it is also stable, so two points sharing an offset keep insertion
        // order (the spec's "jump" idiom: old value then new value).
        if !points.is_sorted_by_key(|p| p.sample_offset) {
            points.sort_by_key(|p| p.sample_offset);
        }
    }

    #[cfg(test)]
    pub fn to_queue(&self) -> ParameterQueue {
        let mut queue = ParameterQueue::new(ParamAddress::Opaque(self.param_id().into()));
        self.for_each_point(|p| {
            queue.add_point(p.sample_offset, p.value.get());
        });
        queue
    }

    /// Iterate over each `ParameterPoint` in this queue without exposing
    /// the underlying `AudioThreadCell`. RT-safe.
    pub fn for_each_point(&self, mut f: impl FnMut(&ParameterPoint)) {
        for point in self.points.borrow().iter() {
            f(point);
        }
    }

    pub fn param_id(&self) -> u32 {
        *self.param_id.borrow()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.points.borrow().len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.points.borrow().is_empty()
    }
}

impl IParamValueQueueTrait for ParamValueQueueImpl {
    unsafe fn getParameterId(&self) -> u32 {
        *self.param_id.borrow()
    }

    unsafe fn getPointCount(&self) -> i32 {
        self.points.borrow().len() as i32
    }

    unsafe fn getPoint(&self, index: i32, sample_offset: *mut i32, value: *mut f64) -> tresult {
        let points = self.points.borrow();
        if index < 0 || index >= points.len() as i32 {
            return kInvalidArgument;
        }
        let point = &points[index as usize];
        if !sample_offset.is_null() {
            *sample_offset = point.sample_offset;
        }
        if !value.is_null() {
            *value = point.value.get();
        }
        kResultOk
    }

    unsafe fn addPoint(&self, sample_offset: i32, value: f64, index: *mut i32) -> tresult {
        let mut points = self.points.borrow_mut();
        points.push(ParameterPoint {
            sample_offset,
            // Inbound across the ABI: the *plugin* calls this, so the value is
            // as untrusted as anything else crossing that edge. `Normalized`
            // clamps it, which is a guard this direction never had.
            value: Normalized::new(value),
        });
        if !index.is_null() {
            *index = (points.len() - 1) as i32;
        }
        kResultOk
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue_with_points(count: usize) -> ParameterQueue {
        let mut q = ParameterQueue::new(ParamAddress::Opaque(42u32.into()));
        for i in 0..count {
            q.add_point(i as i32, i as f64 * 0.01);
        }
        q
    }

    /// Points added out of order must reach the plugin sorted ascending by
    /// sample offset.
    ///
    /// VST3 plugins interpolate between *consecutive* points
    /// (`slope = (y2 - y1) / (x2 - x1)`), so an unsorted pair produces a
    /// negative denominator and inverts the automation ramp. Callers merge
    /// automation from several sources (host lanes, routed MIDI CCs) and
    /// don't guarantee interleaved order, so the queue must sort.
    #[test]
    fn refill_from_queue_sorts_points_by_sample_offset() {
        let mut source = ParameterQueue::new(ParamAddress::Opaque(7u32.into()));
        source.add_point(384, 0.75);
        source.add_point(0, 0.25);
        source.add_point(128, 0.5);

        let queue = ParamValueQueueImpl::new_empty(0);
        queue.refill_from_queue(&source);

        let mut got = Vec::new();
        queue.for_each_point(|p| got.push((p.sample_offset, p.value.get())));
        assert_eq!(
            got,
            vec![(0, 0.25), (128, 0.5), (384, 0.75)],
            "points must be ascending by sample offset"
        );
    }

    /// Two points at the same offset keep insertion order — the spec's "jump"
    /// idiom transmits old-value-then-new-value at one position, and swapping
    /// them would inverting the jump.
    #[test]
    fn refill_from_queue_is_stable_for_equal_offsets() {
        let mut source = ParameterQueue::new(ParamAddress::Opaque(7u32.into()));
        source.add_point(64, 0.1); // old value
        source.add_point(64, 0.9); // new value at the same instant

        let queue = ParamValueQueueImpl::new_empty(0);
        queue.refill_from_queue(&source);

        let mut got = Vec::new();
        queue.for_each_point(|p| got.push(p.value.get()));
        assert_eq!(
            got,
            vec![0.1, 0.9],
            "equal offsets must keep insertion order"
        );
    }

    /// The sort must not allocate: it runs on every block that carries
    /// automation, on the audio thread. The existing no-alloc tests feed
    /// already-sorted points, so they never exercise the sorting branch.
    #[test]
    fn refill_from_queue_unsorted_is_allocation_free() {
        let mut source = ParameterQueue::new(ParamAddress::Opaque(7u32.into()));
        for i in (0..16i32).rev() {
            source.add_point(i * 8, f64::from(i) * 0.01);
        }

        let queue = ParamValueQueueImpl::new_empty(0);
        queue.refill_from_queue(&source); // warm up capacity

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..1_000 {
                queue.refill_from_queue(&source);
            }
        });
        assert_eq!(queue.len(), 16);
    }

    /// RT regression: `refill_from_queue` must not allocate when the
    /// inline SmallVec capacity (16) is sufficient. Covers the hot
    /// automation path where a DAW streams per-buffer points.
    #[test]
    fn refill_from_queue_is_allocation_free() {
        let queue = ParamValueQueueImpl::new_empty(0);
        let source = queue_with_points(8);

        // Warm up.
        queue.refill_from_queue(&source);

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..10_000 {
                queue.refill_from_queue(&source);
            }
        });
        assert_eq!(queue.len(), 8);
    }

    /// Refilling with more points than have been seen before grows
    /// once; after that, subsequent refills at the same size reuse the
    /// heap capacity. The no-alloc assertion covers the steady state.
    #[test]
    fn refill_from_queue_steady_state_is_allocation_free_after_grow() {
        let queue = ParamValueQueueImpl::new_empty(0);
        let big = queue_with_points(32); // > INLINE_POINTS
                                         // Grow first (outside the no-alloc scope).
        queue.refill_from_queue(&big);

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..1_000 {
                queue.refill_from_queue(&big);
            }
        });
        assert_eq!(queue.len(), 32);
    }
}
