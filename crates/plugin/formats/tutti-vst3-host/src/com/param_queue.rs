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
//! buffers. Its point storage is a [`PointBuf`] sized at construction and
//! never grown: past it the queue keeps its latest value
//! ([`PointBuf::push_or_replace_last`]), counted. A pooled
//! list lends a queue a larger buffer for a block
//! ([`lend_from`](ParamValueQueueImpl::lend_from)).

use vst3::Steinberg::{
    kInvalidArgument, kResultFalse, kResultOk, tresult,
    Vst::{IParamValueQueue, IParamValueQueueTrait},
};
use vst3::{Class, ComWrapper};

use crate::types::{ParameterPoint, ParameterQueue};
#[cfg(test)]
use tutti_plugin_types::ParamAddress;
use tutti_plugin_types::{Normalized, PointBuf, PointPool};
use tutti_types::AudioThreadCell;

/// The least point room a queue is built with. Typical automation carries a
/// handful of points per buffer; past a queue's room it keeps its latest
/// value, counted.
pub(crate) const MIN_POINT_ROOM: usize = 16;

pub struct ParamValueQueueImpl {
    param_id: AudioThreadCell<u32>,
    points: AudioThreadCell<PointBuf>,
    /// Points the plugin's `addPoint` could not add as new points (the
    /// queue full; it kept the latest value).
    dropped: AudioThreadCell<u64>,
}

impl Class for ParamValueQueueImpl {
    type Interfaces = (IParamValueQueue,);
}

impl ParamValueQueueImpl {
    /// An empty queue with room for exactly `points` points (a pooled list's
    /// queue draws on the list's shared pool past them). Allocates; call it
    /// off the audio thread.
    pub fn with_room(param_id: u32, points: usize) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            param_id: AudioThreadCell::new(param_id),
            points: AudioThreadCell::new(PointBuf::with_capacity(points)),
            dropped: AudioThreadCell::new(0),
        })
    }

    /// The points this queue holds.
    pub(crate) fn capacity(&self) -> usize {
        self.points.borrow().capacity()
    }

    /// Borrows a block from `pool` as queue `index` of its list
    /// ([`PointPool::lend`]): how a pooled list gives a queue more room for
    /// a block. Allocation-free.
    pub(crate) fn lend_from(&self, pool: &mut PointPool, index: usize) -> bool {
        pool.lend(index, &mut self.points.borrow_mut())
    }

    /// Swaps this queue's point storage with `other`, contents and all: how
    /// a pooled list takes a lent block back ([`PointPool::give_back`]).
    /// Allocation-free.
    pub(crate) fn swap_points(&self, other: &mut PointBuf) {
        core::mem::swap(&mut *self.points.borrow_mut(), other);
    }

    /// Replaces this queue's contents with `queue`'s points in place, within
    /// the room it has: never allocates. Past the room the queue keeps its
    /// latest value ([`PointBuf::push_or_replace_last`], so the parameter
    /// still ends the block on it), and the points left out are returned,
    /// for the caller to count.
    ///
    /// VST3 `IParamValueQueue` values are **normalized `0..1`** (the plugin
    /// reads them via `getPoint` and un-normalizes internally), which matches
    /// the host's authoring convention; `Normalized` carries that guard.
    pub fn refill_from_queue(&self, queue: &ParameterQueue) -> u64 {
        // Same boundary as `from_queue`: unwrap the opaque handle for the ABI.
        *self.param_id.borrow_mut() = queue.param_id.opaque().map(|id| id.get()).unwrap_or(0);
        let mut points = self.points.borrow_mut();
        points.clear();
        let mut left_out = 0;
        for p in &queue.points {
            // Copied verbatim: `Normalized` carries the unit-interval guard
            // at construction. A `clamp(0.0, 1.0)` here would be redundant,
            // and unsafe besides — `f64::clamp` returns NaN for a NaN input.
            if !points.push_or_replace_last(*p) {
                left_out += 1;
            }
        }
        // VST3 requires the points a plugin reads via `getPoint(0..n)` to be in
        // ascending sample-offset order: the documented interpolation is
        // `slope = (y2 - y1) / (x2 - x1)` over *consecutive* points, so an
        // out-of-order pair yields a negative `x2 - x1` and inverts the ramp.
        // Callers assemble automation from several sources (host lanes, routed
        // MIDI CCs) and have no obligation to interleave them in order, so the
        // sort belongs here, at the boundary where the plugin's view is built.
        //
        // Stable, so two points sharing an offset keep insertion order (the
        // spec's "jump" idiom: old value then new value), and an insertion
        // sort, because std's stable sort heap-allocates its scratch past 256
        // points (4 KiB of them) and a queue holds up to a chunk's room
        // (`ParameterQueue::chunk_room`: past 256 from a block of 8 Ki
        // frames). The points arrive as runs each in order (host lanes,
        // routed CCs); its cost is quadratic in the overlap of those runs
        // (up to n1 * n2 moves for two interleaved runs), bounded by the
        // queue's room.
        if !points.is_sorted_by_key(|p| p.sample_offset) {
            tutti_plugin_types::insertion_sort_by_key(&mut points, |p| p.sample_offset);
        }
        left_out
    }

    /// Empties this queue for `param_id`, keeping its point storage. For a
    /// pooled queue handed to the plugin through `addParameterData`.
    pub fn reset(&self, param_id: u32) {
        *self.param_id.borrow_mut() = param_id;
        self.points.borrow_mut().clear();
    }

    /// Points the plugin added past this queue's room, since it was built.
    pub(crate) fn points_dropped(&self) -> u64 {
        *self.dropped.borrow()
    }

    /// Iterates over each `ParameterPoint` in this queue without exposing
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

    /// Adds the plugin's point within the queue's room, `kResultOk` with
    /// its `index`. Past the room the queue keeps its latest value
    /// ([`PointBuf::replace_latest`]): a point no earlier than the latest
    /// takes its place, `kResultOk` with that slot's `index`; an earlier one
    /// is dropped, `kResultFalse` with `index` untouched. Either way a point
    /// is lost, counted
    /// ([`ParameterChangesImpl::points_dropped`](super::ParameterChangesImpl::points_dropped)).
    /// Never allocates: the plugin calls this on the audio thread.
    unsafe fn addPoint(&self, sample_offset: i32, value: f64, index: *mut i32) -> tresult {
        let mut points = self.points.borrow_mut();
        let point = ParameterPoint {
            sample_offset,
            // Inbound across the ABI: the *plugin* calls this, so the value is
            // as untrusted as anything else crossing that edge. `Normalized`
            // clamps it, which is a guard this direction never had.
            value: Normalized::new(value),
        };
        let at = match points.try_push(point) {
            Ok(()) => Some(points.len() - 1),
            Err(point) => {
                *self.dropped.borrow_mut() += 1;
                points.replace_latest(point)
            }
        };
        match at {
            Some(i) if !index.is_null() => *index = i as i32,
            Some(_) => {}
            None => return kResultFalse,
        }
        kResultOk
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue_with_points(count: usize) -> ParameterQueue {
        let mut q = ParameterQueue::with_capacity(ParamAddress::Opaque(42u32.into()), count);
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
        let mut source = ParameterQueue::with_capacity(ParamAddress::Opaque(7u32.into()), 3);
        source.add_point(384, 0.75);
        source.add_point(0, 0.25);
        source.add_point(128, 0.5);

        let queue = ParamValueQueueImpl::with_room(0, MIN_POINT_ROOM);
        queue.refill_from_queue(&source);

        let mut got = Vec::new();
        queue.for_each_point(|p| got.push((p.sample_offset, p.value.get())));
        assert_eq!(
            got,
            vec![(0, 0.25), (128, 0.5), (384, 0.75)],
            "points must be ascending by sample offset"
        );
    }

    /// **An unsorted queue past 256 points refills without allocating**: a
    /// queue's room is a chunk's (`ParameterQueue::chunk_room`, past 256 from
    /// a block of 8 Ki frames), and std's stable sort allocates its scratch
    /// past 256 points. Two interleaved runs, each in order.
    ///
    /// Mutation (run): the sort back to `points.sort_by_key(|p|
    /// p.sample_offset)` aborts in the gate.
    #[test]
    fn refilling_an_unsorted_queue_past_256_points_does_not_allocate() {
        const RUN: i32 = 150;
        let mut source =
            ParameterQueue::with_capacity(ParamAddress::Opaque(7u32.into()), 2 * RUN as usize);
        for i in 0..RUN {
            source.add_point(2 * i, 0.25);
        }
        for i in 0..RUN {
            source.add_point(2 * i + 1, 0.75);
        }
        let queue = ParamValueQueueImpl::with_room(0, 2 * RUN as usize);
        let left_out = assert_no_alloc::assert_no_alloc(|| queue.refill_from_queue(&source));
        assert_eq!(left_out, 0);
        let mut got = Vec::new();
        queue.for_each_point(|p| got.push(p.sample_offset));
        assert_eq!(got, (0..2 * RUN).collect::<Vec<_>>());
    }

    /// Two points at the same offset keep insertion order — the spec's "jump"
    /// idiom transmits old-value-then-new-value at one position, and swapping
    /// them would inverting the jump.
    #[test]
    fn refill_from_queue_is_stable_for_equal_offsets() {
        let mut source = ParameterQueue::with_capacity(ParamAddress::Opaque(7u32.into()), 2);
        source.add_point(64, 0.1); // old value
        source.add_point(64, 0.9); // new value at the same instant

        let queue = ParamValueQueueImpl::with_room(0, MIN_POINT_ROOM);
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
        let mut source = ParameterQueue::with_capacity(ParamAddress::Opaque(7u32.into()), 16);
        for i in (0..16i32).rev() {
            source.add_point(i * 8, f64::from(i) * 0.01);
        }

        let queue = ParamValueQueueImpl::with_room(0, MIN_POINT_ROOM);
        queue.refill_from_queue(&source); // warm up capacity

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..1_000 {
                queue.refill_from_queue(&source);
            }
        });
        assert_eq!(queue.len(), 16);
    }

    /// RT regression: `refill_from_queue` must not allocate when the
    /// queue's reserved room ([`MIN_POINT_ROOM`]) is sufficient. Covers the hot
    /// automation path where a DAW streams per-buffer points.
    #[test]
    fn refill_from_queue_is_allocation_free() {
        let queue = ParamValueQueueImpl::with_room(0, MIN_POINT_ROOM);
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

    /// **A queue holds the room it was built with and never grows**: one
    /// built for 32 points takes 32 without allocating, block after block,
    /// and a 33rd keeps the queue's latest value, the point left out
    /// returned for counting, still without allocating. (Reworded with
    /// `PointBuf`, and renamed from
    /// `refill_from_queue_steady_state_is_allocation_free_after_grow`: a
    /// queue past `MIN_POINT_ROOM` used to grow once on the audio thread.)
    ///
    /// Mutation (run): `refill_from_queue` with `try_push` (dropping past
    /// the room) rather than `push_or_replace_last` → the queue ends on
    /// frame 31, not 32 → fails.
    #[test]
    fn refill_from_queue_holds_its_room_and_never_grows() {
        let queue = ParamValueQueueImpl::with_room(0, 32);
        let big = queue_with_points(32);
        let over = queue_with_points(33);
        let left_out = assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..1_000 {
                assert_eq!(queue.refill_from_queue(&big), 0);
            }
            queue.refill_from_queue(&over)
        });
        assert_eq!(left_out, 1);
        assert_eq!(queue.len(), 32);
        assert_eq!(queue.capacity(), 32);
        let mut last = None;
        queue.for_each_point(|p| last = Some(p.sample_offset));
        assert_eq!(last, Some(32), "ends on its latest");
    }

    /// **The plugin's `addPoint` past the room keeps the latest value,
    /// counted, and says where it went.** A queue of `MIN_POINT_ROOM` takes
    /// that many points (`kResultOk`, each at its index), then a later point
    /// replaces its latest: `kResultOk` with the replaced slot's index, the
    /// lost point counted. An earlier point is dropped: `kResultFalse`,
    /// `index` untouched, counted. All inside the no-alloc gate.
    ///
    /// Mutations (run), each fails: `addPoint` dropping the point past the
    /// room (no `replace_latest`) → the queue ends on its earlier value; the
    /// miss not counted → `points_dropped` is 0; the replacement answered
    /// `kResultFalse` (as it was) → the second result is wrong; the
    /// replacement's index not written → `at` is still -1.
    #[test]
    fn add_point_past_the_room_keeps_the_latest_counted() {
        use vst3::Steinberg::Vst::IParamValueQueueTrait;
        let queue = ParamValueQueueImpl::with_room(3, MIN_POINT_ROOM);
        let ptr = queue.to_com_ptr::<IParamValueQueue>().unwrap();
        let (fitted, replaced, dropped) = assert_no_alloc::assert_no_alloc(|| unsafe {
            let mut at = -1;
            let fitted = (0..MIN_POINT_ROOM as i32)
                .all(|k| ptr.addPoint(k, 0.25, &mut at) == kResultOk && at == k);
            at = -1;
            let replaced = (ptr.addPoint(100, 0.75, &mut at), at);
            at = -1;
            let dropped = (ptr.addPoint(50, 0.5, &mut at), at);
            (fitted, replaced, dropped)
        });
        assert!(fitted);
        assert_eq!(replaced, (kResultOk, MIN_POINT_ROOM as i32 - 1));
        assert_eq!(dropped, (kResultFalse, -1));
        assert_eq!(queue.points_dropped(), 2);
        assert_eq!(queue.len(), MIN_POINT_ROOM);
        let mut last = None;
        queue.for_each_point(|p| last = Some((p.sample_offset, p.value.get())));
        assert_eq!(last, Some((100, 0.75)));
    }
}
