//! IParamValueQueue COM implementation.
//!
//! # Real-time safety
//!
//! The queue is one object, `{ param_id, points }`, in one
//! [`AudioThreadCell`] (see [`super::event_list`]), and its drop count is an
//! atomic beside it. A plugin thread may be inside a COM entry point of the
//! queue (it can keep the `IParamValueQueue*` and read it from a worker) when
//! the host comes to stage the next block, so every borrow of the queue, the
//! host's included, takes `try_`: the host reaches the storage only through
//! [`try_stage`](ParamValueQueueImpl::try_stage), which is `None` while a
//! plugin thread holds the queue, and the list that owns the queue skips the
//! block, counted.
//!
//! [`QueueState::refill`] lets callers recycle a single
//! `ComWrapper<ParamValueQueueImpl>` across buffers. Its point storage is a
//! [`PointBuf`] sized at construction and never grown: past it the queue keeps
//! its latest value ([`PointBuf::push_or_replace_last`]), counted. A pooled
//! list lends a queue a larger buffer for a block
//! ([`QueueState::lend_from`]).

use crate::guard::{Out, guard};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use vst3::Steinberg::{
    Vst::{IParamValueQueue, IParamValueQueueTrait, kNoParamId},
    kInvalidArgument, kResultFalse, kResultOk, tresult,
};
use vst3::{Class, ComWrapper};

use tutti_plugin_types::{Normalized, PointBuf, PointPool};
use tutti_plugin_types::{ParameterPoint, ParameterQueue};
use tutti_rt::{AudioThreadCell, BorrowGuard};

/// The least point room a queue is built with. Typical automation carries a
/// handful of points per buffer; past a queue's room it keeps its latest
/// value, counted.
pub const MIN_POINT_ROOM: usize = 16;

/// What a queue holds: its parameter and its points. Reached only through a
/// borrow of the queue's cell, so the host cannot touch it without holding the
/// boundary the plugin's COM calls respect.
pub(crate) struct QueueState {
    param_id: u32,
    points: PointBuf,
}

pub struct ParamValueQueueImpl {
    state: AudioThreadCell<QueueState>,
    /// Points the plugin's `addPoint` could not add as new points (the
    /// queue full; it kept the latest value). Atomic, not in `state`, so a COM
    /// entry that finds `state` borrowed still counts its refusal.
    dropped: AtomicU64,
}

impl Class for ParamValueQueueImpl {
    type Interfaces = (IParamValueQueue,);
}

impl QueueState {
    /// The points this queue holds.
    pub(crate) fn capacity(&self) -> usize {
        self.points.capacity()
    }

    /// The parameter this queue is filed under.
    pub(crate) fn param_id(&self) -> u32 {
        self.param_id
    }

    /// Borrows a block from `pool` as queue `index` of its list
    /// ([`PointPool::lend`]): how a pooled list gives a queue more room for
    /// a block. Allocation-free.
    pub(crate) fn lend_from(&mut self, pool: &mut PointPool, index: usize) -> bool {
        pool.lend(index, &mut self.points)
    }

    /// Swaps this queue's point storage with `other`, contents and all: how
    /// a pooled list takes a lent block back ([`PointPool::give_back`]).
    /// Allocation-free.
    pub(crate) fn swap_points(&mut self, other: &mut PointBuf) {
        core::mem::swap(&mut self.points, other);
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
    pub(crate) fn refill(&mut self, queue: &ParameterQueue) -> u64 {
        // Same boundary as `from_queue`: unwrap the opaque handle for the ABI.
        self.param_id = queue.param_id.get();
        let points = &mut self.points;
        points.clear();
        let mut left_out = 0;
        for p in &queue.points {
            // Copied verbatim: `Normalized` carries the unit-interval guard
            // at construction. A `clamp(0.0, 1.0)` here would be redundant,
            // and unsafe besides: `f64::clamp` returns NaN for a NaN input.
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
        // A stable sort, so two points sharing an offset keep insertion
        // order (the spec's "jump" idiom: old value then new value), and an
        // allocation-free one (`PointBuf::sort_by_offset`): std's stable
        // sort allocates a scratch buffer past 256 points, and a queue holds
        // up to a chunk's room, past 256 at every chunk size
        // (`ParameterQueue::chunk_room` is `ceil(chunk / 32) + 1 + 256`).
        // Linear on points already in order, the common case; quadratic in
        // the overlap of the runs when sources interleave (up to n1 * n2
        // moves), bounded by the queue's room.
        points.sort_by_offset();
        left_out
    }

    /// Empties this queue for `param_id`, keeping its point storage.
    pub(crate) fn reset(&mut self, param_id: u32) {
        self.param_id = param_id;
        self.points.clear();
    }

    /// Puts `point` before every point the queue holds, if it has room.
    /// Allocation-free; `false` (the queue unchanged) when it is full.
    pub(crate) fn prepend(&mut self, point: ParameterPoint) -> bool {
        self.points.try_push_front(point).is_ok()
    }

    /// Iterates over each `ParameterPoint` in this queue. RT-safe.
    pub(crate) fn for_each_point(&self, mut f: impl FnMut(&ParameterPoint)) {
        for point in self.points.iter() {
            f(point);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.points.len()
    }
}

impl ParamValueQueueImpl {
    /// An empty queue with room for exactly `points` points (a pooled list's
    /// queue draws on the list's shared pool past them). Allocates; call it
    /// off the audio thread.
    pub fn with_room(param_id: u32, points: usize) -> ComWrapper<Self> {
        ComWrapper::new(Self {
            state: AudioThreadCell::new(QueueState {
                param_id,
                points: PointBuf::with_capacity(points),
            }),
            dropped: AtomicU64::new(0),
        })
    }

    /// The queue's storage for the host to stage, or `None` while a plugin
    /// thread is inside one of the queue's calls: the host skips the block
    /// rather than wait or panic.
    pub(crate) fn try_stage(&self) -> Option<BorrowGuard<'_, QueueState>> {
        self.state.try_borrow_mut()
    }

    /// [`try_stage`](Self::try_stage) for a test, which owns every reader.
    #[cfg(test)]
    pub(crate) fn stage(&self) -> BorrowGuard<'_, QueueState> {
        self.state.borrow_mut()
    }

    /// The parameter this queue is filed under, or `None` while another
    /// thread is resetting the queue.
    pub(crate) fn try_param_id(&self) -> Option<u32> {
        self.state.try_borrow().map(|s| s.param_id)
    }

    /// A hold on the queue as a plugin thread inside `getPoint` has one, for
    /// a conformance test of what the host does while the plugin holds it.
    #[cfg(any(test, feature = "conformance"))]
    pub(crate) fn hold_as_plugin(&self) -> impl Sized + '_ {
        self.state.borrow()
    }

    /// Points the plugin added past this queue's room, since it was built.
    pub(crate) fn points_dropped(&self) -> u64 {
        self.dropped.load(Relaxed)
    }

    /// Counts one dropped point.
    fn count_dropped(&self) {
        self.dropped.fetch_add(1, Relaxed);
    }
}

/// Every entry point takes its borrow with `try_`: a plugin whose worker
/// threads read this queue during `process` share the borrow, and one that
/// reads it while another of its threads writes it gets the method's failure
/// value, where a panicking borrow would unwind into the plugin's C++ frame
/// and abort the process.
impl IParamValueQueueTrait for ParamValueQueueImpl {
    /// `kNoParamId` while another thread is resetting the queue.
    unsafe fn getParameterId(&self) -> u32 {
        guard(kNoParamId, || self.try_param_id().unwrap_or(kNoParamId))
    }

    /// Zero while another thread is writing the queue.
    unsafe fn getPointCount(&self) -> i32 {
        guard(0, || {
            self.state.try_borrow().map_or(0, |s| s.points.len() as i32)
        })
    }

    /// `kResultFalse` while another thread is writing the queue.
    unsafe fn getPoint(&self, index: i32, sample_offset: *mut i32, value: *mut f64) -> tresult {
        // SAFETY: COM out-parameters: each null or valid for this call.
        let (offset_out, value_out) = unsafe { (Out::new(sample_offset), Out::new(value)) };
        guard(vst3::Steinberg::kInternalError, || {
            let Some(state) = self.state.try_borrow() else {
                return kResultFalse;
            };
            let points = &state.points;
            if index < 0 || index >= points.len() as i32 {
                return kInvalidArgument;
            }
            let point = &points[index as usize];
            offset_out.set(point.sample_offset);
            value_out.set(point.value.get());
            kResultOk
        })
    }

    /// Adds the plugin's point within the queue's room, `kResultOk` with
    /// its `index`. Past the room the queue keeps its latest value
    /// ([`PointBuf::replace_latest`]): a point no earlier than the latest
    /// takes its place, `kResultOk` with that slot's `index`; an earlier one
    /// is dropped, `kResultFalse` with `index` untouched. Either way a point
    /// is lost, counted
    /// ([`ParameterChangesImpl::points_dropped`](super::ParameterChangesImpl::points_dropped)).
    /// Never allocates: the plugin calls this on the audio thread.
    ///
    /// While another thread reads or writes the queue the point is dropped,
    /// `kResultFalse`, and counted too.
    unsafe fn addPoint(&self, sample_offset: i32, value: f64, index: *mut i32) -> tresult {
        // SAFETY: a COM out-parameter: null or valid for this call.
        let index = unsafe { Out::new(index) };
        guard(vst3::Steinberg::kInternalError, || {
            let Some(mut state) = self.state.try_borrow_mut() else {
                self.count_dropped();
                return kResultFalse;
            };
            let points = &mut state.points;
            let point = ParameterPoint {
                sample_offset,
                // Inbound across the ABI: the *plugin* calls this, so the value is
                // as untrusted as anything else crossing that edge. `Normalized`
                // clamps it, which is a guard this direction never had.
                value: Normalized::clamped(value),
            };
            let at = match points.try_push(point) {
                Ok(()) => Some(points.len() - 1),
                Err(point) => {
                    self.count_dropped();
                    points.replace_latest(point)
                }
            };
            match at {
                Some(i) => index.set(i as i32),
                None => return kResultFalse,
            }
            kResultOk
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plugin threads reading a queue at once all get it.
    ///
    /// Mutation (run): `AudioThreadCell::try_acquire_reader` exclusive → the
    /// COM reads answer 0 / `kResultFalse` → fails.
    #[test]
    fn concurrent_reads_of_a_queue_both_succeed() {
        use std::sync::Barrier;
        let queue = ParamValueQueueImpl::with_room(9, MIN_POINT_ROOM);
        queue.stage().refill(&queue_with_points(2));
        let (taken, done) = (Barrier::new(2), Barrier::new(2));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let read = queue.state.borrow();
                taken.wait();
                done.wait();
                assert_eq!(read.len(), 2);
            });
            taken.wait();
            let ptr = queue.to_com_ptr::<IParamValueQueue>().unwrap();
            let (mut offset, mut value) = (0, 0.0);
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            let count = unsafe { ptr.getPointCount() };
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            let got = unsafe { ptr.getPoint(1, &mut offset, &mut value) };
            done.wait();
            assert_eq!(count, 2, "a second reader must see the queue");
            assert_eq!((got, offset), (kResultOk, 1));
        });
    }

    /// A read while the queue is being written answers with the method's
    /// failure value instead of panicking into the plugin.
    ///
    /// Mutation (run): `getPoint` back on a panicking `borrow` → the panic
    /// cannot unwind out of the COM thunk, and the test process aborts
    /// (SIGABRT): the plugin-server crash this guards against.
    #[test]
    fn a_read_during_a_write_fails_at_the_com_level() {
        let queue = ParamValueQueueImpl::with_room(9, MIN_POINT_ROOM);
        queue.stage().refill(&queue_with_points(2));
        let ptr = queue.to_com_ptr::<IParamValueQueue>().unwrap();
        let (mut offset, mut value) = (0, 0.0);
        {
            let _writing = queue.state.borrow_mut();
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            assert_eq!(unsafe { ptr.getPointCount() }, 0);
            // SAFETY: both out-parameters are valid locals.
            let rc = unsafe { ptr.getPoint(0, &mut offset, &mut value) };
            assert_eq!(rc, kResultFalse);
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            assert_eq!(unsafe { ptr.getParameterId() }, kNoParamId);
            // SAFETY: the test calls a COM method on a live object it owns, with arguments
            // valid for the call.
            assert_eq!(unsafe { ptr.addPoint(5, 0.5, &mut 0) }, kResultFalse);
        }
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.getPointCount() }, 2);
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        assert_eq!(unsafe { ptr.getParameterId() }, 42);
        assert_eq!(queue.points_dropped(), 1, "the refused point is counted");
    }

    fn queue_with_points(count: usize) -> ParameterQueue {
        let mut q = ParameterQueue::with_capacity(42u32.into(), count);
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
        let mut source = ParameterQueue::with_capacity(7u32.into(), 3);
        source.add_point(384, 0.75);
        source.add_point(0, 0.25);
        source.add_point(128, 0.5);

        let queue = ParamValueQueueImpl::with_room(0, MIN_POINT_ROOM);
        queue.stage().refill(&source);

        let mut got = Vec::new();
        queue
            .stage()
            .for_each_point(|p| got.push((p.sample_offset, p.value.get())));
        assert_eq!(
            got,
            vec![(0, 0.25), (128, 0.5), (384, 0.75)],
            "points must be ascending by sample offset"
        );
    }

    /// **An unsorted queue past 256 points refills without allocating**: a
    /// queue's room is a chunk's (`ParameterQueue::chunk_room`, past 256 at
    /// every chunk size), and std's stable sort allocates its scratch past
    /// 256 points. Two interleaved runs, each in order.
    ///
    /// Mutation (run): the sort back to `points.sort_by_key(|p|
    /// p.sample_offset)` aborts in the gate.
    #[test]
    fn refilling_an_unsorted_queue_past_256_points_does_not_allocate() {
        const RUN: i32 = 150;
        let mut source = ParameterQueue::with_capacity(7u32.into(), 2 * RUN as usize);
        for i in 0..RUN {
            source.add_point(2 * i, 0.25);
        }
        for i in 0..RUN {
            source.add_point(2 * i + 1, 0.75);
        }
        let queue = ParamValueQueueImpl::with_room(0, 2 * RUN as usize);
        let left_out = assert_no_alloc::assert_no_alloc(|| queue.stage().refill(&source));
        assert_eq!(left_out, 0);
        let mut got = Vec::new();
        queue.stage().for_each_point(|p| got.push(p.sample_offset));
        assert_eq!(got, (0..2 * RUN).collect::<Vec<_>>());
    }

    /// Two points at the same offset keep insertion order — the spec's "jump"
    /// idiom transmits old-value-then-new-value at one position, and swapping
    /// them would inverting the jump.
    #[test]
    fn refill_from_queue_is_stable_for_equal_offsets() {
        let mut source = ParameterQueue::with_capacity(7u32.into(), 2);
        source.add_point(64, 0.1); // old value
        source.add_point(64, 0.9); // new value at the same instant

        let queue = ParamValueQueueImpl::with_room(0, MIN_POINT_ROOM);
        queue.stage().refill(&source);

        let mut got = Vec::new();
        queue.stage().for_each_point(|p| got.push(p.value.get()));
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
        let mut source = ParameterQueue::with_capacity(7u32.into(), 16);
        for i in (0..16i32).rev() {
            source.add_point(i * 8, f64::from(i) * 0.01);
        }

        let queue = ParamValueQueueImpl::with_room(0, MIN_POINT_ROOM);
        queue.stage().refill(&source); // warm up capacity

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..1_000 {
                queue.stage().refill(&source);
            }
        });
        assert_eq!(queue.stage().len(), 16);
    }

    /// RT regression: `refill_from_queue` must not allocate when the
    /// queue's reserved room ([`MIN_POINT_ROOM`]) is sufficient. Covers the hot
    /// automation path where a DAW streams per-buffer points.
    #[test]
    fn refill_from_queue_is_allocation_free() {
        let queue = ParamValueQueueImpl::with_room(0, MIN_POINT_ROOM);
        let source = queue_with_points(8);

        // Warm up.
        queue.stage().refill(&source);

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..10_000 {
                queue.stage().refill(&source);
            }
        });
        assert_eq!(queue.stage().len(), 8);
    }

    /// **A queue holds the room it was built with and never grows**: one
    /// built for 32 points takes 32 without allocating, block after block,
    /// and a 33rd keeps the queue's latest value, the point left out
    /// returned for counting, still without allocating.
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
                assert_eq!(queue.stage().refill(&big), 0);
            }
            queue.stage().refill(&over)
        });
        assert_eq!(left_out, 1);
        assert_eq!(queue.stage().len(), 32);
        assert_eq!(queue.stage().capacity(), 32);
        let mut last = None;
        queue
            .stage()
            .for_each_point(|p| last = Some(p.sample_offset));
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
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
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
        assert_eq!(queue.stage().len(), MIN_POINT_ROOM);
        let mut last = None;
        queue
            .stage()
            .for_each_point(|p| last = Some((p.sample_offset, p.value.get())));
        assert_eq!(last, Some((100, 0.75)));
    }
}
